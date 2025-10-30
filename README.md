## Overview
This is a work in progress. Pure rust + MQTT display and light sync utility.

## Compatibility
Z2M lights + X11 Linux only for now. Testing with Windows soon. Wayland is a long ways out.

Tested with KDE Plasma X11 and Z2M hosted in an LXC with an SLZB-06 coodinator.

## Usage
To use, build with cargo. Create config.yaml at ~/.config/zync/config.yaml, or run the first time without a config and it should create a sample config file for you and panic. Edit it with your MQTT and light settings and start the program again.


#### Sample yaml file
```yaml
# Sample configuration file for two lights and single zone covering full 1080p monitor
# Enter mqtt options, define lights, and set zones that map to those lights in this file.
mqtt:
  name: "my-connection"
  broker: "192.168.1.100"
  port: 1883
  user: "user name"         # optional depending on broker config
  password: "password"      # optional depending on broker config

downsample_factor: 10

lights:
  - light_name: "your_device_name"    # Must match the device name in Z2M. Can be a Z2M group or single light
    service: "Zigbee2MQTT"
    brightness: 200

zone:
  - name: "main_screen"
    x: 0
    y: 0
    width: 1920
    height: 1080
    light_name: "your_device_name"  # Must match device_name of the lights imported above

performance:
  target_fps: 10
  max_delay: 2000                  #max recovery delay in ms before retrying connection
  refresh_threshold: 10
```

## Known issues
Dont use more than 2 zones -- currently an issue with screen capture. More than 3 or more could cause a lot of latency and may require you to adjust target fps.

## Future plans
- Optimization of sampling and averaging
- Rework screen capture so additional zones don't add noteworthy latency
- Windows compatibilty
- Test adaptive framerate. I have a hunch its not actually working.
- Wayland is probably off the table until the xcap crate can support it or I understand async well enough to use other approaches!
