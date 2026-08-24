## Overview
Real-time ambilight clone for Linux + Zigbee2MQTT in Rust.

## Demo
https://github.com/user-attachments/assets/d539e25f-bb2c-441a-ba42-3de5c68eac9f



## Compatibility
Z2M lights on Linux (Wayland and X11)

Tested with:
- KDE Plasma (X11 + Wayland), Gnome Wayland
- Z2M hosted in an LXC with an SLZB-06 coodinator.

Note: Fullscreen apps (games, fullscreen video) are captured on Gnome Wayland because the app negotiates DMA-BUF buffers for the screencast stream. If the startup line says it fell back to shared-memory capture, fullscreen apps won't be captured on Gnome and the lights will hold their last color until you leave fullscreen. See Troubleshooting below.

## Requirements
- A Rust toolchain to build.
- GStreamer 1.24+ with the base plugins, including the OpenGL elements. Arch: `gst-plugins-base`. Debian/Ubuntu: `gstreamer1.0-plugins-base` + `gstreamer1.0-gl`.
- The PipeWire GStreamer plugin. Arch: `gst-plugin-pipewire`. Debian/Ubuntu: `gstreamer1.0-pipewire`.
- `xdg-desktop-portal` plus a backend for your desktop, e.g. `xdg-desktop-portal-gnome`.

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

downsample_factor: 20       # pixel stride, in native display pixels

lights:
  - light_name: "your_device_name"    # Must match the device name in Z2M. Can be a Z2M group or single light
    service: "Zigbee2MQTT"
    brightness: 0.8                   # percent brightness of light. range is 0-1. anything over 1 will be capped to 1 by the app.
    is_group: false                   # set true for a Z2M group. Group commands are Zigbee broadcasts, which a mesh
                                      # only sustains at about 1/s, so groups are paced at 1 update/s (devices: 4/s).
    # max_updates_per_sec: 2          # optional override of that pacing for this light.

# Zones are always given in your display's native resolution. The app captures at
# a much smaller internal resolution for performance and converts these
# coordinates for you, so never scale them down yourself.
zones:
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
  max_commands_per_sec: 6           # ceiling on light commands/sec across all zones.
                                    # Zigbee groups saturate well below the frame
                                    # rate; lower this if you see BUSY errors in Z2M.
```

## Current features
- Connects to MQTT broker and sends messages to Z2M to control lights
- Support for X11 Linux and Wayland
- Dynamic transition and brightness based on screen changes. Slow transition for colors close in distance; fast for big jumps.
- Adaptive framerate driven by the light network itself. Zigbee2MQTT's log stream is monitored for delivery failures, and the send rate backs off whenever the mesh reports congestion. `percent_thread_work` remains as a secondary CPU guard (e.g. 10fps = 100ms thread time; 0.25 means 25ms of capture time will throttle the framerate).
  - Earlier versions throttled on CPU work time alone. That only ever worked on X11, where a screen grab is genuinely expensive; on Wayland the capture is a cheap buffer read, so the loop never backed off and flooded the Zigbee mesh instead.
- `max_commands_per_sec` puts a hard ceiling on commands reaching the mesh, independent of framerate. Zone updates that exceed the budget stay pending rather than being dropped.

## Roadmap
### Planned
- Exploring Windows + MacOS support, and capture card feed for Raspi + HDMI capture card feed for TV support.
- User controls over aesthetics through abstractions or direct variables (e.g. "intensity: high" uses a preconfigured transition settings. The user could override them in the config).

### Other ideas in consideration
- CLI commands to start and stop, initialize a config, change settings
- HomeAssistant trigger for sync. Use a toggle (or any automation) to start and exit the sync loop
- Hue Gradient and other "segment" lights. Requires generics for "ZonePairs" and reworking Zone to Light mapping structure for a many-to-one relationship of Zones to a light's segments.

## Troubleshooting

### Checking what the capture is doing
Run with `ZYNC_DEBUG=1` for diagnostics on stderr.

At startup you get a line reporting the capture source resolution and the capture mode: DMA-BUF (frames stay on the GPU and are scaled there) or shared memory (the fallback path). Then a diagnostics line prints periodically:
- `frames` — frames the compositor actually delivered during the reporting interval. `0` while a fullscreen app is open means the compositor stopped feeding the stream.
- `sent` — light commands sent.
- `deferred` — updates held back by the command budget.
- `zones` — last sampled RGB per zone.

### Lights freeze when a game or video goes fullscreen (Gnome Wayland)
When Mutter hands a fullscreen window straight to the display (direct scanout), the monitor screencast stream stops delivering frames entirely unless the consumer negotiated DMA-BUF buffers. Shared-memory streams get zero frames until the app leaves fullscreen. This app negotiates DMA-BUF and scales frames on the GPU, so it handles that automatically — and it also avoids a full-resolution GPU to CPU readback that gnome-shell was doing for every frame, which was a big chunk of the CPU cost on Wayland.

If the startup line reports shared-memory mode, try these in order:
1. Install the GStreamer GL plugins listed under Requirements. Missing GL elements are the usual reason negotiation fails.
2. `ZYNC_FORCE_SHM=1` exists only for A/B testing the two paths. It will reproduce the freeze — don't set it otherwise.
3. System-side, direct scanout can be turned off compositor-wide with `MUTTER_DEBUG_PAINT=disable-direct-scanout` in gnome-shell's environment (e.g. a file in `~/.config/environment.d/`, then log out and back in), or with the "Disable unredirect fullscreen windows" Gnome extension. Both cost fullscreen latency and performance, so treat them as last resorts.
4. Selecting the specific window instead of the monitor in the portal picker also works. Window streams don't depend on the compositor painting the screen.

### Zigbee2MQTT logs show status=BUSY
Lower `max_commands_per_sec` in the config. Groups are sent as multicast and will saturate the mesh well below any frame rate you'd want to run at.
