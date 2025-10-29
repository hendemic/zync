#![allow(dead_code, unused_imports, unused_variables)]
use std::time::Duration;
use std::thread;
use serde::Deserialize;
use anyhow::{Result, Context};

use crate::capture::{ZoneSampler, ZoneColor};
use crate::lights::{MessageColor, LightController};

#[derive(Deserialize)]
pub struct PerformanceConfig {
    target_fps: u8,
    min_fps: u8,
    refresh_threshold: u8,
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
    consecutive_failures: u16,
    consecutive_successes: u16,
}

impl AdaptiveRate {
    pub fn new (target_interval: u64, current_interval: u64) -> Self {
        AdaptiveRate {target_interval, current_interval, consecutive_failures: 0, consecutive_successes: 0}
    }

    pub fn restore_framerate(&mut self) {
        todo!("build out framerate increase algorithm")
    }

    pub fn throttle_framerate(&mut self) {
        todo!("build out framerate decrease algorithm")
    }

    pub fn get_interval(&self) -> u64 {
        self.current_interval
    }

    pub fn get_target(&self) -> u64 {
        self.target_interval
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
        // start loop
        loop {
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
                let transition: f32 = self.rate.current_interval as f32 / 1000.0;


                match area.zone_light.set_light(color, Some(transition)){
                    Ok(_) => self.rate.restore_framerate(),
                    Err(e) => {
                        eprintln!("Message delivery failed: {}", e);
                        self.rate.throttle_framerate();
                    }
                }
                area.previous_sample = Some(sample);
            }
            thread::sleep(Duration::from_millis(self.rate.current_interval));
        }
    }
}
