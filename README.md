## Overview
This is a work in progress. Pure rust + MQTT display and light sync utility.

## Compatibility
Z2M lights + X11 Linux only for now. Testing with Windows soon. Wayland is a long ways out.

Tested with KDE Plasma X11 and Z2M hosted in an LXC with an SLZB-06 coodinator.

## Usage
To use, build with cargo. Create config.yaml at ~/.config/zync/config.yaml, or run the first time without a config and it should create a sample config file for you and panic. Edit it with your MQTT and light settings and start the program again.

## Current features
- Connects to MQTT broker and sends messages to Z2M to control lights
- Support for X11 Linux
- Dynamic transition and brightness based on screen changes. Slow transition for colors close in distance; fast for big jumps.
- Adaptive framerate. Config sets target for percent of thread time used for screen capture (e.g. 10fps = 100ms thread time. 0.25 means 25ms capture time will throttle framerate). This gives the user some control over CPU thread usage and handles spikes in performance by throttling.

## Roadmap
### Planned
- Wayland support via Pipewire. This may take awhile, but working on Linux Wayland is a key goal.
- Exploring Windows + MacOS support, and capture card feed for Raspi + HDMI capture card feed for TV support.
- User controls over aesthetics through abstractions or direct variables (e.g. "intensity: high" uses a preconfigured transition settings. The user could override them in the config).

### Other ideas in consideration
- CLI commands to start and stop, initialize a config, change settings
- HomeAssistant trigger for sync. Use a toggle (or any automation) to start and exit the sync loop
- Hue Gradient and other "segment" lights. Requires generics for "ZonePairs" and reworking Zone to Light mapping structure for a many-to-one relationship of Zones to a light's segments.

#### Sample yaml file
```yaml
# Sample configuration file for one light and one zone covering full 1080p monitor
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
    brightness: 200                   # Not yet used

zone:
  - name: "main_screen"
    x: 0
    y: 0
    width: 1920
    height: 1080
    light_name: "your_device_name"  # Must match device_name of the lights imported above.

performance:
  max_fps: 10
  max_delay: 2000                  #max recovery delay in ms before retrying connection
  refresh_threshold: 10
```
