//! The application layer: the sync loop, the pacing that keeps it inside what
//! the light network can absorb, and the supervisor that owns a session's
//! lifetime.
//!
//! Depends only on [`crate::domain`] and [`crate::ports`], never on an adapter.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::domain::{
    Config, Frame, LightCommand, LightId, PerformanceConfig, Rgb, TransitionCurve, ZoneSampler,
};
use crate::ports::{FrameSource, LightSink};

const FRAME_RECOVERY_RATE: f32 = 0.2;
const FRAME_RECOVERY_BUFFER: u16 = 5;
const FRAME_THROTTLE_RATE: u64 = 10;

/// How a reported delivery failure cuts a light's rate, and how the rate comes
/// back. Recovery waits for a quiet period first so that a burst of failures
/// cannot be undone in the gaps between its own log lines.
const BUDGET_FAILURE_BACKOFF: f32 = 0.5;
const BUDGET_FLOOR_PER_SEC: f32 = 0.25;
const BUDGET_RECOVERY_DELAY: Duration = Duration::from_secs(3);
/// Fraction of the configured rate regained per quiet second.
const BUDGET_RECOVERY_PER_SEC: f32 = 0.1;

/// Instructions a running session accepts from outside the process.
///
/// One variant today. Pause and Resume land with the Home Assistant switch, and
/// the important property is already established here: they arrive on this
/// channel rather than through a mechanism of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    /// Stop syncing, apply the stop policy to the lights, and exit.
    Shutdown,
}

/// Why a session stopped. An error stopping it propagates instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// A [`ControlCommand::Shutdown`] arrived.
    Requested,
    /// Every control sender was dropped, so nothing can reach us again.
    ControlClosed,
}

/// Token bucket bounding how fast commands reach the light network.
///
/// Nothing downstream of us expires a command: MQTT QoS 0 has no TTL, and
/// Zigbee2MQTT's adapter queue holds requests indefinitely and retries them.
/// Once a command is published it will be delivered, however stale. So the only
/// way to keep the lights current is to never hand the mesh more than it can
/// deliver, and to cut the rate the moment it reports it cannot.
struct CommandBudget {
    tokens: f32,
    capacity: f32,
    max_rate: f32,
    refill_per_sec: f32,
    last_refill: Instant,
    last_failure: Option<Instant>,
}

impl CommandBudget {
    fn new(commands_per_sec: f32) -> Self {
        let rate = commands_per_sec.max(0.1);
        CommandBudget {
            tokens: 1.0,
            // A small burst allowance is enough; anything larger just becomes a
            // backlog inside the light service.
            capacity: rate.clamp(1.0, 2.0),
            max_rate: rate,
            refill_per_sec: rate,
            last_refill: Instant::now(),
            last_failure: None,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f32();
        self.last_refill = now;

        let quiet = self
            .last_failure
            .is_none_or(|at| now.duration_since(at) >= BUDGET_RECOVERY_DELAY);
        if quiet && self.refill_per_sec < self.max_rate {
            self.refill_per_sec = (self.refill_per_sec
                + self.max_rate * BUDGET_RECOVERY_PER_SEC * elapsed)
                .min(self.max_rate);
        }

        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }

    fn has_token(&mut self) -> bool {
        self.refill();
        self.tokens >= 1.0
    }

    /// Whether `n` commands could all be taken right now. Used to admit a group
    /// of same-frame zone updates as a unit rather than one token at a time.
    fn has_tokens(&mut self, n: u32) -> bool {
        self.refill();
        self.tokens >= n as f32
    }

    /// Caller must have checked [`Self::has_token`].
    fn take(&mut self) {
        self.tokens -= 1.0;
    }

    fn on_failure(&mut self) {
        self.refill_per_sec =
            (self.refill_per_sec * BUDGET_FAILURE_BACKOFF).max(BUDGET_FLOOR_PER_SEC);
        // Drop any saved-up burst too; the mesh has just said it is full.
        self.tokens = self.tokens.min(1.0);
        self.last_failure = Some(Instant::now());
    }

    fn rate(&self) -> f32 {
        self.refill_per_sec
    }
}

/// Paces the capture loop on CPU work time only. Mesh health is handled by the
/// per-light budgets; slowing the loop never reduced the command rate anyway.
pub struct AdaptiveRate {
    target_interval: u64,
    current_interval: u64,
    max_interval: u64,
    consecutive_failures: u16,
    consecutive_successes: u16,
    percent_thread_work: f32,
}

impl AdaptiveRate {
    pub fn new(
        target_interval: u64,
        max_interval: u64,
        percent_thread_work: f32,
    ) -> Self {
        AdaptiveRate {
            target_interval,
            current_interval: target_interval,
            max_interval,
            consecutive_failures: 0,
            consecutive_successes: 0,
            percent_thread_work,
        }
    }

