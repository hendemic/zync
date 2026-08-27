## Overview
Real-time ambilight clone for Linux and MacOS using Zigbee2MQTT.

## Demo
https://github.com/user-attachments/assets/d539e25f-bb2c-441a-ba42-3de5c68eac9f

## Compatibility
Z2M lights on Linux (Wayland and X11)
Z2M lights on MacOS

Tested with:
- KDE Plasma (X11 + Wayland), Gnome Wayland
- MacOS 26
- Z2M hosted in an LXC with an SLZB-06 coodinator.

Note: Fullscreen apps (games, fullscreen video) are captured on Gnome Wayland because the app negotiates DMA-BUF buffers for the screencast stream. If the startup line says it fell back to shared-memory capture, fullscreen apps won't be captured on Gnome and the lights will hold their last color until you leave fullscreen. See Troubleshooting below.

## Requirements
- A Rust toolchain to build.
- For Linux: 
    - GStreamer 1.24+ with the base plugins, including the OpenGL elements. Arch: `gst-plugins-base`. Debian/Ubuntu: `gstreamer1.0-plugins-base` + `gstreamer1.0-gl`.
    - The PipeWire GStreamer plugin. Arch: `gst-plugin-pipewire`. Debian/Ubuntu: `gstreamer1.0-pipewire`.
    - `xdg-desktop-portal` plus a backend for your desktop, e.g. `xdg-desktop-portal-gnome`.
- For macOS:
    - Nothing to install. Capture goes through ScreenCaptureKit, called directly as FFI, so there are no build dependencies beyond the toolchain.
    - Permission to record the screen. The system prompt is raised on first run; once it has been answered, changing it means visiting System Settings → Privacy & Security → Screen & System Audio Recording. macOS files the grant against whatever *launched* zync, so the entry to enable is the terminal you started it from — not zync itself.

## Usage
Build with `cargo build --release`; the binary is `zync`.

```
zync start           # start syncing in the background, and give the terminal back
zync status          # is it running?
zync logs -f         # follow what it is doing
zync stop            # stop syncing and fade the lights back

zync start -f        # run in this terminal instead, logging as it goes
```

`zync start` detaches into its own session, so it keeps running after you close the shell. Use `-f`/`--foreground` when you want to watch it directly.

On first run `zync start` creates a commented config at `~/.config/zync/config.yaml` and exits so you can fill in your broker and lights. Config problems are reported by `zync start` itself rather than only landing in a log.

Stopping — with `zync stop`, with Ctrl-C on a foreground run, or with `kill` on the service — fades the lights back over five seconds to whatever they were showing before syncing started. `on_stop` in the config chooses that behaviour.

`zync stop` reaches the running instance over your MQTT broker, so it works from another terminal, another shell, or a script. Two topics are involved, both namespaced by the instance name:

| Topic | Purpose |
|---|---|
| `zync/<instance>/status` | retained `online` / `offline`, with `offline` as the last will |
| `zync/<instance>/control` | accepts `shutdown` |

### Running on more than one machine
The instance name defaults to your hostname, so pointing two machines at the same broker works without any config change — even if you copied `config.yaml` between them. It sets both the MQTT client id and the topics above, and both need to be unique per machine: a shared client id makes the broker disconnect each instance in turn (MQTT requires it), and a shared control topic means `zync stop` may reach the wrong machine.

Set `instance:` in the config only if you want a name other than the hostname.

### Files
Paths below are the Linux ones. macOS has no XDG state directory, so both the config and the state land under `~/Library/Application Support/zync/` instead. `zync status` prints the resolved config and log paths for the machine you are on.

| Path | Owner |
|---|---|
| `~/.config/zync/config.yaml` | you |
| `~/.local/state/zync/state.json` | the app — currently the screencast portal's restore token (Linux only) |
| `~/.local/state/zync/zync.pid` | the app — the running service, so `status` and `stop` can find it |
| `~/.local/state/zync/logs/zync.<date>.log` | the app — daily rotation, seven files kept. This is what `zync logs` reads |
| `~/.local/state/zync/logs/stderr.log` | the app — anything that escapes the logger, such as a panic |

