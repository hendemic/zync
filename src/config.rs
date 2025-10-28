#[allow(dead_code)]
mod config {
    struct AppConfig {
        mqtt: MQTTClient,
        lights: Vec<LightConfig>,
        zones: Vec<ZoneConfig>,
        sampling: SamplingConfig,
        performance: PerformanceConfig,
    }

    impl AppConfig {
        fn load(&self) {
        }

    }
}
