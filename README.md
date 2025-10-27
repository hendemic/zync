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
- **Spawn background thread for MQTT event loop** (handles connection.iter())
- Initialize LightControllers (pass config.lights + client)
- Initialize ScreenZone (pass config.screen)
- Initialize SyncEngine (pass controllers, zone, config.performance)
- Handle graceful shutdown (Ctrl+C)
- Run sync engine in main thread
- Error reporting to user

### config.rs
- mod config
  - struct AppConfig
    - mqtt: MqttConfig
    - lights: Vec<LightConfig> (imported from lights.rs)
    - screen: ScreenConfig (imported from capture.rs)
    - performance: PerformanceConfig (imported from sync.rs)
    - fn load (loads TOML, deserializes into AppConfig)
  - struct MqttConfig
    - broker, port, username, password
    - fn create_client (returns Client and Connection)

### lights.rs
- mod lights
  - enum LightService (Zigbee2MQTT, HomeAssistant, ESPHome, Tasmota)
  - struct LightConfig
    - name, service, device_name, max_brightness
  - struct LightController
    - config: LightConfig
    - client: Client
    - fn new (takes LightConfig, Client)
    - fn get_topic (formats topic based on service)
    - fn format_payload (formats payload based on service)
    - fn set_light (takes LightSetting, publishes via MQTT client)

### capture.rs
- mod capture
  - struct ScreenConfig
    - downsample_factor, color_change_threshold
  - struct ScreenZone
    - config: ScreenConfig
    - fn new (takes ScreenConfig)
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
    - **fn run (main sync loop - runs in main thread)**
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
