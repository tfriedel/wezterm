# WezTerm Scroll Smoothness Investigation

## Goal

Achieve **all four** of these simultaneously:
- **Latency**: 15-18ms (non-negotiable)
- **Smoothness**: Low stddev (<1ms), zero stutters
- **Frame pacing**: Consistent vsync-aligned frames
- **CPU efficiency**: <20% during scrolling

## Summary of Investigation

### The Problem

WezTerm on Windows 11 exhibited stuttery scrolling compared to Windows Terminal and Alacritty. Investigation revealed that wgpu's `Fifo` (vsync) mode doesn't properly block for vsync on Windows because DWM (Desktop Window Manager) handles composition.

### What We Tried

#### 1. Fifo + DwmFlush (smooth but high latency)
Added `DwmFlush()` call after `present()` to sync with DWM compositor.

**Results:**
- Smoothness: Excellent (0.03ms stddev)
- Latency: **48ms** (unacceptable - adds ~1 frame)
- CPU: 14.8%

**Code location:** `wezterm-gui/src/termwindow/render/draw.rs:260-268`

```rust
#[cfg(windows)]
if self.config.webgpu_present_mode == WebGpuPresentMode::Fifo {
    unsafe {
        winapi::um::dwmapi::DwmFlush();
    }
}
```

**Why it adds latency:** DwmFlush waits AFTER present for the frame to be displayed, adding one full frame of latency.

#### 2. Pre-present vsync timing with DwmGetCompositionTimingInfo (failed)
Attempted to calculate when next vsync occurs and sleep until just before it, then present.

**Results:** Made latency worse (72ms) due to:
- Windows sleep() imprecision (~15.6ms default timer resolution)
- Timing calculation placed before GPU submit, delaying everything

**Code exists but unused:** `dwm_vsync` module in `draw.rs`

#### 3. Mailbox mode (current best for low latency)
Let wgpu run in Mailbox mode - presents immediately, DWM picks up latest frame at its vsync.

**Results:**
- Smoothness: Good (0.98ms stddev)
- Latency: **18ms** ✓
- CPU: **38.5%** (too high)

### Current Best Configurations

| Config | Latency | StdDev | CPU | Verdict |
|--------|---------|--------|-----|---------|
| Fifo + DwmFlush | 48ms | 0.03ms | 14.8% | ❌ Latency too high |
| Mailbox | 18ms | 0.98ms | 38.5% | ⚠️ CPU too high |
| **Target** | **15-18ms** | **<1ms** | **<20%** | 🎯 |

## Benchmark Results Comparison

### All Terminals Tested

| Terminal | Mode | StdDev | P99 | Max | Stutters | CPU |
|----------|------|--------|-----|-----|----------|-----|
| WezTerm | Fifo+DwmFlush | **0.03ms** | 16.75ms | 16.78ms | 0 | 14.8% |
| WezTerm | Mailbox | 0.98ms | 18.11ms | 31.89ms | 0 | 38.5% |
| Windows Terminal | Normal | 0.16ms | 17.09ms | 17.82ms | 0 | **2.2%** |
| Windows Terminal | Low Latency | 0.36ms | 0.99ms | 36.13ms | **68** | 14.3% |
| Alacritty | Default | 3.54ms | 28.95ms | 41.15ms | 2 | 17.1% |

### Key Observations

1. **Windows Terminal normal mode** achieves excellent smoothness (0.16ms stddev) with minimal CPU (2.2%)
2. **Windows Terminal Low Latency mode** causes many stutters (68) - trading smoothness for latency
3. **WezTerm Mailbox** uses 38.5% CPU because it renders uncapped frames for GPU to pick from
4. **Alacritty** has unexplained multi-second spikes occasionally

## Ideas for Achieving the Goal

### Approach 1: Smarter Mailbox with Frame Pacing

The problem with Mailbox is it renders as fast as possible, wasting CPU. Instead:

1. Use Mailbox mode (for low latency)
2. After present(), calculate time until next vsync using `DwmGetCompositionTimingInfo`
3. If next frame would arrive before vsync, sleep briefly instead of rendering immediately
4. This gives vsync-aligned rendering without the DwmFlush latency penalty

