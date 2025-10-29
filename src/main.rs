use std::thread;
use std::time::Duration;
use anyhow::Result;

use crate::config::AppConfig;
use crate::lights::*;
mod config;
mod lights;
mod capture;
mod sync;

// This is current just a test of the config loading, and toggling a single light. Will update once I have sync fully defined.
fn main() -> Result<()> {
    //load configuration file
    let config = AppConfig::load()?;

    //set connection
    let (client, mut connection) = config.mqtt.create_client()?;

    // start notification thread
    thread::spawn(move || {
        for notification in connection.iter().enumerate() {
            println!("Notification = {:?}", notification);
        }
    });

    // test by changing a light with a single message
    let light_config = &config.lights[0];
    let controller = LightController::new(light_config.clone(), &client);

    let color = ColorCommand::new(255, 0, 0);
    controller.set_light(color, Some(0.5))?;

    thread::sleep(Duration::from_secs(3));

    // keep running for 30s to test connection
    for _k in 0..30 {
        thread::sleep(Duration::from_secs(1));
        println!("Running...");
    }

    Ok(())
}
