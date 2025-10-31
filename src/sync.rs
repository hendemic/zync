use std::time::{Duration, Instant};
use std::thread;
use serde::Deserialize;
use anyhow::{Result};

use crate::capture::{ZoneSampler, ZoneColor};
use crate::lights::{MessageColor, LightController};

const FRAME_RECOVERY_RATE: f32 = 0.5;
const FRAME_RECOVERY_BUFFER: u16 = 2;
const FRAME_THROTTLE_RATE: u64 = 20;
const TRANSITION_SOFTNESS: f32 = 3.0;

#[derive(Deserialize)]
pub struct PerformanceConfig {
    pub target_fps: u64,
    pub max_delay: u64,
    pub refresh_threshold: u8,
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
}

impl AdaptiveRate {
    #[allow(dead_code)] // not using new. keeping for testing.
    pub fn new (target_interval: u64, current_interval: u64, max_interval: u64) -> Self {
        AdaptiveRate {
            target_interval,
            current_interval,
            max_interval,
            consecutive_failures: 0,
            consecutive_successes: 0,
        }
    }
    pub fn new_from_fps (fps: u64, max_interval: u64) -> Self {
        AdaptiveRate {
            target_interval: 1000 / fps,
            current_interval: 1000 / fps,
            max_interval,
            consecutive_failures: 0,
            consecutive_successes: 0}
    }
    /// After successful messages, this function increases the framerate back toward its target.
    /// It waits for a number of successful messages. Once its successfully sent enough messages,
    /// we can start increasing the message rate. This aims to be a simple AIMD-like network ping
    /// balances with a users perception of framerate/light changes to not abruptly slow/start.
    fn restore_framerate(&mut self) {
        let delta: i64 = self.current_interval as i64 - self.target_interval as i64;

        //if delta is negative or very close to the target, just set it to the target
        if delta <= 20 {
            self.current_interval = self.target_interval;
        }

        //otherwise, decreate current_interval by 20% of the delta
        else if self.consecutive_successes > FRAME_RECOVERY_BUFFER {
            self.current_interval = self.current_interval - (FRAME_RECOVERY_RATE * delta as f32) as u64;
        }

        self.consecutive_successes += 1;
        self.consecutive_failures = 0;
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
    }
    fn adjust_timing(&self, work_time: u64) -> u64 {
        let adjust: i64 = self.current_interval as i64 - work_time as i64;

        if adjust > 0 {
            (self.current_interval - work_time) as u64
        } else {
            println!("Caution: High latency. Adjust zone, lights, downsample, and/or FPS.");
            0
        }
    }

}

pub struct SyncEngine<'a> {
    zones: Vec<ZonePair<'a>>,
    rate: AdaptiveRate,
    config: PerformanceConfig,
    downsample: u8,
}

impl<'a> SyncEngine<'a> {
    pub fn new(zones: Vec<ZonePair<'a>>, rate: AdaptiveRate, config: PerformanceConfig, downsample: u8) -> Self {
        SyncEngine {zones, rate, config, downsample}
    }

    pub fn run(&mut self) -> Result<()>{
        loop {
            let now = Instant::now();
            for area in &mut self.zones {

                // grab screen
                let sample = area.zone.sample(self.downsample)?;

                //check if we have a don't previous sample or if its meaningfully different to determine if we update the lights
                let update = match &area.previous_sample {
                                None => true,
                                Some(prev) => sample.differs_from(prev, self.config.refresh_threshold),
                            };

                if !update {
                    continue;
                }

                // send light command and handle rate adaption
                let color = MessageColor::from(sample);
                let transition: f32 = self.rate.current_interval as f32 / 1000.0 * TRANSITION_SOFTNESS;


                match area.zone_light.set_light(color, Some(transition)){
                    Ok(_) => self.rate.restore_framerate(),
                    Err(e) => {
                        println!("Message delivery failed: {}", e);
                        self.rate.throttle_framerate();
                    }
                }
                area.previous_sample = Some(sample);
            }
            let elapsed_time = now.elapsed().as_millis() as u64;
            thread::sleep(Duration::from_millis(self.rate.adjust_timing(elapsed_time)));
        }
    }
}
