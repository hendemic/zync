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

//This is a color sample from the screen. Its separate from ColorCommand because it implements differs_from, which takes ownership of a past ZoneSample
impl ZoneSample {
    pub fn new (r: u8, g: u8, b: u8) -> Self {
        ZoneSample{ r, g, b }
    }
    pub fn differs_from (&self, other: &ZoneSample, threshold: u8) -> bool {
        todo!("build out calculation ")
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
        todo!("build out sampling ")
    }

}
