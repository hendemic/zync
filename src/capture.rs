use image::RgbaImage;
use serde::Deserialize;
use anyhow::{Result, bail};
use xcap::*;
//use std::time::{Duration, Instant};


/// Captures screen across platforms
pub trait ScreenCapture {
    fn new(monitor: Monitor) -> Result<Box<dyn ScreenCapture>> where Self: Sized;
    fn capture_frame(&self) -> Result<RgbaImage>;

}

//Structs for X11, Wayland, and in the future MacOS and Windows.
pub struct X11Capturer { monitor: Monitor }
pub struct WaylandCapturer {monitor: Monitor } //add pipewire settings


impl ScreenCapture for X11Capturer {
    fn new(monitor: Monitor) -> Result<Box<dyn ScreenCapture>> {
        Ok(Box::new(X11Capturer {monitor}))
    }

    fn capture_frame(&self) -> Result<RgbaImage> {
        let image = self.monitor.capture_image()?;
        Ok(image)
    }
}

impl ScreenCapture for WaylandCapturer {
    fn new(monitor: Monitor) -> Result<Box<dyn ScreenCapture>> {
        Ok(Box::new(WaylandCapturer {monitor}))

        // for wayland, new() will use internal functions to create pipewire connection,
        // and start stream in thread
    }

    ///placeholder for now - will take latest frame from buffer
    fn capture_frame(&self) -> Result<RgbaImage> {
        let image = self.monitor.capture_image()?;
        Ok(image)
    }
}

/// Constructor for new ScreenCapture based on platform
pub fn new_screen() -> Result<Box<dyn ScreenCapture>> {
    let monitors = Monitor::all()?;

    let monitor = monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .expect("No primary monitor found");

    #[cfg(target_os = "linux")]
    {
        if std::env::var("WAYLAND_DISPLAY").is_ok() {  //TODO need to check if there are other checks to make sure I accurately detect wayland
            bail!("Wayland support not yet available");
        }
        else {
            X11Capturer::new(monitor)
        }
    }
    #[cfg(target_os = "windows")]
    bail!("Windows not yet supported");

    #[cfg(target_os = "macos")]
    bail!("MacOS not yet supported");
}

/// rectangular zone on screen to sample color from
#[derive(Deserialize)]
pub struct ZoneConfig {
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
    pub fn compare_sample(&self, other: &ZoneColor) -> f32 {
        let dr = (self.r as f32 - other.r as f32).abs();
        let dg = (self.g as f32 - other.g as f32).abs();
        let db = (self.b as f32 - other.b as f32).abs();

        (dr.powi(2) + dg.powi(2) + db.powi(2)).sqrt()
    }
    ///this function checks if any color channel exceeds a given threshold
    pub fn differs_from (&self, other: &ZoneColor, threshold: u8) -> bool {
        let diff = self.compare_sample(other);
        diff > threshold as f32
    }

}

///Used to sample a region on a monitor
pub struct ZoneSampler {
    config: ZoneConfig
}

impl ZoneSampler {
    pub fn new (config: ZoneConfig) -> Result<Self> {
        Ok(ZoneSampler {config})
    }

    pub fn get_light_name(&self) -> String {
        self.config.light_name.clone()
    }

    /// Captures average rgb values for a zone. Uses downsampling for larger zones.
    pub fn sample (&self, screenshot: &RgbaImage, downsample: u8) -> Result<ZoneColor> {

        //let time1 = Instant::now();

        //set loop start + stop for iterating through pixels
        let x_start = self.config.x;
        let x_end = x_start + self.config.width;
        let y_start = self.config.y;
        let y_end = y_start + self.config.height;


        // Calculate average
        let mut r_sum = 0u64;
        let mut g_sum = 0u64;
        let mut b_sum = 0u64;
        let mut count = 0u64;

        for y_pixel in (y_start..y_end).step_by(downsample as usize) {
            for x_pixel in (x_start..x_end).step_by(downsample as usize) {
                let pixel = screenshot.get_pixel(x_pixel, y_pixel);
                r_sum += pixel[0] as u64;
                g_sum += pixel[1] as u64;
                b_sum += pixel[2] as u64;
                count += 1;
            }

        }

        // println!("Total image process time: {}micro sec", time1.elapsed().as_micros());

        Ok(ZoneColor {
            r: (r_sum / count) as u8,
            g: (g_sum / count) as u8,
            b: (b_sum / count) as u8,
        })
    }
}
