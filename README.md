## Overview
This is a work in progress. Pure rust + MQTT display and light sync utility.

## Compatibility
Z2M lights on Linux (Wayland and X11)

Tested with:
- KDE Plasma (X11 + Wayland), Gnome Wayland
- Z2M hosted in an LXC with an SLZB-06 coodinator.

Note: In some fullscreen games on Gnome Wayland, you'll need to boot the game and then start this app and actually select the specific window. Still figuring out what can be done to make this work better, but fullscreen games sometimes bypass the pipewire stream in Gnome.

## Usage
To use, build with cargo. Create config.yaml at ~/.config/zync/config.yaml, or run the first time without a config and it should create a sample config file for you and panic. Edit it with your MQTT and light settings and start the program again.

#### Sample yaml file
```yaml
# Sample configuration file for one light and single zone covering full 1080p monitor
# Enter mqtt options, define lights, and set zones that map to those lights in this file.
mqtt:
  name: "my-connection"
  broker: "192.168.1.100"
  port: 1883
  user: "user name"         # optional depending on broker config
  password: "password"      # optional depending on broker config

downsample_factor: 20

lights:
  - light_name: "your_device_name"    # Must match the device name in Z2M. Can be a Z2M group or single light
    service: "Zigbee2MQTT"
    brightness: 0.8                   # percent brightness of light. range is 0-1. anything over 1 will be capped to 1 by the app.

zone:
  - name: "main_screen"
    x: 0
    y: 0
    width: 1920
    height: 1080
    light_name: "your_device_name"  # Must match device_name of the lights imported above

performance:
  max_fps: 12                       # max_fps. make sure it isn't too high for your lights. 10-12 is a safe starting point.
  max_delay: 500                    # max recovery delay in ms before retrying connection
  refresh_threshold: 10             # difference in color required to send MQTT light change
  percent_thread_work: 0.25         # max work/interval ratio.
  fps_reporting: 10                 # time in seconds between fps averages output in terminal. raise percent_thread_work for higher FPS.
```

## Current features
- Connects to MQTT broker and sends messages to Z2M to control lights
- Support for X11 Linux and Wayland
- Dynamic transition and brightness based on screen changes. Slow transition for colors close in distance; fast for big jumps.
- Adaptive framerate. Config sets target for percent of thread time used for screen capture (e.g. 10fps = 100ms thread time. 0.25 means 25ms capture time will throttle framerate). This gives the user some control over CPU thread usage and handles spikes in performance by throttling.
  - This approach only works on X11. Wayland with pipewire is extremely low latency and the pipewire stream is what uses the most CPU.

## Roadmap
### Planned
- Exploring Windows + MacOS support, and capture card feed for Raspi + HDMI capture card feed for TV support.
- User controls over aesthetics through abstractions or direct variables (e.g. "intensity: high" uses a preconfigured transition settings. The user could override them in the config).

### Other ideas in consideration
- CLI commands to start and stop, initialize a config, change settings
- HomeAssistant trigger for sync. Use a toggle (or any automation) to start and exit the sync loop
- Hue Gradient and other "segment" lights. Requires generics for "ZonePairs" and reworking Zone to Light mapping structure for a many-to-one relationship of Zones to a light's segments.
