use std::time::{Duration, Instant};
use std::thread;
use serde::Deserialize;
use anyhow::{Result};
use chrono::Local;

use crate::capture::{ScreenCapture, ZoneColor, ZoneSampler};
use crate::lights::{MessageColor, LightController};

const FRAME_RECOVERY_RATE: f32 = 0.2;
const FRAME_RECOVERY_BUFFER: u16 = 5;
const FRAME_THROTTLE_RATE: u64 = 10;
const TRANSITION_SOFTNESS: f32 = 0.3;
const TRANSITION_MIN: f32 = 0.02;
const TRANSITION_MAX: f32 = 1.0;

#[derive(Deserialize)]
pub struct PerformanceConfig {
    pub max_fps: u64,
    pub max_delay: u64,
    pub refresh_threshold: u8,
    pub percent_thread_work: f32,
    pub fps_reporting: u64,
}

/// This is handles a zone and its cooresponding lights. Defined here to maintain independence between light and capture modules.
pub struct ZonePair<'a>{
    zone: ZoneSampler,
    zone_light: LightController<'a>,
    previous_sample: Option<ZoneColor>,
}

impl<'a> ZonePair<'a> {
    pub fn new (zone: ZoneSampler, zone_light: LightController<'a>, previous_sample: Option<ZoneColor>) -> Self {
        ZonePair {zone, zone_light, previous_sample}
    }
}

///Adaptive rate struct controls framerate in the event of message bounces.
pub struct AdaptiveRate {
    target_interval: u64,
    current_interval: u64,
    max_interval: u64,
    consecutive_failures: u16,
    consecutive_successes: u16,
    percent_thread_work: f32,
}

impl AdaptiveRate {
    #[allow(dead_code)] // not using new. keeping for testing.
    pub fn new (target_interval: u64, current_interval: u64, max_interval: u64, percent_thread_work: f32) -> Self {
        AdaptiveRate {
            target_interval,
            current_interval,
            max_interval,
            consecutive_failures: 0,
            consecutive_successes: 0,
            percent_thread_work,
        }
    }
    pub fn new_from_fps (fps: u64, max_interval: u64, percent_thread_work: f32) -> Self {
        AdaptiveRate {
            target_interval: 1000 / fps,
            current_interval: 1000 / fps,
            max_interval,
            consecutive_failures: 0,
            consecutive_successes: 0,
            percent_thread_work,
        }
    }
    /// After successful messages, this function increases the framerate back toward its target.
    /// It waits for a number of successful messages. Once its successfully sent enough messages,
    /// we can start increasing the message rate. This aims to be a simple AIMD-like network ping
    /// balances with a users perception of framerate/light changes to not abruptly slow/start.
    fn restore_framerate(&mut self) {
        let delta: i64 = self.current_interval as i64 - self.target_interval as i64;

        //if delta is negative or very close to the target, just set it to the target
        if delta <= 10 {
            self.current_interval = self.target_interval;
        }

        //otherwise, decreate current_interval by 20% of the delta
        else if self.consecutive_successes > FRAME_RECOVERY_BUFFER {
            self.current_interval = self.current_interval - (FRAME_RECOVERY_RATE * delta as f32) as u64;
        }

        self.consecutive_successes += 1;
        self.consecutive_failures = 0;

        // println!("{}\tFramerate restored:\tNew interval: {:>4}ms  Consecutive successes: {:>2}",
        //     Local::now().format("%H:%M:%S%.3f"),
        //     self.current_interval,
        //     self.consecutive_successes,
        // );

    }
    /// Refresh rate is dropped by 20ms each failure. If we've failed 10 times it sets it to the
    /// max thats configured in order to wait for the mesh to recover.
    fn throttle_framerate(&mut self) {
        if self.current_interval < self.max_interval {
            self.current_interval = match self.consecutive_failures {
                f if f < 10 => self.current_interval + FRAME_THROTTLE_RATE,
                _ => self.max_interval,
            };
        }
        else {
            self.current_interval = self.max_interval;
        }

        self.consecutive_successes = 0;
        self.consecutive_failures += 1;

        // println!("{}\tFramerate throttled:\tNew interval: {:>4}ms  Consecutive failures: {:>3}",
        //     Local::now().format("%H:%M:%S%.3f"),
        //     self.current_interval,
        //     self.consecutive_failures,
        // );

    }
    fn adjust_timing(&mut self, work_time: u64) -> u64 {

        // setting threshold for throttling to 1/3 the target rate to avoid high CPU usage
        // My theory is that this ratio of work time to interval time is what determines CPU usage
        // (aka CPU % thread usage is proportional to work_time/target_interval. Need to test this.)
        let throttle_threshold = (self.current_interval as f32 * self.percent_thread_work) as u64;

        if work_time > throttle_threshold {
            self.throttle_framerate();
        } else {
            self.restore_framerate();
        }

        let adjust: i64 = self.current_interval as i64 - work_time as i64;

        match adjust {
            a if a > 0 => (self.current_interval - work_time) as u64,
            _ => {
                println!("Caution: High latency. Adjust zone, lights, downsample, and/or Max FPS. Work latency: {}", work_time);
                0
            }
        }
    }

}

