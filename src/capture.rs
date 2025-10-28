#![allow(dead_code)]

//use pure rust implementation instead of opencv
use screenshots::Screen;
use image::imageops;
use average_color;
use serde::Deserialize;
use anyhow::Result;

#[derive(Deserialize)]
pub struct ZoneConfig {
    x1: u32,
    y1: u32,
    width: u32,
    height: u32,
}

pub struct ZoneSample { pub r: u8, pub g: u8, pub b: u8}

//This is a color sample from the screen. Its separate from ColorCommand because it implements differs_from and both could have their own unique functions in the future.
impl ZoneSample {
    pub fn new (r: u8, g: u8, b: u8) -> Self {
        ZoneSample{ r, g, b }
    }

    //this function checks if any color channel exceeds a given threshold
    //implemented in the sync module to determine if a message is required or not in order to reduce MQTT message overhead.
    pub fn differs_from (&self, other: &ZoneSample, threshold: u8) -> bool {
        let diffs = (
            self.r.abs_diff(other.r),
            self.g.abs_diff(other.g),
            self.b.abs_diff(other.b)
        );
        let max_diff = diffs.0.max(diffs.1).max(diffs.2);

        max_diff > threshold
    }
}

//ZoneGrabber is the executor object that is used to sample within a zone and return a zone sample.
pub struct ZoneGrabber {zone: ZoneConfig}

impl ZoneGrabber {
    pub fn new (zone: ZoneConfig) -> Self {
        ZoneGrabber { zone }
    }

    pub fn sample (&self, downsample: u8) -> Result<ZoneSample> {
        //This function will handle screen shot, cropping to the zone, downsampling for mean calculation, and then mean calculation all in one.
        todo!("build out sampling function")
    }

}
