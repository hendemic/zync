This is a work in progress. Pure rust + MQTT display and light sync utility.

X11 Linux only for now. Testing with Windows soon. Wayland is a long ways out.

To use, build with cargo. Create config.yaml at ~/.config/zync/config.yaml, or run the first time without a config and it should create a sample config file for you and panic. Edit it with your MQTT and light settings and start the program again.

Sample yaml file
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