    pub fn from_fps(fps: u64, max_interval: u64, percent_thread_work: f32) -> Self {
        Self::new(1000 / fps.max(1), max_interval, percent_thread_work)
    }

    /// Walks the frame rate back toward its target after a run of cheap frames.
    /// Recovery is gradual, and waits for a few good frames first, so the visible
    /// rate does not oscillate: an AIMD-like response tuned for what a person
    /// watching the lights will accept.
    fn restore_framerate(&mut self) {
        let delta = self.current_interval as i64 - self.target_interval as i64;

        if delta <= 10 {
            self.current_interval = self.target_interval;
        } else if self.consecutive_successes > FRAME_RECOVERY_BUFFER {
            self.current_interval -= (FRAME_RECOVERY_RATE * delta as f32) as u64;
        }

        self.consecutive_successes += 1;
        self.consecutive_failures = 0;
    }

    /// Backs the interval off a step per expensive frame, jumping straight to the
    /// configured maximum once it is clear the machine cannot keep up.
    fn throttle_framerate(&mut self) {
        self.current_interval = match self.consecutive_failures {
            _ if self.current_interval >= self.max_interval => self.max_interval,
            f if f < 10 => self.current_interval + FRAME_THROTTLE_RATE,
            _ => self.max_interval,
        };

        self.consecutive_successes = 0;
        self.consecutive_failures += 1;
    }

    /// Folds one frame's cost into the rate and returns how long to wait before
    /// the next one.
    fn next_delay(&mut self, work_time: u64) -> Duration {
        // Purely a guard against the loop starving the machine.
        let throttle_threshold = (self.current_interval as f32 * self.percent_thread_work) as u64;

        if work_time > throttle_threshold {
            self.throttle_framerate();
        } else {
            self.restore_framerate();
        }

        match self.current_interval.checked_sub(work_time) {
            Some(remaining) if remaining > 0 => Duration::from_millis(remaining),
            _ => {
                warn!(
                    work_ms = work_time,
                    "high capture latency; adjust zones, downsample, or max_fps"
                );
                Duration::ZERO
            }
        }
    }

