# WezTerm Scroll Smoothness Investigation - Handover Document

## Goal

Achieve smooth scrolling on Windows with:
- **Latency:** 15-18ms (measured with Typometer)
- **Frame StdDev:** <1ms (measured with PresentMon)
- **CPU Usage:** <20% during continuous scrolling
- **No stutters**

Must work across different display refresh rates (60Hz, 120Hz, 144Hz, 240Hz, etc.)

## Current Results (60Hz display, Mailbox mode)

| Metric | Current | Target | Status |
|--------|---------|--------|--------|
| Latency | 17-18ms | 15-18ms | ✓ |
| StdDev | 2-5ms | <1ms | ⚠️ |
| CPU | 20-28% | <20% | ⚠️ |
| Stutters | 0-9 | 0 | ⚠️ |

## Background: The Windows DWM Problem

On Windows, wgpu's Fifo (vsync) mode doesn't actually block waiting for vsync because DWM (Desktop Window Manager) handles composition. This means:

1. **Fifo mode:** Frames render uncapped, wasting CPU. wgpu's `present()` returns immediately.
2. **Mailbox mode:** Same as Fifo on Windows - presents immediately, DWM picks up latest frame at vsync.

The difference from Linux/macOS is that DWM is an additional compositor layer that decouples app rendering from display vsync.

## Approaches Tried

### 1. DwmFlush() after present (Fifo mode)
**Location:** `wezterm-gui/src/termwindow/render/draw.rs:270-280`

```rust
WebGpuPresentMode::Fifo => {
    unsafe { winapi::um::dwmapi::DwmFlush(); }
}
```

**Results:**
- StdDev: 0.03ms ✓ (excellent)
- CPU: 14.8% ✓
- Latency: **48ms** ❌ (adds ~30ms!)

**Why:** DwmFlush blocks until the frame is actually displayed by DWM, adding one full frame of latency.

### 2. Timer throttle in event loop (original WezTerm approach)
**Location:** `window/src/os/windows/window.rs:1621-1666`

The event loop has a timer-based throttle that delays paint requests based on `max_fps` config.

**Results:**
- StdDev: 0.88ms ✓
- CPU: 21.5% ✓
- Latency: **21ms** ❌

**Why:** The throttle blocks BEFORE paint, so when input arrives during throttled period, the first frame is delayed by up to `1000/max_fps` ms.

### 3. Bypass throttle + no pacing (pure Mailbox)
**Change:** Set `use_timer_throttle = false` for Mailbox mode

**Results:**
- StdDev: 1.15ms ✓
- CPU: **29.6%** ❌ (renders ~130fps)
- Latency: 15ms ✓

**Why:** No throttle means immediate response to input, but CPU spins rendering frames that get discarded.

### 4. Bypass throttle + post-present sleep (current approach)
**Location:** `wezterm-gui/src/termwindow/render/draw.rs:292-306`

```rust
WebGpuPresentMode::Mailbox | WebGpuPresentMode::AutoNoVsync => {
    dwm_vsync::ensure_timer_resolution();
    std::thread::sleep(Duration::from_millis(6));
}
```

**Results:**
- StdDev: 2-5ms ⚠️
- CPU: 20-28% ⚠️
- Latency: 17-18ms ✓

**Why:** Sleep after present doesn't delay the current frame (already presented), only the next render cycle. But fixed sleep duration is problematic for different refresh rates.

### 5. Vsync-aligned sleep using DwmGetCompositionTimingInfo
**Location:** `wezterm-gui/src/termwindow/render/draw.rs:36-87` (dwm_vsync module)

Attempted to sleep until `time_to_vsync - margin` using DWM timing info.

**Results:**
- StdDev: **6.15ms** ❌ (worse!)
- CPU: 15.4% ✓
- Latency: untested

**Why:** Windows sleep() is imprecise (even with `timeBeginPeriod(1)`). Sleep jitter of 1-3ms causes us to sometimes overshoot the vsync window, creating inconsistent frame times.

