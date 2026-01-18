---
tags:
  - gpu
---
# `webgpu_present_mode = "Mailbox"`

{{since('nightly')}}

Controls the GPU presentation timing and vsync behavior when using the WebGpu frontend.

This option is only applicable when you have configured `front_end = "WebGpu"`.

The possible values are:

* `"Mailbox"` - Low latency without tearing. The GPU renders frames as fast as possible and the most recent complete frame is presented at vsync. This is the default because it typically offers lower latency than Fifo without tearing (when supported by the driver/display).
* `"Fifo"` - Vsync enabled. Frames are presented in order, synchronized with the display refresh rate. Provides smooth visuals with no tearing, but may add input latency (typically 1-2 frames).
* `"Immediate"` - Lowest latency. Frames are presented immediately without waiting for vsync. This provides the lowest possible input latency but may cause screen tearing.
* `"AutoNoVsync"` - Automatically selects the best low-latency mode. Tries Mailbox first, falls back to Immediate if Mailbox is not supported, and finally falls back to Fifo.

If a requested mode is not supported by your display, wezterm will log a warning and fall back to Fifo.

## Example

To reduce input latency while avoiding tearing:

```lua
local config = {}
config.front_end = 'WebGpu'
config.webgpu_present_mode = 'Mailbox'
return config
```

For the absolute lowest latency (at the cost of potential tearing):

```lua
local config = {}
config.front_end = 'WebGpu'
config.webgpu_present_mode = 'Immediate'
return config
```

See also [webgpu_max_frame_latency](webgpu_max_frame_latency.md),
[webgpu_power_preference](webgpu_power_preference.md).
