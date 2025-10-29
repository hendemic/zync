#![allow(dead_code, unused_imports, unused_variables)]

use screenshots::Screen;
use image::imageops::*;
use image::DynamicImage;
use serde::Deserialize;
use anyhow::Result;
use xcap::Monitor;

/// rectangular zone on screen to sample color from
#[derive(Deserialize)]
pub struct ZoneConfig {
    //Add multiple monitor support here in future versions. Configure zone in yaml. For now we're just using the primary monitor by default, but zones should be configurable by user by monitor.
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    light_name: String,
}

/// This is a color sample from the screen. Its separate from ColorCommand because it implements differs_from and both could have their own unique functions in the future.
#[derive(Clone, Copy)]
pub struct ZoneColor { pub r: u8, pub g: u8, pub b: u8}

impl ZoneColor {
    /// constructor for new ZoneColor
    pub fn new (r: u8, g: u8, b: u8) -> Self {
        ZoneColor{ r, g, b }
    }

    ///this function checks if any color channel exceeds a given threshold
    pub fn differs_from (&self, other: &ZoneColor, threshold: u8) -> bool {
        let diffs = (
            self.r.abs_diff(other.r),
            self.g.abs_diff(other.g),
            self.b.abs_diff(other.b)
        );

        let max_diff = diffs.0.max(diffs.1).max(diffs.2);

        max_diff > threshold
    }
}

///Used to sample a region on a monitor
pub struct ZoneSampler {
    config: ZoneConfig,
    monitor: Monitor,
}

impl ZoneSampler {
    pub fn new (config: ZoneConfig) -> Result<Self> {
        // this simple version automatically grabs the primary display. Future updates will allow configuration of monitor in the .yaml file, and this constructor will be a simple pass through of fields from ZoneConfig. Lifted some of this code from screenshots crate example.
        let monitors = Monitor::all()?;

        let monitor = monitors
            .into_iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .expect("No primary monitor found");

        Ok(ZoneSampler {config, monitor})
    }

    /// Captures average rgb values for a zone. Uses downsampling for larger zones.
    pub fn sample (&self, downsample: u8) -> Result<ZoneColor> {

        let snippet = self.monitor.capture_region(
            self.config.x,
            self.config.y,
            self.config.width,
            self.config.height,
        )?;

        // Downsample image unless its smaller than 100x100.
        // This is a starting point for future optimization on cut off for downsampling vs using snippet directly.
        let snippet = if (snippet.width() * snippet.height()) > 10000 {
            image::imageops::resize(
                &snippet,
                snippet.width() / (downsample as u32),
                snippet.height() / (downsample as u32),
                FilterType::Nearest,
            )
        } else {
            snippet
        };

        //Calculate average
        let mut r_sum = 0u64;
        let mut g_sum = 0u64;
        let mut b_sum = 0u64;
        let mut count = 0u64;

        for pixel in snippet.pixels() {
            r_sum += pixel[0] as u64;
            g_sum += pixel[1] as u64;
            b_sum += pixel[2] as u64;
            count += 1;
        }

        Ok(ZoneColor {
            r: (r_sum / count) as u8,
            g: (g_sum / count) as u8,
            b: (b_sum / count) as u8,
        })
    }
}