    pub fn current_interval(&self) -> u64 {
        self.current_interval
    }
}

/// A zone and the state needed to pace the light that follows it.
struct ZoneState {
    sampler: ZoneSampler,
    light: LightId,
    previous_sample: Option<Rgb>,
    /// Paced per light, because a congested group must not throttle a healthy
    /// device on the same screen.
    budget: CommandBudget,
    failures_seen: u64,
    failures_in_interval: u64,
}

impl ZoneState {
    /// Folds failures reported since the last tick into the budget. One backoff
    /// per tick regardless of count: a saturated mesh logs dozens of lines per
    /// second, and halving once per line would crater the rate instantly.
    fn absorb_failures(&mut self, reported: u64) {
        let new_failures = reported.saturating_sub(self.failures_seen);
        self.failures_seen = reported;

        if new_failures > 0 {
            self.budget.on_failure();
            self.failures_in_interval += new_failures;
        }
    }
}

/// A zone's sampled colour and the command it would send, worked out during the
/// planning pass before any budget is spent.
struct PlannedCommand {
    zone_index: usize,
    sample: Rgb,
    command: LightCommand,
}

/// One pass of capture, sampling, and publishing. Ticked by the [`Supervisor`].
pub struct SyncLoop {
    frames: Box<dyn FrameSource>,
    sink: Box<dyn LightSink>,
    zones: Vec<ZoneState>,
    rate: AdaptiveRate,
    /// Overall ceiling across every light. Per-light budgets do the real pacing.
    budget: CommandBudget,
    performance: PerformanceConfig,
    curve: TransitionCurve,
    downsample: u8,
    interval_samples: Vec<u64>,
    last_report: Instant,
    frames_at_last_report: u64,
    commands_sent: u64,
    commands_deferred: u64,
    max_work_ms: u64,
}

impl SyncLoop {
    /// Wires a configuration onto a frame source and a light sink.
    ///
    /// Fails if the configuration is inconsistent, so a bad config is reported at
    /// startup rather than as a missing zone at run time.
    pub fn new(
        config: &Config,
        frames: Box<dyn FrameSource>,
        sink: Box<dyn LightSink>,
    ) -> Result<Self> {
        config.validate()?;

        let source_size = frames.source_size();
        let rates: HashMap<&LightId, f32> = config
            .lights
            .iter()
            .map(|light| (&light.light_name, light.updates_per_sec()))
            .collect();

        let zones = config
            .zones
            .iter()
            .map(|zone| {
                let light = zone.light_name.clone();
                // validate() has already established every zone has a light.
                let rate = rates.get(&light).copied().unwrap_or(1.0);
                Ok(ZoneState {
                    sampler: ZoneSampler::new(zone.clone(), source_size)?,
                    light,
                    previous_sample: None,
                    budget: CommandBudget::new(rate),
                    failures_seen: 0,
                    failures_in_interval: 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(SyncLoop {
            frames,
            sink,
            zones,
            rate: AdaptiveRate::from_fps(
                config.performance.max_fps,
                config.performance.max_delay,
                config.performance.percent_thread_work,
            ),
            budget: CommandBudget::new(config.performance.max_commands_per_sec),
            performance: config.performance.clone(),
            curve: config.intensity.curve(),
            downsample: config.downsample_factor,
            interval_samples: Vec::new(),
            last_report: Instant::now(),
            frames_at_last_report: 0,
            commands_sent: 0,
            commands_deferred: 0,
            max_work_ms: 0,
        })
    }

    /// Records the lights' current state so it can be handed back on shutdown.
    pub fn begin(&mut self) -> Result<()> {
        let (width, height) = self.frames.source_size();
        info!(
            width,
            height,
            mode = %self.frames.describe(),
            zones = self.zones.len(),
            "capture source ready; zones are configured in these coordinates"
        );

        self.last_report = Instant::now();
        self.sink.snapshot()
    }

    /// Applies the stop policy. Runs even when the session ended by error, so a
    /// crash does not leave the lights stuck on the last frame of a game.
    pub fn end(&mut self) {
        if let Err(e) = self.sink.restore() {
            warn!(error = ?e, "could not restore the lights to their previous state");
        }
    }

    /// Captures one frame, updates whatever changed, and returns how long to wait
    /// before the next one.
    pub fn tick(&mut self) -> Result<Duration> {
        let started = Instant::now();
        let frame = self.frames.next_frame()?;

        let mut planned = Vec::new();
        for (index, zone) in self.zones.iter_mut().enumerate() {
            if let Some(command) = Self::plan_zone(
                zone,
                index,
                &frame,
                self.downsample,
                self.performance.refresh_threshold,
                self.curve,
                self.sink.as_mut(),
            )? {
                planned.push(command);
            }
        }

        Self::commit_planned(
            &mut self.zones,
            planned,
            self.sink.as_mut(),
            &mut self.budget,
            &mut self.commands_sent,
            &mut self.commands_deferred,
        )?;

        self.report();

        let work_ms = started.elapsed().as_millis() as u64;
        self.max_work_ms = self.max_work_ms.max(work_ms);
        Ok(self.rate.next_delay(work_ms))
    }

    /// Samples one zone and works out what it would send, without spending any
    /// budget. Splitting this from the commit means every zone that changed on
    /// this frame can be judged against the budgets together, so a scene change
    /// that moves several zones at once either reaches all of their lights or
    /// none of them, rather than reaching them one at a time.
    ///
    /// Takes its collaborators as arguments rather than reading `self` so the
    /// per-zone borrow does not conflict with the shared sink.
    #[allow(clippy::too_many_arguments)]
    fn plan_zone(
        zone: &mut ZoneState,
        zone_index: usize,
        frame: &Frame,
        downsample: u8,
        refresh_threshold: u8,
        curve: TransitionCurve,
        sink: &mut dyn LightSink,
    ) -> Result<Option<PlannedCommand>> {
        let sample = zone.sampler.sample(frame, downsample)?;

        let changed = zone
            .previous_sample
            .is_none_or(|previous| sample.differs_from(&previous, refresh_threshold));
        if !changed {
            return Ok(None);
        }

        let transition = zone
            .previous_sample
            .map_or(curve.max_transition, |previous| curve.transition(&previous, &sample));
        let command = LightCommand::from_sample(sample, transition);

        // A change visible in the sample can still round to the command the light
        // already holds. Record it and move on rather than treat it as pending.
        if !sink.would_send(&zone.light, command) {
            zone.previous_sample = Some(sample);
            return Ok(None);
        }

        zone.absorb_failures(sink.failures(&zone.light));

        Ok(Some(PlannedCommand { zone_index, sample, command }))
    }

    /// Admits a frame's worth of planned zone commands against the budgets and
    /// sends whatever is admitted.
    ///
    /// A zone whose own light budget is empty is deferred on its own — a single
    /// congested or failure-backed-off light must not indefinitely hold back
    /// zones sharing nothing but the same frame. Among the zones that do have a
    /// per-light token, the group is all-or-nothing against the shared global
    /// budget: either every one of them fits, and all go out together, or none
    /// of them do, and all stay pending for the next frame. Out of budget
    /// deliberately leaves `previous_sample` untouched so the change stays
    /// pending and goes out as soon as the mesh has headroom, rather than being
    /// silently dropped.
    fn commit_planned(
        zones: &mut [ZoneState],
        planned: Vec<PlannedCommand>,
        sink: &mut dyn LightSink,
        global_budget: &mut CommandBudget,
        sent: &mut u64,
        deferred: &mut u64,
    ) -> Result<()> {
        let mut ready = Vec::with_capacity(planned.len());
        for command in planned {
            if zones[command.zone_index].budget.has_token() {
                ready.push(command);
            } else {
                *deferred += 1;
            }
        }

        if ready.is_empty() {
            return Ok(());
        }

        if !global_budget.has_tokens(ready.len() as u32) {
            *deferred += ready.len() as u64;
            return Ok(());
        }

        for planned in ready {
            let zone = &mut zones[planned.zone_index];
            if sink.send(&zone.light, planned.command)? {
                zone.budget.take();
                global_budget.take();
                *sent += 1;
            }
            zone.previous_sample = Some(planned.sample);
        }

        Ok(())
    }

    /// Periodic frame rate line, plus diagnostics at debug level.
    ///
    /// A stalled capture stream and a screen that is not changing produce exactly
    /// the same visible result — lights that never move — so the frame counter is
    /// reported alongside what the zones actually saw, and each light's current
    /// pacing so a backed-off budget is visible rather than inferred.
    fn report(&mut self) {
        self.interval_samples.push(self.rate.current_interval());

        if self.last_report.elapsed().as_secs() < self.performance.fps_reporting {
            return;
        }

        // Computed and rounded in f64 so the logged value reads as 12.05 rather
        // than the full binary expansion of an f32.
        let total: u64 = self.interval_samples.iter().sum();
        let average = total as f64 / self.interval_samples.len().max(1) as f64;
        let fps = (100_000.0 / average).round() / 100.0;
        // Debug, not info: once every reporting interval forever, this would
        // otherwise be almost the entire log and drown the events that matter.
        debug!(fps, "capture rate");

        let captured = self.frames.frames_captured();
        for zone in &self.zones {
            debug!(
                zone = zone.sampler.name(),
                light = %zone.light,
                color = ?zone.previous_sample,
                rate_per_sec = zone.budget.rate(),
                failures = zone.failures_in_interval,
                "zone"
            );
        }
        debug!(
            frames = captured.saturating_sub(self.frames_at_last_report),
            sent = self.commands_sent,
            deferred = self.commands_deferred,
            work_max_ms = self.max_work_ms,
            "interval"
        );

        self.frames_at_last_report = captured;
        self.interval_samples.clear();
        self.last_report = Instant::now();
        self.commands_sent = 0;
        self.commands_deferred = 0;
        self.max_work_ms = 0;
        for zone in &mut self.zones {
            zone.failures_in_interval = 0;
        }
    }
}

/// Owns a session and the channel every outside instruction arrives on.
///
/// The CLI, a signal handler, and later the Home Assistant switch are all just
/// senders on that one channel, which is why none of them needs a mechanism of
/// its own.
pub struct Supervisor {
    session: SyncLoop,
    control: Receiver<ControlCommand>,
    /// Held so the channel never reports itself disconnected merely because no
    /// control source happens to be wired up.
    _keepalive: Sender<ControlCommand>,
}

impl Supervisor {
    pub fn new(session: SyncLoop) -> (Self, Sender<ControlCommand>) {
        let (tx, rx) = channel();
        let supervisor = Supervisor {
            session,
            control: rx,
            _keepalive: tx.clone(),
        };

        (supervisor, tx)
    }

    /// Runs until told to stop. The stop policy is applied on the way out whether
    /// the session ended cleanly or by error.
    pub fn run(&mut self) -> Result<StopReason> {
        self.session.begin()?;
        let outcome = self.drive();
        self.session.end();

        if let Ok(reason) = &outcome {
            info!(?reason, "session stopped");
        }
        outcome
    }

    /// Waiting for the next control message doubles as the inter-frame sleep, so
    /// a shutdown is acted on within one frame interval without a second thread.
    fn drive(&mut self) -> Result<StopReason> {
        loop {
            let delay = self.session.tick()?;

            match self.control.recv_timeout(delay) {
                Ok(ControlCommand::Shutdown) => return Ok(StopReason::Requested),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(StopReason::ControlClosed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Intensity, LightService, LightSpec, MqttConfig, StopPolicy, Zone};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Renders a solid colour by default, but the left and right halves can be
    /// set independently so a test can drive two zones through different
    /// samples on the same frame.
    struct FakeFrames {
        left: Mutex<Rgb>,
        right: Mutex<Rgb>,
        served: AtomicU64,
    }

    impl FakeFrames {
        fn new(color: Rgb) -> Self {
            FakeFrames { left: Mutex::new(color), right: Mutex::new(color), served: AtomicU64::new(0) }
        }

        fn set(&self, color: Rgb) {
            *self.left.lock().unwrap() = color;
            *self.right.lock().unwrap() = color;
        }

        fn set_left(&self, color: Rgb) {
            *self.left.lock().unwrap() = color;
        }

        fn set_right(&self, color: Rgb) {
            *self.right.lock().unwrap() = color;
        }
    }

    impl FrameSource for Arc<FakeFrames> {
        fn next_frame(&self) -> Result<Frame> {
            let left = *self.left.lock().unwrap();
            let right = *self.right.lock().unwrap();
            let pixels: Vec<u8> = (0..16u32)
                .flat_map(|_y| {
                    (0..16u32).flat_map(move |x| {
                        let c = if x < 8 { left } else { right };
                        [c.r, c.g, c.b, 255]
                    })
                })
                .collect();
            self.served.fetch_add(1, Ordering::Relaxed);
            Ok(Frame::from_packed_rgba(pixels, 16, 16)?)
        }

        fn source_size(&self) -> (u32, u32) {
            (16, 16)
        }

        fn frames_captured(&self) -> u64 {
            self.served.load(Ordering::Relaxed)
        }

        fn describe(&self) -> String {
            "fake".into()
        }
    }

    #[derive(Default)]
    struct FakeSink {
        sent: Vec<LightCommand>,
        /// Last command accepted per light, so `would_send` can dedupe correctly
        /// once more than one light is in play.
        last_by_light: HashMap<LightId, LightCommand>,
        failures: u64,
        snapshots: u64,
        restores: u64,
    }

    impl LightSink for Arc<Mutex<FakeSink>> {
        fn would_send(&self, light: &LightId, command: LightCommand) -> bool {
            let sink = self.lock().unwrap();
            sink.last_by_light.get(light).is_none_or(|last| {
                last.color != command.color || last.brightness != command.brightness
            })
        }

        fn send(&mut self, light: &LightId, command: LightCommand) -> Result<bool> {
            if !LightSink::would_send(self, light, command) {
                return Ok(false);
            }
            let mut sink = self.lock().unwrap();
            sink.sent.push(command);
            sink.last_by_light.insert(light.clone(), command);
            Ok(true)
        }

        fn failures(&self, _light: &LightId) -> u64 {
            self.lock().unwrap().failures
        }

        fn snapshot(&mut self) -> Result<()> {
            self.lock().unwrap().snapshots += 1;
            Ok(())
        }

        fn restore(&mut self) -> Result<()> {
            self.lock().unwrap().restores += 1;
            Ok(())
        }
    }

    fn config(max_commands_per_sec: f32) -> Config {
        Config {
            mqtt: MqttConfig {
                name: "t".into(),
                broker: "localhost".into(),
                port: 1883,
                user: None,
                password: None,
            },
            lights: vec![LightSpec {
                service: LightService::Zigbee2MQTT,
                light_name: LightId::new("lamp"),
                brightness: 1.0,
                is_group: false,
                max_updates_per_sec: Some(max_commands_per_sec),
                fallback_state: None,
            }],
            zones: vec![Zone {
                name: "all".into(),
                x: 0,
                y: 0,
                width: 16,
                height: 16,
                light_name: LightId::new("lamp"),
            }],
            downsample_factor: 1,
            performance: PerformanceConfig {
                max_fps: 60,
                max_delay: 500,
                refresh_threshold: 10,
                percent_thread_work: 0.25,
                fps_reporting: 3600,
                max_commands_per_sec,
            },
            on_stop: StopPolicy::Restore,
            instance: None,
            intensity: Intensity::Normal,
        }
    }

    /// The fakes are shared through `Arc` so a test can still inspect them after
    /// the loop has taken ownership. Boxed trait objects are `'static`, which is
    /// the whole point of having removed the borrowed handles.
    fn session(
        frames: &Arc<FakeFrames>,
        sink: &Arc<Mutex<FakeSink>>,
        max_commands_per_sec: f32,
    ) -> SyncLoop {
        SyncLoop::new(
            &config(max_commands_per_sec),
            Box::new(Arc::clone(frames)),
            Box::new(Arc::clone(sink)),
        )
        .unwrap()
    }

    /// Two zones, each covering half the frame and mapped to its own light, so
    /// a test can drive them through independent samples with `set_left` /
    /// `set_right` while still exercising one shared global budget.
    fn two_zone_config(max_commands_per_sec: f32, light_a_rate: f32, light_b_rate: f32) -> Config {
        Config {
            mqtt: MqttConfig {
                name: "t".into(),
                broker: "localhost".into(),
                port: 1883,
                user: None,
                password: None,
            },
            lights: vec![
                LightSpec {
                    service: LightService::Zigbee2MQTT,
                    light_name: LightId::new("a"),
                    brightness: 1.0,
                    is_group: false,
                    max_updates_per_sec: Some(light_a_rate),
                    fallback_state: None,
                },
                LightSpec {
                    service: LightService::Zigbee2MQTT,
                    light_name: LightId::new("b"),
                    brightness: 1.0,
                    is_group: false,
                    max_updates_per_sec: Some(light_b_rate),
                    fallback_state: None,
                },
            ],
            zones: vec![
                Zone { name: "left".into(), x: 0, y: 0, width: 8, height: 16, light_name: LightId::new("a") },
                Zone { name: "right".into(), x: 8, y: 0, width: 8, height: 16, light_name: LightId::new("b") },
            ],
            downsample_factor: 1,
            performance: PerformanceConfig {
                max_fps: 60,
                max_delay: 500,
                refresh_threshold: 10,
                percent_thread_work: 0.25,
                fps_reporting: 3600,
                max_commands_per_sec,
            },
            on_stop: StopPolicy::Restore,
            instance: None,
            intensity: Intensity::Normal,
        }
    }

    fn two_zone_session(
        frames: &Arc<FakeFrames>,
        sink: &Arc<Mutex<FakeSink>>,
        max_commands_per_sec: f32,
        light_a_rate: f32,
        light_b_rate: f32,
    ) -> SyncLoop {
        SyncLoop::new(
            &two_zone_config(max_commands_per_sec, light_a_rate, light_b_rate),
            Box::new(Arc::clone(frames)),
            Box::new(Arc::clone(sink)),
        )
        .unwrap()
    }

    #[test]
    fn a_first_frame_always_produces_a_command() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(200, 100, 50)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));

        session(&frames, &sink, 100.0).tick().unwrap();

        assert_eq!(sink.lock().unwrap().sent.len(), 1);
    }

    #[test]
    fn an_unchanged_screen_sends_nothing_further() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(200, 100, 50)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        let mut loop_ = session(&frames, &sink, 100.0);

        for _ in 0..5 {
            loop_.tick().unwrap();
        }

        assert_eq!(sink.lock().unwrap().sent.len(), 1);
    }

    /// The load-bearing half of the pacing contract: an update that arrives with
    /// no budget left is held, not dropped, and goes out on a later tick.
    #[test]
    fn a_deferred_change_is_delivered_once_the_budget_refills() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(200, 100, 50)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        // 100/s is one token every 10ms, so back-to-back ticks cannot both send.
        let mut loop_ = session(&frames, &sink, 100.0);

        loop_.tick().unwrap();
        frames.set(Rgb::new(20, 200, 220));
        loop_.tick().unwrap();
        assert_eq!(sink.lock().unwrap().sent.len(), 1, "the second change is deferred");

        thread::sleep(Duration::from_millis(20));
        loop_.tick().unwrap();

        let sent = &sink.lock().unwrap().sent;
        assert_eq!(sent.len(), 2, "the held change should go out once a token exists");
        assert_eq!(sent[1].color, Rgb::new(20, 200, 220));
    }

    /// The budget must hold a change back rather than drop it, so the light still
    /// catches up once there is headroom.
    #[test]
    fn a_change_over_budget_is_deferred_not_lost() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(0, 0, 0)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        let mut loop_ = session(&frames, &sink, 0.1);

        loop_.tick().unwrap();
        let after_first = sink.lock().unwrap().sent.len();

        for shade in 1..8u8 {
            frames.set(Rgb::new(shade * 30, shade * 30, shade * 30));
            loop_.tick().unwrap();
        }

        assert_eq!(
            sink.lock().unwrap().sent.len(),
            after_first,
            "no further command should fit in the budget"
        );
        assert!(loop_.commands_deferred > 0, "deferrals should be counted");
    }

    /// The whole point of the fix: a scene change that moves two zones on the
    /// same frame must not let one reach its light while the other waits for a
    /// later tick. With only one global token available, neither goes out;
    /// once a second token has accrued, both go out on the very next tick.
    #[test]
    fn two_zones_changing_together_are_sent_together_or_not_at_all() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(200, 100, 50)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        // Capacity is two tokens at this rate, but only one is available up
        // front, so the very first tick already exercises "not enough for the
        // whole group yet".
        let mut loop_ = two_zone_session(&frames, &sink, 100.0, 100.0, 100.0);

        loop_.tick().unwrap();

        assert_eq!(sink.lock().unwrap().sent.len(), 0, "neither zone should send with only one token");
        assert_eq!(loop_.commands_deferred, 2);
        assert!(
            loop_.zones.iter().all(|zone| zone.previous_sample.is_none()),
            "a deferred change must leave previous_sample untouched"
        );

        thread::sleep(Duration::from_millis(15));
        loop_.tick().unwrap();

        assert_eq!(
            sink.lock().unwrap().sent.len(),
            2,
            "both zones should go out together once the group fits in the budget"
        );
    }

