#![allow(dead_code, unused_imports, unused_variables)]

//use pure rust implementation instead of opencv
use screenshots::Screen;
use image::imageops;
use average_color;
use serde::Deserialize;
use anyhow::Result;

/// rectangular zone on screen to sample color from
#[derive(Deserialize)]
pub struct ZoneConfig {
    //Add multiple monitor support here???
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// This is a color sample from the screen. Its separate from ColorCommand because it implements differs_from and both could have their own unique functions in the future.
pub struct ZoneSample { pub r: u8, pub g: u8, pub b: u8}

impl ZoneSample {
    /// constructor for new ZoneSample
    pub fn new (r: u8, g: u8, b: u8) -> Self {
        ZoneSample{ r, g, b }
    }

    ///this function checks if any color channel exceeds a given threshold
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


pub struct ZoneGrabber {zone: ZoneConfig}

impl ZoneGrabber {
    pub fn new (zone: ZoneConfig) -> Self {
        ZoneGrabber { zone }
    }

    /// captures average rgb values for a zone from downsampled crop
    pub fn sample (&self, downsample: u8) -> Result<ZoneSample> {
        //This function will handle screen shot, cropping to the zone, downsampling for mean calculation, and then mean calculation all in one.
        todo!("build out sampling function")
    }

}
