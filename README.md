# Architecture

## Working Scope
- Configuration file and config loader
  - Light or light group references
  - Downstream service (Z2M, ZHA, etc)
  - MQTT config
  - Performance (target framerate, fails before dynamically adjusting, etc)
- Dynamic framerate
  - Error handling for stuck Zigbee
- One zone on screen using OpenCV
- Started directly from terminal before gaming (or via steam launcher)

## Future Scope
- Onboarding and set up from CLI
- Additional CLI commands (framerate override, add lights, etc)

## Module Definition
### main.rs
- Parse CLI arguments
- Load AppConfig from config.rs
- Extract config.mqtt and create MQTT client/connection
- Spawn background thread for MQTT event loop
- Initialize LightControllers (pass config.lights)
- Initialize ScreenZone (pass config.screen)
- Initialize SyncEngine (pass controllers, zone, config.performance)
- Handle graceful shutdown (Ctrl+C)
- Run sync engine in main thread
- Error reporting to user

### config.rs
- mod config (note: read rust book on generic types and use those here. decide if I need to make this modular from concrete config types/structs defined in other mods. There is an argument not to since config is specific to the app and I won't really use these with any other types/structrs from those defined here...)
  - struct AppConfig
    - mqtt: MqttClient
    - lights: Vec<LightConfig> (implements from lights.rs)
    - screen: ScreenConfig (implements from capture.rs)
    - performance: PerformanceConfig (implements from sync.rs)
    - fn load (loads TOML)
  - struct MqttClient
    - broker, port, username, password
    - fn create_client (returns Client and Connection)
  - in the future most CLI config functions defined here. "run" will use the sync mod.

### lights.rs
- mod lights
  - enum LightService (Zigbee2MQTT, HomeAssistant ZHA, ESPHome, etc. Start with Z2M only but use this enum and add logic for future platforms to test. Could expand app to use Hue API too depending on complexity.)
  - struct LightConfig
    - name, service, device_name, max_brightness
  - struct LightController
    - config: LightConfig
    - client: Client
    - fn new (takes LightConfig, Client)
    - fn get_topic (formats topic based on LightService enum)
    - fn format_payload (formats payload based on service)
    - fn set_light (takes LightSetting, formats payload, and publishes payload to MQTT client)
      - Note: either use generics here so lights doesn't have an MQTT dependency or make the actual publishing a function in MQTTClient I think a generic here makes sense though in case I set up others in the future.
      - Need to read more on generic types and what I need to implement where.
      - Ideally this just implements the specific delivery code defined in MQTTClient, that could be HueAPI, ZWaveClient, etc in the future
      - Could also just send it and make lights dependent on MQTT for now and figure out a more graceful architecture if I ever need it...

### capture.rs
- mod capture
  - struct ScreenConfig
    - downsample_factor, color_change_threshold, zones (hashmap of ScreenZone + Vec<LightConfig>)
      - note: need to think through this more. I don't necessarily want lights and capture to be so dependent. but maybe its fine and I just put these in one file as two very related modules and call it a day. since capture defines the colors we're sending to lights, and zones/lights are tightly coupled and using generics and forcing these to be modular from one another seems like it makes these interfaces harder to use anyway.
  - struct ScreenZone
    - fn new
    - fn sample_screen (captures and returns LightSetting)
    - fn calculate_average_color
  - struct LightSetting
    - r, g, b, brightness
    - fn new
    - fn from_color_auto_brightness
    - fn differs_from (threshold comparison)

### sync.rs
- mod sync
  - struct PerformanceConfig
    - target_fps, min_fps, adaptive_framerate
  - struct AdaptiveRate
    - target_interval, current_interval, consecutive_successes/failures
    - fn new (takes target_fps, min_fps)
    - fn on_success
    - fn on_failure
    - fn get_interval
  - struct SyncEngine
    - lights: Vec<LightController>
    - zone: ScreenZone
    - rate: AdaptiveRate
    - config: PerformanceConfig
    - fn new (takes Vec<LightController>, ScreenZone, PerformanceConfig)
    - fn run (main sync loop - runs in main thread)
      - Loop: sample screen → publish to lights → handle success/failure → sleep

## Thread Architecture
- **Main Thread:** Runs SyncEngine.run() - screen capture, color calculation, publishing commands
- **Background Thread:** Runs connection.iter() - MQTT network I/O, keeps connection alive

## Data Flow
1. Load AppConfig from TOML (config.rs)
2. Extract pieces:
   - config.mqtt → create MQTT client/connection
   - config.lights → create Vec<LightController>
   - config.screen → create ScreenZone
   - config.performance → pass to SyncEngine
3. Run SyncEngine with constructed components
