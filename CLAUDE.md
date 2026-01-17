# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
# Install system dependencies (auto-detects OS: Debian, Fedora, Arch, Alpine, etc.)
./get-deps

# Build main binaries
cargo build -p wezterm -p wezterm-gui -p wezterm-mux-server

# Type-check without building (fastest for iteration)
cargo check

# Run tests
cargo nextest run                           # All tests (preferred)
cargo test --all                            # Alternative
cargo nextest run -p <crate>                # Single crate

# Format code (uses nightly rustfmt)
cargo +nightly fmt

# Build and serve documentation locally
ci/build-docs.sh serve
```

## Architecture Overview

WezTerm is a GPU-accelerated terminal emulator with multiplexing. The codebase is organized as a Cargo workspace with ~46 crates.

### Core Layers

**Terminal Emulation (`term/`)**
- Pure terminal model, no windowing system dependency
- VT escape sequence parsing, screen cells, scrollback
- xterm-compatible behavior (reference: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)

**Multiplexer (`mux/`)**
- tmux-like functionality: panes, tabs, windows
- Domain abstraction (local, SSH, serial)
- Lua scripting integration via `mux-lua`

**GUI (`wezterm-gui/`)**
- GPU-accelerated rendering via wgpu with OpenGL fallback
- Font rendering through `wezterm-font` (HarfBuzz shaping, FreeType loading)
- Platform windowing via `window/` crate

**CLI (`wezterm/`)**
- Command-line interface using clap
- Client for connecting to mux server

**Server (`wezterm-mux-server/`)**
- Multiplexer server for remote connections

### Platform Abstraction (`window/`)

- Windows: WinAPI + DWM
- macOS: Cocoa/Core Graphics
- Linux X11: XCB + xkbcommon
- Wayland: smithay-client-toolkit (optional feature)

### Lua Scripting

Configuration uses Lua 5.4 (vendored via mlua). Domain-specific Lua APIs are in `lua-api-crates/`:
- `battery`, `color-funcs`, `filesystem`, `logging`, `mux`, `plugin`
- `procinfo-funcs`, `serde-funcs`, `share-data`, `spawn-funcs`
- `ssh-funcs`, `termwiz-funcs`, `time-funcs`, `url-funcs`, `window-funcs`

### Vendored Dependencies (`deps/`)

Cairo, Fontconfig, FreeType, and HarfBuzz are vendored for predictable builds.

### Key Supporting Crates

- `termwiz/` - TUI widgets and terminal utilities
- `config/` - Configuration system
- `portable-pty/` (at `pty/`) - PTY abstraction layer
- `bidi/` - Bidirectional text support
- `codec/` - Encoding/decoding utilities

### no_std Crates

Some crates support `no_std`: `wezterm-escape-parser`, `wezterm-cell`, `wezterm-surface`. Check these with their default features disabled.

## Code Style

- Edition 2018 (some crates use 2021)
- 4-space indentation
- Module-level import granularity
- Format with nightly rustfmt before submitting PRs

## Windows Development

### Build and Deploy

```powershell
# Build release binary (for testing)
cargo build --release -p wezterm-gui

# The binary is at: target\release\wezterm-gui.exe
# Test by running directly - no installation needed
```

### Path Handling

When writing paths in PowerShell scripts or commands:

- **In PowerShell scripts**: Use Windows-style paths with backslashes: `S:\projects\wezterm`
- **In cargo/rust code**: Use forward slashes or raw strings: `r"S:\projects\wezterm"`
- **In PowerShell strings with escapes**: Backslash doesn't need escaping, but use single quotes for literal strings

```powershell
# Correct PowerShell paths
$path = "S:\projects\wezterm\target\release\wezterm-gui.exe"
$path = 'S:\projects\wezterm\target\release\wezterm-gui.exe'

# Calling executables
Start-Process -FilePath "S:\projects\wezterm\target\release\wezterm-gui.exe"
```

### Frame Timing Benchmark

A PowerShell script measures scrolling smoothness across terminals using Intel PresentMon:

```powershell
# Prerequisites
winget install Intel.PresentMon

# Run benchmark (tests WezTerm, Alacritty, Windows Terminal)
.\benchmark_terminals.ps1

# Results saved to benchmark_results\<timestamp>\
```

**What it measures:**
- Frame timing (stddev, P50/P95/P99, max)
- Stutters (frames > 2x average)
- CPU usage during scrolling

**Key files:**
- `benchmark_terminals.ps1` - Main benchmark script
- `benchmark_scroll.cmd` - Infinite scroll generator for consistent load

### WezTerm Configuration for Testing

Edit `~\.wezterm.lua` (e.g., `C:\Users\<name>\.wezterm.lua`):

```lua
config.front_end = "WebGpu"
config.max_fps = 120

-- Present modes:
-- "Fifo" - Vsync, smooth but ~48ms latency
-- "Mailbox" - Low latency (~18ms), higher CPU without pacing
-- "Immediate" - Lowest latency, highest CPU
config.webgpu_present_mode = "Mailbox"
config.webgpu_max_frame_latency = 1
```

### Key Rendering Code Locations

- **Frame pacing/vsync**: `wezterm-gui/src/termwindow/render/draw.rs`
  - `dwm_vsync` module: DWM timing info for vsync alignment
  - `call_draw_webgpu()`: WebGPU rendering with present mode handling
- **Present mode config**: `config/src/frontend.rs` (`WebGpuPresentMode` enum)
- **Timer throttle bypass**: `window/src/os/windows/window.rs`

### Scroll Smoothness Investigation

See `SCROLL_SMOOTHNESS_INVESTIGATION.md` for detailed findings on achieving:
- 15-18ms latency
- <1ms frame time stddev
- <20% CPU usage

**TL;DR:** wgpu's Fifo mode doesn't block for vsync on Windows (DWM handles composition). Solutions:
1. `DwmFlush()` after present - smooth but adds latency
2. Mailbox with frame pacing - sleep after present until near next vsync
