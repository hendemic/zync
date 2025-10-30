#![allow(dead_code, unused_imports, unused_variables)]
use image::imageops::*;
use image::DynamicImage;
use serde::Deserialize;
use anyhow::Result;
use xcap::*;
use std::time::{Duration, Instant};

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

    pub fn get_light_name(&self) -> String {
        self.config.light_name.clone()
    }

    /// Captures average rgb values for a zone. Uses downsampling for larger zones.
    pub fn sample (&self, downsample: u8) -> Result<ZoneColor> {

        //let time1 = Instant::now();

        // Takes 20-25ms! There's really no way around this without getting more sophisticated as we're just using a screenshot.
        // TODO: Make a separate capture function that captures the whole screen and passes the full image into ZoneSamplers.
        // If we have 2-3 Zones this can hurt performance a lot!
        let mut full_image = self.monitor.capture_image()?;
        //println!("Capture time: {}ms", time1.elapsed().as_millis());


        let time2 = Instant::now();
        let snippet = image::imageops::crop(
            &mut full_image,
            self.config.x,
            self.config.y,
            self.config.width,
            self.config.height,
        ).to_image();

        //println!("Crop time: {}ms", time2.elapsed().as_nanos());


        //let time3 = Instant::now();


        // Downsample image unless its smaller than 100x100.
        // 90-600 micro seconds depending on downsample factor
        // TODO: Apply downsampling in averaging calculation by skipping pixels. Should save time.
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
        //println!("Resize time: {}micro sec", time3.elapsed().as_micros());

        //let time4 = Instant::now();

        // Calculate average
        // This is calculation isn't ideal. Don't need to average every single pixel.
        // This works for a POC, and confirmed it is the shortest operation at 90 nanos at 100 downsample, 1.4 micro sec at 20, 5.5 micro sec at 10
        // TODO: Look into iterators in Rust book. If I can sample every Nth pixel, I can save time here and in downsampling/resizing.
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

        //println!("Averaging time: {} nano secs", time4.elapsed().as_nanos());
        //println!("Total image capture and process time: {}ms", time1.elapsed().as_millis());

        Ok(ZoneColor {
            r: (r_sum / count) as u8,
            g: (g_sum / count) as u8,
            b: (b_sum / count) as u8,
        })
    }
}