    /// Regression guard: once the global budget already holds enough tokens for
    /// the whole group, same-frame changes are not held back waiting for
    /// anything further — they go out together on the first tick that sees
    /// them.
    #[test]
    fn two_zones_changing_together_send_in_one_frame_with_plentiful_budget() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(10, 10, 10)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        let mut loop_ = two_zone_session(&frames, &sink, 100.0, 100.0, 100.0);

        // Let the global budget reach its full two-token capacity before the
        // tick under test.
        thread::sleep(Duration::from_millis(25));
        loop_.tick().unwrap();

        assert_eq!(sink.lock().unwrap().sent.len(), 2, "both zones should send in the same frame");
        assert_eq!(loop_.commands_deferred, 0);
    }

    /// A zone whose own light is throttled hard must not hold back a zone
    /// sharing nothing but the same frame: the starved zone is deferred on its
    /// own, and the healthy zone still goes out. This is the fallback to the
    /// all-or-nothing rule above, chosen so one congested or backed-off light
    /// cannot indefinitely stall every other zone.
    #[test]
    fn a_zone_starved_of_its_own_budget_does_not_hold_back_a_healthy_zone() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(10, 10, 10)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        // Zone A's light is throttled to 1/s; zone B's is not. The global
        // budget is generous so it is never the bottleneck here.
        let mut loop_ = two_zone_session(&frames, &sink, 100.0, 1.0, 100.0);

