# WezTerm Scroll Smoothness - Current State

## Summary

Investigation into achieving smooth scrolling with low latency on Windows 11.

**Latest Benchmark Results (Mailbox mode):**
| Metric | Result | Target | Status |
|--------|--------|--------|--------|
| StdDev | **0.88ms** | <1ms | ✓ |
| CPU | **21.5%** | <20% | ⚠️ (1.5% over) |
| Stutters | **0** | 0 | ✓ |
| Frames | 607 (60.7fps) | ~60fps | ✓ |
| Latency | ~18ms | 15-18ms | ✓ (estimated) |

## Key Findings

### 1. wgpu Fifo Mode Doesn't Block on Windows
On Windows, wgpu's Fifo (vsync) mode doesn't actually block waiting for vsync because DWM handles composition. This means frames render uncapped, wasting CPU.

**Solution:** Call `DwmFlush()` after `present()` in Fifo mode. This blocks until the frame is displayed by DWM, giving perfect frame pacing (0.03ms stddev) but adds ~30ms latency.

### 2. Mailbox Mode Gives Low Latency but High CPU
Without any pacing, Mailbox mode renders ~1500 fps, most of which are discarded by DWM. This gives excellent latency (~18ms) and stddev (~0.98ms) but uses 38% CPU.

### 3. Existing Timer Throttle Works Well
WezTerm already has a timer-based throttle in `window/src/os/windows/window.rs` that limits paint requests based on `max_fps` config. This throttle:
- Uses `async_io::Timer` to schedule next paint
- Prevents >max_fps paint requests
- Works well with Mailbox mode

### 4. Custom Frame Pacing Caused Jitter
Attempts to add vsync-aligned frame pacing after `present()` using `DwmGetCompositionTimingInfo` caused jitter because:
- Windows sleep timing isn't precise (even with `timeBeginPeriod(1)`)
- The custom pacing interfered with the existing timer throttle
- Result: 8-10ms stddev (worse than no pacing)

## Current Implementation

### draw.rs (WebGPU rendering)
```rust
match self.config.webgpu_present_mode {
    WebGpuPresentMode::Fifo => {
        // Use DwmFlush for smooth frame pacing (adds latency)
        DwmFlush();
    }
    WebGpuPresentMode::Mailbox | WebGpuPresentMode::AutoNoVsync => {
        // No additional pacing - rely on timer throttle in window.rs
    }
    WebGpuPresentMode::Immediate => {
        // No pacing
    }
}
```

### window.rs (Event loop throttle)
- For non-Fifo modes: Timer throttle to `max_fps`
- For Fifo mode: No throttle (DwmFlush handles pacing)

## Recommended Configuration

For best balance of latency, smoothness, and CPU:

```lua
-- ~/.wezterm.lua
config.front_end = "WebGpu"
config.webgpu_present_mode = "Mailbox"
config.max_fps = 60  -- Match your display refresh rate
config.webgpu_max_frame_latency = 1
```

## Files Modified

- `wezterm-gui/src/termwindow/render/draw.rs` - Frame pacing logic, DwmFlush for Fifo
- `wezterm-gui/Cargo.toml` - Added `profileapi` and `timeapi` features
- `window/src/os/windows/window.rs` - Timer throttle bypass for Fifo mode
- `benchmark_terminals.ps1` - Automated benchmark script
- `benchmark_scroll.cmd` - Infinite scroll generator
- `CLAUDE.md` - Updated with Windows development notes

## Utilities Added

### dwm_vsync module (draw.rs)
- `ensure_timer_resolution()` - Sets Windows timer to 1ms resolution
- `time_until_next_vsync()` - Gets time to next vsync using DwmGetCompositionTimingInfo
- `precise_sleep()` - Sleep wrapper (currently just std::thread::sleep)

These utilities are available for future optimization attempts.

## Future Work

To get CPU under 20% while maintaining current smoothness:

1. **Try `max_fps = 60`** in config (currently 120) since display is 60Hz
2. **Investigate waitable timer objects** - More precise than async_io::Timer
3. **Event loop optimization** - The CPU usage may be from event processing, not rendering
4. **Profile CPU usage** - Identify where cycles are being spent

## Benchmark Script Usage

```powershell
# Run benchmark (WezTerm only)
.\benchmark_terminals.ps1 -Duration 10

# Results saved to benchmark_results\<timestamp>\
```

Edit `$EnabledTerminals` in script to test other terminals (Alacritty, Windows Terminal).