#### Sample yaml file
```yaml
# Sample configuration: one light following a single zone covering a 1080p monitor.
# Enter your MQTT options, define lights, then set zones that map to those lights.
mqtt:
  name: "my-connection"
  broker: "192.168.1.100"
  port: 1883
  user: "user name"         # optional depending on broker config
  password: "password"      # optional depending on broker config

downsample_factor: 20       # pixel stride, in native display pixels

# Names this machine on the broker. Defaults to your hostname, which is usually
# what you want. Two machines pointed at the same broker must not share it: it
# sets both the MQTT client id and the topics `zync stop` uses, so a shared value
# means the two instances disconnect each other and `zync stop` may hit the wrong
# one. Only set this if you want a name other than the hostname.
# instance: "gaming-rig"

# What to do with the lights when syncing stops (zync stop, Ctrl-C, or a crash):
#   restore  put each light back the way it was before syncing started, falling
#            back to its fallback_state if that could not be read
#   default  always apply fallback_state
#   off      turn every light off
#   hold     leave the lights on the last colour they were sent
on_stop: restore

# How aggressively big colour jumps (cuts, explosions) are shortened relative to
# small, gradual changes:
#   slow     gentle fades throughout — good for film and ambient content
#   normal   the default balance (default if omitted)
#   extreme  snaps almost instantly on cuts — good for fast-paced games
# A custom curve is also accepted in place of a preset name:
#   intensity:
#     custom:
#       softness: 0.4          # falloff shape for small/gradual changes
#       cut_midpoint: 0.4      # normalized colour distance (0-1) where the cut kicks in
#       cut_steepness: 14.0    # how sharply transitions shorten past cut_midpoint
#       min_transition: 0.02   # fastest allowed transition, in seconds
#       max_transition: 1.0    # slowest allowed transition, in seconds
intensity: normal

lights:
  - light_name: "your_device_name"    # Must match the device name in Z2M. Can be a Z2M group or single light
    service: "Zigbee2MQTT"
    brightness: 0.8                   # percent brightness of light. range is 0-1. anything over 1 is rejected.
    is_group: false                   # set true for a Z2M group. Group commands are Zigbee broadcasts, which a mesh
                                      # only sustains at about 1/s, so groups are paced at 1 update/s (devices: 4/s).
    # max_updates_per_sec: 2          # optional override of that pacing for this light.

    # Used when on_stop is `default`, or when `restore` could not read this
    # light's previous state — which is common for groups. Anything the service
    # accepts works here; it is passed through untouched.
    # fallback_state:
    #   state: "ON"
    #   brightness: 200
    #   color_temp: 370

# Zones are always given in your display's native resolution. The app captures at
# a much smaller internal resolution for performance and converts these
# coordinates for you, so never scale them down yourself.
zones:
  - name: "main_screen"
    x: 0
    y: 0
    width: 1920
    height: 1080
    light_name: "your_device_name"  # Must match a light_name defined above

performance:
  max_fps: 12                       # max_fps. make sure it isn't too high for your lights. 10-12 is a safe starting point.
  max_delay: 500                    # max recovery delay in ms before retrying connection
  refresh_threshold: 10             # difference in color required to send MQTT light change
  percent_thread_work: 0.25         # max work/interval ratio.
  fps_reporting: 10                 # time in seconds between fps averages in the log. raise percent_thread_work for higher FPS.
  max_commands_per_sec: 6           # ceiling on light commands/sec across all zones.
                                    # Zigbee groups saturate well below the frame
                                    # rate; lower this if you see BUSY errors in Z2M.
```

## Current features
- Connects to MQTT broker and sends messages to Z2M to control lights
- Support for Linux (X11 and Wayland) and macOS, one capture backend per platform behind a common interface
- Runs as a background service: `zync start`, `zync status`, `zync logs`, `zync stop`. Stop reaches the running instance over MQTT, so it works from any terminal
- Lights are returned to their previous state on stop. Each light's state is read back from Z2M at startup; lights that don't report one (groups, usually) fall back to a configured `fallback_state`
- Dynamic transition and brightness based on screen changes. Slow transition for colors close in distance; fast for big jumps, with a cut gate that snaps big jumps (cuts, explosions) even faster without changing the pacing of small, gradual changes. Tunable via `intensity` (`slow`, `normal`, `extreme`, or a custom curve).
- Adaptive framerate driven by the light network itself. Zigbee2MQTT's log stream is monitored for delivery failures, and the send rate backs off whenever the mesh reports congestion. `percent_thread_work` remains as a secondary CPU guard (e.g. 10fps = 100ms thread time; 0.25 means 25ms of capture time will throttle the framerate).
  - Earlier versions throttled on CPU work time alone. That only ever worked on X11, where a screen grab is genuinely expensive; on Wayland the capture is a cheap buffer read, so the loop never backed off and flooded the Zigbee mesh instead.
- `max_commands_per_sec` puts a hard ceiling on commands reaching the mesh, independent of framerate. Zone updates that exceed the budget stay pending rather than being dropped.
- Rotating log files, and the Wayland monitor picker only appears once — the portal's restore token is persisted.
- Multiple machines can sync against one broker; each is namespaced by its hostname unless `instance` says otherwise.

