use rumqttc::{Client, MqttOptions, QoS};
//use std::io::{self};
use std::thread;
use std::time::Duration;
mod config;
mod lights;

fn main() {
    let mut mqttoptions = MqttOptions::new("zync-test", "192.168.10.20", 1883);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    mqttoptions.set_credentials("rust-mqtt", "rust");

    let (client, mut connection) = Client::new(mqttoptions, 10);
    client.subscribe("hello/rumqtt", QoS::AtMostOnce).unwrap();

    let light = "zigbee2mqtt/mikes-office-lightstrip-lower/set";

    thread::spawn(move || {
        for i in 0..6 {
            let brightness = if i % 2 == 0 { 50 } else { 0 };
            let light_command = format!(
                r#"{{"brightness":{},"color":{{"r":255,"g":0,"b":0}},"transition":0.15}}"#,
                brightness
            );
            let delay = 1000;
            // let delay = if i < 5 {
            //     1000
            // } else if i < 15 {
            //     250
            // } else {
            //     150
            // };

            println!(
                "Cycle {}: Setting brightness to {} ({}ms delay)",
                i + 1,
                brightness,
                delay
            );

            // Use try_publish to avoid building up buffer - skip if channel is full
            match client.try_publish(light, QoS::AtMostOnce, false, light_command.as_bytes()) {
                Ok(_) => {}
                Err(_) => println!("  Skipped - channel full"),
            }

            thread::sleep(Duration::from_millis(delay));
        }

        // User input and color changing code (commented out)
        // loop {
        //     println!("enter color:");
        //     let mut input = String::new();
        //     io::stdin().read_line(&mut input).unwrap();
        //     let input = input.trim();
        //
        //     let (r, g, b) = match input {
        //         "red" => (255, 0, 0),
        //         "green" => (0, 255, 0),
        //         "blue" => (0, 0, 255),
        //         "white" => (255, 255, 255),
        //         "purple" => (255, 0, 255),
        //         "yellow" => (255, 255, 0),
        //         "cyan" => (0, 255, 255),
        //         "quit" => break,
        //         _ => {
        //             println!("Unknown color");
        //             continue;
        //         }
        //     };
        //
        //     let light_command = format!(
        //         r#"{{"state":"ON","brightness": 255, "color":{{"r":{},"g":{},"b":{}}}}}"#,
        //         r, g, b
        //     );
        //
        //     client
        //         .publish(light, QoS::AtMostOnce, false, light_command.as_bytes())
        //         .unwrap();
        //
        //     thread::sleep(Duration::from_millis(1000));
        // }
    });

    // Iterate to poll the eventloop for connection progress
    for (_i, _notification) in connection.iter().enumerate() {
        //println!("Notification = {:?}", notification);
    }
}