pub struct SyncEngine<'a> {
    screen: Box<dyn ScreenCapture>,
    zones: Vec<ZonePair<'a>>,
    rate: AdaptiveRate,
    config: PerformanceConfig,
    downsample: u8,
    interval_samples: Vec<u64>,
    last_report_time: Instant,
}

impl<'a> SyncEngine<'a> {
    pub fn new(screen: Box<dyn ScreenCapture>, zones: Vec<ZonePair<'a>>, rate: AdaptiveRate, config: PerformanceConfig, downsample: u8) -> Self {
        SyncEngine {
            screen,
            zones,
            rate,
            config,
            downsample,
            interval_samples: Vec::new(),
            last_report_time: Instant::now(), //defining on creation as default value. updates when Run() starts.
        }
    }

    pub fn calculate_transition(sample: &ZoneColor, previous: &ZoneColor) -> f32 {
        let distance = sample.compare_sample(previous);
        let norm_distance = (distance / 441.0).min(1.0);

        TRANSITION_MAX - norm_distance.powf(TRANSITION_SOFTNESS) * (TRANSITION_MAX - TRANSITION_MIN)

    }

    pub fn send_fps_message(&mut self) -> () {
        self.interval_samples.push(self.rate.current_interval);

        if self.last_report_time.elapsed().as_secs() >= self.config.fps_reporting {
            let avg = (self.interval_samples.iter().sum::<u64>() / self.interval_samples.len() as u64) as f32;
            println!("{}\tAvg fps: {:>5.2}",
                Local::now().format("%H:%M:%S"),
                1000 as f32 / avg,
            );
            self.interval_samples.clear();
            self.last_report_time = Instant::now();
        }
    }

    pub fn run(&mut self) -> Result<()>{
        self.last_report_time = Instant::now();

        loop {
            let now = Instant::now();
            let frame = self.screen.capture_frame()?;

            for area in &mut self.zones {

                // grab screen
                let sample = area.zone.sample(&frame, self.downsample)?;

                //check if we have a don't previous sample or if its meaningfully different to determine if we update the lights
                let update = match &area.previous_sample {
                                None => true,
                                Some(prev) => sample.differs_from(prev, self.config.refresh_threshold),
                            };

                if !update {
                    continue;
                }

                // send light command and handle rate adaption
                let transition = match &area.previous_sample {
                    Some(prev) => SyncEngine::calculate_transition(&sample, prev),
                    None => TRANSITION_MAX,
                };

                let color = MessageColor::from(sample);

                area.zone_light.set_light(color, Some(transition))?;
                area.previous_sample = Some(sample);
            }

            self.send_fps_message();

            let elapsed_time = now.elapsed().as_millis() as u64;
            thread::sleep(Duration::from_millis(self.rate.adjust_timing(elapsed_time)));
        }
    }
}
