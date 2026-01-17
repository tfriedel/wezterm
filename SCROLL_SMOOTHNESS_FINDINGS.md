# Scroll Smoothness Investigation Findings

## Date: 2026-01-17

## Problem Statement
Stuttery/unsmooth scrolling on Windows 11 compared to Windows Terminal, especially visible when scrolling in neovim in tmux over WSL.

## Test Configuration
```lua
config.front_end = "WebGpu"
config.max_fps = 120
config.webgpu_present_mode = "Fifo"  -- vsync enabled
config.webgpu_max_frame_latency = 1
```

## Diagnostic Metrics Added
We added frame timing instrumentation to measure:

| Metric | Location | Purpose |
|--------|----------|---------|
| `gui.frame.interval` | paint.rs | Time between consecutive frames |
| `gui.frame.interval.jitter` | paint.rs | Deviation from target interval (1000ms/max_fps) |
| `gui.frame.acquire_texture` | draw.rs | Time spent in `get_current_texture()` |
| `gui.frame.present_wait` | draw.rs | Time spent in `present()` |

## Key Findings

### 1. Vsync Is Not Blocking
Despite `webgpu_present_mode = "Fifo"`, neither `acquire_texture` nor `present` blocks for vsync:

| Operation | Expected (120Hz) | Actual |
|-----------|------------------|--------|
| `acquire_texture` | ~8.33ms | 40-60µs |
| `present` | ~8.33ms | 50-125µs |

**Implication**: Frames are not being paced by vsync. The GPU is not waiting for vertical blank.

### 2. Frame Intervals Are Highly Variable
Sample data during scrolling:

```
frame interval=17.3ms  jitter=9.0ms
frame interval=10.9ms  jitter=2.6ms
frame interval=9.8ms   jitter=1.5ms
frame interval=74.2ms  jitter=65.8ms   <- large gap
frame interval=526.8ms jitter=518.5ms  <- huge gap (input pause)
frame interval=11.7ms  jitter=3.4ms
frame interval=90.3ms  jitter=81.9ms   <- large gap
```

**Pattern**: Frames come in bursts when input arrives, then large gaps waiting for next input event.

### 3. Root Cause: Timer-Based Paint Throttle

Location: `window/src/os/windows/window.rs:1611-1657`

```rust
unsafe fn wm_paint(hwnd: HWND, ...) -> Option<LRESULT> {
    // ...
    if inner.paint_throttled {
        inner.invalidated = true;
        return Some(0);  // Skip paint if throttled
    }

    // Dispatch paint request
    inner.events.dispatch(WindowEvent::NeedRepaint);

    // Start timer-based throttle
    inner.paint_throttled = true;
    promise::spawn::spawn(async move {
        // THIS IS THE PROBLEM: Timer doesn't align with vsync!
        async_io::Timer::after(Duration::from_millis(1000 / max_fps)).await;
        // ... re-enable painting after timer
    }).detach();
}
```

**The Problem**:
1. Paint requests are throttled by an async timer (8.33ms at 120fps)
2. This timer runs independently of the display's vsync signal
3. Frames arrive at random phases relative to vsync boundaries
4. Creates double-delay: timer delay + potential vsync wait
5. Results in inconsistent frame pacing

### 4. Why Fifo Mode Isn't Helping

In proper vsync operation:
- `acquire_texture()` or `present()` should block until vsync
- This blocking naturally paces frames to display refresh rate
- No additional throttling needed

Current behavior:
- Timer throttle requests frames at timer intervals
- These don't align with vsync boundaries
- GPU/driver may be triple-buffering or dropping frames
- Results in variable frame delivery

## Proposed Fix

### Phase 1: Detect Fifo Mode in Window Code
Pass the present mode configuration to the window layer so it knows whether vsync is active.

### Phase 2: Disable Timer Throttle for Fifo Mode
When using Fifo (vsync) present mode:
- Skip the `async_io::Timer` based throttle
- Let vsync blocking in `acquire_texture`/`present` naturally pace frames
- This ensures frames align with vsync boundaries

### Phase 3: Validate
After fix, expected metrics:
- `acquire_texture` OR `present` should block ~8.33ms (at 120Hz)
- Frame intervals should be consistent ~8.33ms
- Jitter should be <3ms standard deviation

## Files to Modify

1. `window/src/os/windows/window.rs` - Disable timer throttle for Fifo mode
2. `window/src/lib.rs` or `window/src/connection.rs` - Pass present mode config
3. `config/src/config.rs` - Ensure present mode is accessible

## Success Criteria
- Frame interval standard deviation < 3ms
- P95 jitter < 5ms
- Visually smooth scrolling comparable to Windows Terminal