## The Unsolved Problem

**Fixed sleep duration doesn't scale across refresh rates:**

| Display | Refresh Period | With 6ms sleep (renders ~125fps) |
|---------|---------------|----------------------------------|
| 60Hz | 16.67ms | ✓ Works (extra frames discarded) |
| 144Hz | 6.94ms | ❌ Can't keep up, misses frames |
| 165Hz | 6.06ms | ❌ Misses more frames |
| 240Hz | 4.17ms | ❌ Misses 115 frames/sec |

**Required sleep for each refresh rate:**
- 60Hz: ~10-14ms sleep is fine
- 144Hz: needs ~4-5ms max sleep
- 240Hz: needs ~2ms max sleep
- 360Hz: needs ~1ms max sleep

## Possible Solutions to Explore

### A. Make sleep duration configurable
Add a config option like `frame_pacing_sleep_ms` that users can tune for their display. Simple but requires user knowledge.

### B. Query refresh rate and calculate sleep dynamically
Use `DwmGetCompositionTimingInfo` to get `qpcRefreshPeriod`, convert to ms, and calculate appropriate sleep:
```rust
let refresh_period_ms = get_refresh_period(); // e.g., 16.67 for 60Hz
let sleep_ms = (refresh_period_ms - 6.0).max(1.0); // leave 6ms margin
```

The challenge: we tried this and got jitter. Might need better handling of edge cases.

### C. Hybrid approach: timer throttle with input priority
Modify the timer throttle to allow immediate paint on input events, but throttle "idle" repaints (animations, cursor blink). This would give:
- Low latency for input (bypass throttle)
- Low CPU for idle (throttle active)

### D. Use Windows waitable timers instead of sleep()
`CreateWaitableTimerExW` with `HIGH_RESOLUTION` flag might give more precise timing than `Sleep()`. Could reduce jitter in vsync-aligned approach.

### E. Accept the tradeoff
For 60Hz displays: current 6ms sleep works well
For high refresh: fall back to timer throttle (accept higher latency)

## Key Code Locations

### Frame pacing (after present)
`wezterm-gui/src/termwindow/render/draw.rs:255-318`
- DwmFlush for Fifo mode
- Post-present sleep for Mailbox mode
- dwm_vsync module with timing utilities

### Timer throttle (event loop)
`window/src/os/windows/window.rs:1611-1666`
- `use_timer_throttle` flag controls whether throttle is active
- Currently only enabled for Immediate mode

### Config
`~/.wezterm.lua`:
```lua
config.front_end = "WebGpu"
config.webgpu_present_mode = "Mailbox"  -- or "Fifo"
config.max_fps = 60  -- affects timer throttle
```

## Benchmark Methodology

### Frame timing (PresentMon)
```powershell
.\benchmark_terminals.ps1 -Duration 10
```
Uses Intel PresentMon to capture frame times. Results in `benchmark_results\<timestamp>\`

### Input latency (Typometer)
Manual tool - measures keypress to screen update latency. Target: 15-18ms average.

### Key metrics
- **StdDev:** Frame time consistency. <1ms = smooth, >5ms = visible jitter
- **CPU:** During continuous scrolling. <20% = acceptable
- **Stutters:** Frames >2x average. Should be 0.

## Dependencies Added

`wezterm-gui/Cargo.toml`:
```toml
winapi = { features = ["profileapi", "timeapi"] }
```
- `profileapi`: QueryPerformanceCounter/Frequency for timing
- `timeapi`: timeBeginPeriod for 1ms timer resolution

## Git Commits

- `374c4af` - Mailbox frame pacing investigation - achieve 0.88ms stddev
- `f55a858` - Add DWM vsync timing utilities and frame timing benchmark
- `cca181c` - Post-present frame pacing for Mailbox mode (6ms sleep)
