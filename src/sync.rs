#![allow(dead_code, unused_imports, unused_variables)]
use serde::Deserialize;


use crate::{capture::ZoneConfig, lights::LightController};

#[derive(Deserialize)]
pub struct PerformanceConfig {
    target_fps: u8,
    min_fps: u8,
    refresh_threshold: u8,
}

pub struct ZoneMap<'a>{
    name: String,
    zone: ZoneConfig,
    zone_light: LightController<'a>,
}

impl<'a> ZoneMap<'a> {
    pub fn new (name: String, zone: ZoneConfig, zone_light: LightController<'a>) -> Self {
        ZoneMap {name, zone, zone_light}
    }
}

pub struct AdaptiveRate {
    target_interval: u16,
    current_interval: u16, //note: use for transition input
    consecutive_failures: u16,
    consecutive_successes: u16,
}

impl AdaptiveRate {
    pub fn new (target_interval: u16, current_interval: u16) -> Self {
        AdaptiveRate {target_interval, current_interval, consecutive_failures: 0, consecutive_successes: 0}
    }

    pub fn on_success(&mut self) {
        todo!("build out framerate increase algorithm")
    }

    pub fn on_failure(&mut self) {
        todo!("build out framerate decrease algorithm")
    }

    pub fn get_interval(&self) -> u16 {
        self.current_interval
    }

    pub fn get_target_interval(&self) -> u16 {
        self.target_interval
    }

}

pub struct SyncEngine<'a> {
    zones: Vec<ZoneMap<'a>>,
    rate: AdaptiveRate,
    config: PerformanceConfig,
}

impl<'a> SyncEngine<'a> {
    pub fn new(zones: Vec<ZoneMap<'a>>, rate: AdaptiveRate, config: PerformanceConfig) -> Self {
        SyncEngine {zones, rate, config}
    }

    pub fn run(&mut self) {
        todo!("build out sync engine")
    }
}
