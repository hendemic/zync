#![allow(dead_code, unused_imports, unused_variables)]
use std::time::Duration;
use std::thread;


use serde::Deserialize;
use anyhow::{Result, Context};

use crate::capture::ZoneSample;

use crate::lights::ColorCommand;
use crate::{capture::{ZoneConfig, ZoneGrabber}, lights::LightController};

#[derive(Deserialize)]
pub struct PerformanceConfig {
    target_fps: u8,
    min_fps: u8,
    refresh_threshold: u8,
}

pub struct ZonePair<'a>{
    zone: ZoneGrabber,
    zone_light: LightController<'a>,
    previous_sample: Option<ZoneSample>,
}

impl<'a> ZonePair<'a> {
    pub fn new (zone: ZoneGrabber, zone_light: LightController<'a>, previous_sample: Option<ZoneSample>) -> Self {
        ZonePair {zone, zone_light, previous_sample}
    }
}

pub struct AdaptiveRate {
    target_interval: u64,
    current_interval: u64, //note: use for transition input
    consecutive_failures: u16,
    consecutive_successes: u16,
}

impl AdaptiveRate {
    pub fn new (target_interval: u64, current_interval: u64) -> Self {
        AdaptiveRate {target_interval, current_interval, consecutive_failures: 0, consecutive_successes: 0}
    }

    pub fn unthrottle(&mut self) {
        todo!("build out framerate increase algorithm")
    }

    pub fn throttle(&mut self) {
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
                let color = ColorCommand::from(sample);
                let transition: f32 = self.rate.current_interval as f32 / 1000.0;


                match area.zone_light.set_light(color, Some(transition)){
                    Ok(_) => self.rate.unthrottle(),
                    Err(e) => {
                        eprintln!("Failed to update light: {}", e);
                        self.rate.throttle();
                    }
                }

                area.previous_sample = Some(sample);
            }
            thread::sleep(Duration::from_millis(self.rate.current_interval));
        }
    }
}