## Architecture
Four crates, so the dependency direction is enforced by the compiler rather than by review.

```
zync-core       domain model, ports, sync loop, supervisor — no platform deps
zync-capture    screen capture, one backend per platform
zync-adapters   Zigbee2MQTT, MQTT bus, config on disk
zync            the binary: logging, CLI
```

`zync-capture` is the only crate allowed to use `unsafe`; every other crate forbids it, so the native boundary stays confined to the capture backends. Nothing above that crate knows which backend is running.

## Roadmap
### Planned
- Home Assistant toggle to start and stop syncing. The control channel it needs already exists.
- A TUI for creating zones visually.
- Windows capture backend.
- Capture card feed for Raspi + HDMI capture card feed for TV support.

### Other ideas in consideration
- Hue Gradient and other "segment" lights. Requires reworking the zone-to-light mapping into a many-to-one relationship of zones to a light's segments.
- `zync config` and `zync doctor` subcommands.
- A systemd user unit, so syncing can start with the session.

## Troubleshooting

### Checking what the capture is doing

```
zync logs                 # recent events
zync logs -f              # follow
zync logs -s              # just the most recent run, in full
zync logs -v              # include per-frame detail (same as --level debug)
zync logs --level warn    # problems only
zync logs --level all     # everything in the file
zync logs -n 200          # more history
```

The log file always records zync's own detail, so `-v` works on a run that has already finished — you don't have to have predicted before starting that you'd want it. Third-party crates are recorded at info, and zbus at error, because rumqttc and gstreamer at debug would bury everything, and zbus warns about property caching for every portal request whose object has already gone away.

`--level` and `-v` filter what is printed. What gets *recorded* is `RUST_LOG`, read when the service starts; overriding it replaces the defaults above, so keep `zbus=error` in it. `ZYNC_DEBUG=1` still works, and now means "show the detail on the terminal too" for a foreground run.

Per-frame diagnostics live at debug:
- `frames` — frames the compositor actually delivered during the reporting interval. `0` while a fullscreen app is open means the compositor stopped feeding the stream.
- `sent` — light commands sent.
- `deferred` — updates held back by the command budget.
- `capture rate` — the frame rate, once per `fps_reporting` seconds.
- one line per zone with its last sampled colour, current pacing, and failures charged to it.

At startup an event line reports the capture source resolution and the capture mode. On Linux that is DMA-BUF (frames stay on the GPU and are scaled there) or shared memory (the fallback path); on macOS it is ScreenCaptureKit, which scales in the compositor.

### Lights freeze when a game or video goes fullscreen (Gnome Wayland)
When Mutter hands a fullscreen window straight to the display (direct scanout), the monitor screencast stream stops delivering frames entirely unless the consumer negotiated DMA-BUF buffers. Shared-memory streams get zero frames until the app leaves fullscreen. This app negotiates DMA-BUF and scales frames on the GPU, so it handles that automatically — and it also avoids a full-resolution GPU to CPU readback that gnome-shell was doing for every frame, which was a big chunk of the CPU cost on Wayland.

If the startup line reports shared-memory mode, try these in order:
1. Install the GStreamer GL plugins listed under Requirements. Missing GL elements are the usual reason negotiation fails.
2. `ZYNC_FORCE_SHM=1` exists only for A/B testing the two paths. It will reproduce the freeze — don't set it otherwise.
3. System-side, direct scanout can be turned off compositor-wide with `MUTTER_DEBUG_PAINT=disable-direct-scanout` in gnome-shell's environment (e.g. a file in `~/.config/environment.d/`, then log out and back in), or with the "Disable unredirect fullscreen windows" Gnome extension. Both cost fullscreen latency and performance, so treat them as last resorts.
4. Selecting the specific window instead of the monitor in the portal picker also works. Window streams don't depend on the compositor painting the screen.

### The monitor picker appears every time (Linux)
The portal's restore token is saved to `~/.local/state/zync/state.json`. Delete that file to be asked again; if it keeps reappearing, your portal backend may not be issuing a token.

### Zigbee2MQTT logs show status=BUSY
Lower `max_commands_per_sec` in the config. Groups are sent as multicast and will saturate the mesh well below any frame rate you'd want to run at.

### `zync stop` says no running instance was found
It looks for the retained `zync/<instance>/status` message, and prints the instance name it looked for. Two reasons it comes up empty:
- Your broker was restarted after `zync start`, so the retained message is gone. Use Ctrl-C in the terminal running it instead.
- The instance name differs between the two invocations, e.g. `instance:` is set in one config and not another. `zync start` logs the name it registered under.