        // Reach full global capacity, then send the unconditional first sample
        // for both zones together, spending both zones' own tokens too.
        thread::sleep(Duration::from_millis(25));
        loop_.tick().unwrap();
        assert_eq!(sink.lock().unwrap().sent.len(), 2);

        // Give the global budget (100/s) just enough time to recover a token;
        // zone A's own budget (1/s) is still empty over the same span.
        thread::sleep(Duration::from_millis(15));
        frames.set(Rgb::new(220, 30, 30));
        loop_.tick().unwrap();

        assert_eq!(
            sink.lock().unwrap().sent.len(),
            3,
            "zone B's change should go out even though zone A's own budget is empty"
        );
        assert_eq!(loop_.commands_deferred, 1, "zone A is deferred on its own, not as part of the group");
        assert_eq!(
            loop_.zones[0].previous_sample,
            Some(Rgb::new(10, 10, 10)),
            "zone A's change stays pending"
        );
        assert_eq!(loop_.zones[1].previous_sample, Some(Rgb::new(220, 30, 30)));
    }

    #[test]
    fn a_session_snapshots_on_entry_and_restores_on_exit() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(10, 10, 10)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        let mut loop_ = session(&frames, &sink, 100.0);

        loop_.begin().unwrap();
        loop_.end();

        let recorded = sink.lock().unwrap();
        assert_eq!((recorded.snapshots, recorded.restores), (1, 1));
    }

    #[test]
    fn a_zone_naming_an_unconfigured_light_is_rejected_at_startup() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(10, 10, 10)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        let mut config = config(100.0);
        config.zones[0].light_name = LightId::new("nonexistent");

        assert!(
            SyncLoop::new(&config, Box::new(Arc::clone(&frames)), Box::new(Arc::clone(&sink)))
                .is_err()
        );
    }

    #[test]
    fn a_shutdown_command_stops_the_supervisor() {
        let frames = Arc::new(FakeFrames::new(Rgb::new(10, 10, 10)));
        let sink = Arc::new(Mutex::new(FakeSink::default()));
        let (mut supervisor, control) = Supervisor::new(session(&frames, &sink, 100.0));

        control.send(ControlCommand::Shutdown).unwrap();

        assert_eq!(supervisor.run().unwrap(), StopReason::Requested);
        assert_eq!(sink.lock().unwrap().restores, 1, "lights must be restored");
    }

    #[test]
    fn budget_hands_out_a_token_then_makes_the_caller_wait() {
        let mut budget = CommandBudget::new(1.0);

        assert!(budget.has_token());
        budget.take();
        assert!(!budget.has_token());
    }

    #[test]
    fn has_tokens_requires_the_whole_amount_at_once() {
        // Fast refill, so capacity (clamped to 2.0) is reached quickly.
        let mut budget = CommandBudget::new(1000.0);

        assert!(!budget.has_tokens(2), "only the initial single token is available yet");
        assert!(budget.has_tokens(1));

        thread::sleep(Duration::from_millis(5));
        assert!(budget.has_tokens(2), "capacity is two tokens once refilled");
    }

    #[test]
    fn a_reported_failure_halves_the_rate() {
        let mut budget = CommandBudget::new(4.0);
        let before = budget.rate();

        budget.on_failure();

        assert!(budget.rate() < before);
        assert!(budget.rate() >= BUDGET_FLOOR_PER_SEC);
    }

    #[test]
    fn repeated_failures_stop_at_the_floor() {
        let mut budget = CommandBudget::new(4.0);

        for _ in 0..50 {
            budget.on_failure();
        }

        assert_eq!(budget.rate(), BUDGET_FLOOR_PER_SEC);
    }

    #[test]
    fn cheap_frames_hold_the_target_interval() {
        let mut rate = AdaptiveRate::from_fps(10, 500, 0.25);

        let delay = rate.next_delay(1);

        assert_eq!(rate.current_interval(), 100);
        assert_eq!(delay, Duration::from_millis(99));
    }

    #[test]
    fn expensive_frames_back_the_interval_off() {
        let mut rate = AdaptiveRate::from_fps(10, 500, 0.25);

        rate.next_delay(90);

        assert!(rate.current_interval() > 100);
    }

    #[test]
    fn sustained_overload_settles_at_the_configured_maximum() {
        let mut rate = AdaptiveRate::from_fps(10, 500, 0.25);

        for _ in 0..20 {
            rate.next_delay(400);
        }

        assert_eq!(rate.current_interval(), 500);
    }

    #[test]
    fn work_longer_than_the_interval_asks_for_no_wait() {
        let mut rate = AdaptiveRate::new(100, 500, 0.25);

        assert_eq!(rate.next_delay(600), Duration::ZERO);
    }
}