```rust
// Pseudocode
present();
let time_to_vsync = get_time_until_next_vsync();
if time_to_vsync > frame_render_time {
    sleep(time_to_vsync - frame_render_time - margin);
}
```

### Approach 2: Adaptive Present Mode

Switch between modes based on activity:
- During scrolling/animation: Use Mailbox for low latency
- When idle: Use Fifo for power efficiency

### Approach 3: Use DirectComposition/DComp

Windows Terminal likely uses DirectComposition for its excellent results. This provides:
- Direct integration with DWM
- Proper vsync without DwmFlush overhead
- Hardware-accelerated composition

### Approach 4: Fix the Pre-present Timing

The earlier attempt failed due to:
1. Sleep placed before GPU submit
2. Imprecise Windows timers

To fix:
1. Call `timeBeginPeriod(1)` to get 1ms timer resolution
2. Submit GPU work first
3. Use spin-wait for final <2ms instead of sleep
4. Present just before vsync

```rust
// Corrected approach
queue.submit(commands);  // GPU work first

let time_to_vsync = get_time_until_next_vsync();
if time_to_vsync > 2ms {
    sleep(time_to_vsync - 2ms);  // Coarse sleep
}
while get_time_until_next_vsync() > 100us {
    spin_loop_hint();  // Precise spin-wait
}

output.present();  // Right before vsync
// NO DwmFlush - we're already at vsync
```

## Running the Benchmark

### Prerequisites

```powershell
# Install PresentMon
winget install Intel.PresentMon
```

### Quick Test

```powershell
cd S:\projects\wezterm
.\benchmark_terminals.ps1
```

### Configuration

Edit `benchmark_terminals.ps1` line ~50 to select terminals:
```powershell
$EnabledTerminals = @("WezTerm", "Alacritty", "WindowsTerminalDev")
```

Options: `"WezTerm"`, `"WindowsTerminal"`, `"WindowsTerminalDev"`, `"Alacritty"`

### Parameters

```powershell
.\benchmark_terminals.ps1 -Duration 10  # Recording duration in seconds
```

### What the Benchmark Does

1. Launches terminal with infinite scrolling text
2. Waits 5 seconds for warmup
3. Records frame timing with PresentMon for 10 seconds
4. Samples CPU usage every 200ms during recording
5. Kills terminal and analyzes results

### Output

Results saved to `benchmark_results\<timestamp>\`:
- `<Terminal>.csv` - Raw PresentMon data
- `summary.csv` - Comparison table

### Key Metrics

| Metric | Good | Bad | Why it matters |
|--------|------|-----|----------------|
| StdDev | <1ms | >5ms | Frame pacing consistency |
| P99 | <20ms | >30ms | Worst-case frame times |
| Stutters | 0 | >5 | Visible hitches |
| CPU | <20% | >30% | Power/thermal efficiency |

## Measuring Latency

The benchmark measures frame timing, not input latency. To measure latency:

```powershell
# Uses a separate tool - see benchmark script
# Latency = time from input to pixel change
# Target: 15-18ms
```

Manual method: Use high-speed camera or [Is It Snappy](https://isitsnappy.com/) with a 240Hz+ display.

## Files Modified

- `wezterm-gui/src/termwindow/render/draw.rs` - DwmFlush code, dwm_vsync module
- `wezterm-gui/Cargo.toml` - Added `dwmapi`, `profileapi` features to winapi
- `window/src/os/windows/window.rs` - Bypass timer throttle for Fifo mode
- `benchmark_terminals.ps1` - Benchmark script
- `benchmark_scroll.cmd` - Infinite scroll generator

## For Windows Terminal Team

Your "low latency" mode shows 68 stutters in our benchmark. Observations:

1. **Running uncapped causes stutters** - 15455 frames in 10s = ~1545 fps average, but with spikes to 36ms
2. **Normal mode is excellent** - 0.16ms stddev, 2.2% CPU, suggesting good DWM integration
3. **The tradeoff isn't necessary** - WezTerm Mailbox achieves 18ms latency with 0.98ms stddev

Consider:
- Frame pacing even in low latency mode (render at vsync rate, just don't wait after present)
- The stutters may be from CPU scheduling when rendering uncapped
- Your normal mode's approach might just need the latency reduced, not bypassed entirely
