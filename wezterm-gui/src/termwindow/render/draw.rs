use crate::colorease::ColorEaseUniform;
use crate::termwindow::webgpu::ShaderUniform;
use crate::termwindow::RenderFrame;
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use config::FreeTypeLoadTarget;
use std::time::{Duration, Instant};

/// Frame pacing constants - centralized for easy tuning and documentation.
mod pacing_constants {
    use std::time::Duration;

    /// Minimum sleep duration worth attempting (below this, overhead exceeds benefit)
    pub const MIN_SLEEP_THRESHOLD: Duration = Duration::from_micros(100);

    /// Adaptive safety buffer ratio (percentage of frame time)
    /// Provides headroom for timer imprecision and scheduling jitter
    pub const BUFFER_RATIO: f64 = 0.12;

    /// Minimum safety buffer (for high refresh rates like 240Hz+)
    pub const MIN_BUFFER: Duration = Duration::from_micros(500);

    /// Maximum safety buffer (for low refresh rates like 30Hz)
    pub const MAX_BUFFER: Duration = Duration::from_millis(3);

    /// Maximum duration we'll attempt to sleep (sanity cap to prevent overflow issues)
    /// Any frame time longer than this is unreasonable
    pub const MAX_SLEEP_DURATION: Duration = Duration::from_secs(1);

    /// Threshold for spin-waiting in fallback mode (Windows legacy)
    pub const SPIN_THRESHOLD: Duration = Duration::from_millis(2);

    /// Maximum spin time to prevent runaway CPU usage
    pub const MAX_SPIN_TIME: Duration = Duration::from_millis(3);

    /// Threshold to determine if GPU driver already synced with vsync
    /// If present() took longer than this, skip DwmFlush
    pub const DRIVER_VSYNC_THRESHOLD: Duration = Duration::from_millis(4);
}

/// Cross-platform frame pacing utilities.
///
/// Provides precise sleeping for frame rate limiting. Platform-specific implementations
/// optimize for precision while minimizing system impact.
mod frame_pacing {
    use super::pacing_constants::*;
    use std::time::{Duration, Instant};

    /// Sleep for the specified duration with reasonable precision.
    ///
    /// On Windows 10 1803+, uses high-resolution waitable timers (no system-wide impact).
    /// On older Windows, uses hybrid sleep+spin with temporary timer resolution elevation.
    /// On other platforms, uses standard thread sleep.
    pub fn precise_sleep(duration: Duration) {
        // Don't bother with very short sleeps
        if duration < MIN_SLEEP_THRESHOLD {
            return;
        }

        // Cap duration to prevent overflow and unreasonable waits
        let duration = duration.min(MAX_SLEEP_DURATION);

        #[cfg(windows)]
        {
            windows_precise_sleep(duration);
        }

        #[cfg(not(windows))]
        {
            // On non-Windows platforms, use standard sleep
            // This is less precise but avoids platform-specific complexity
            std::thread::sleep(duration);
        }
    }

    /// Sleep until the target frame time has elapsed since `frame_start`.
    /// Uses adaptive safety buffer based on refresh rate.
    ///
    /// Returns the actual time slept (for metrics).
    pub fn pace_frame(frame_start: Instant, target_fps: u64) -> Duration {
        if target_fps == 0 {
            return Duration::ZERO;
        }

        // Cap FPS to reasonable bounds to prevent precision issues
        // At 10000 FPS, frame time is 0.1ms which is at the edge of timer precision
        let target_fps = target_fps.min(10000);

        let target_frame_time = Duration::from_secs_f64(1.0 / target_fps as f64);
        let elapsed = frame_start.elapsed();

        if elapsed >= target_frame_time {
            // Already past deadline, no sleep needed
            return Duration::ZERO;
        }

        let remaining = target_frame_time - elapsed;

        // Adaptive safety buffer based on frame time
        let safety_buffer =
            Duration::from_secs_f64(target_frame_time.as_secs_f64() * BUFFER_RATIO)
                .clamp(MIN_BUFFER, MAX_BUFFER);

        let sleep_duration = remaining.saturating_sub(safety_buffer);

        if sleep_duration > MIN_SLEEP_THRESHOLD {
            let sleep_start = Instant::now();
            precise_sleep(sleep_duration);
            let actual_sleep = sleep_start.elapsed();
            log::trace!(
                "pace_frame: target={:?}, elapsed={:?}, slept={:?}",
                target_frame_time,
                elapsed,
                actual_sleep
            );
            return actual_sleep;
        }

        Duration::ZERO
    }

    // ============ Windows-specific implementation ============

    #[cfg(windows)]
    mod windows_impl {
        use super::*;
        use std::cell::RefCell;
        use std::sync::OnceLock;

        /// Flag for CREATE_WAITABLE_TIMER_HIGH_RESOLUTION (Windows 10 1803+)
        const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x00000002;

        /// Result of checking for high-resolution timer support
        #[derive(Clone, Copy, Debug)]
        enum HighResTimerSupport {
            Available,
            NotAvailable,
        }

        /// Cached check for high-resolution timer availability (thread-safe, checked once)
        static HIGH_RES_SUPPORT: OnceLock<HighResTimerSupport> = OnceLock::new();

        // Thread-local cached timer handle to avoid creating/destroying handles per-frame
        thread_local! {
            static CACHED_TIMER: RefCell<Option<TimerHandle>> = const { RefCell::new(None) };
        }

        /// RAII wrapper for a Windows waitable timer handle
        struct TimerHandle {
            handle: winapi::shared::ntdef::HANDLE,
        }

        impl TimerHandle {
            /// Create a new high-resolution timer handle, or None if not supported
            fn new_high_res() -> Option<Self> {
                let handle = unsafe {
                    winapi::um::synchapi::CreateWaitableTimerExW(
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                        winapi::um::winnt::TIMER_ALL_ACCESS,
                    )
                };

                if handle.is_null() {
                    None
                } else {
                    Some(Self { handle })
                }
            }

            /// Sleep for the specified duration using this timer
            fn sleep(&self, duration: Duration) -> bool {
                // Convert duration to 100-nanosecond intervals (negative = relative time)
                // Use saturating conversion to prevent overflow
                let nanos_100 = duration.as_nanos().min(i64::MAX as u128) / 100;
                let due_time = -(nanos_100 as i64);

                unsafe {
                    let set_result = winapi::um::synchapi::SetWaitableTimer(
                        self.handle,
                        &due_time as *const i64 as *const _,
                        0,                    // no period (one-shot)
                        None,                 // no completion routine
                        std::ptr::null_mut(), // no completion arg
                        0,                    // don't resume from suspend
                    );

                    if set_result == 0 {
                        return false;
                    }

                    winapi::um::synchapi::WaitForSingleObject(
                        self.handle,
                        winapi::um::winbase::INFINITE,
                    );
                }

                true
            }
        }

        impl Drop for TimerHandle {
            fn drop(&mut self) {
                unsafe {
                    winapi::um::handleapi::CloseHandle(self.handle);
                }
            }
        }

        /// RAII guard for temporary timer resolution elevation.
        /// Only used as fallback when high-res timers aren't available.
        struct TimerResolutionGuard;

        impl TimerResolutionGuard {
            fn acquire() -> Self {
                unsafe {
                    winapi::um::timeapi::timeBeginPeriod(1);
                }
                Self
            }
        }

        impl Drop for TimerResolutionGuard {
            fn drop(&mut self) {
                unsafe {
                    winapi::um::timeapi::timeEndPeriod(1);
                }
            }
        }

        /// Check if high-resolution waitable timers are supported (cached, thread-safe)
        fn get_high_res_support() -> HighResTimerSupport {
            *HIGH_RES_SUPPORT.get_or_init(|| {
                // Try to create a high-resolution timer to test support
                match TimerHandle::new_high_res() {
                    Some(_handle) => {
                        // Handle is dropped here, we just needed to test
                        log::debug!("High-resolution waitable timers available (Windows 10 1803+)");
                        HighResTimerSupport::Available
                    }
                    None => {
                        log::debug!(
                            "High-resolution timers not available, will use fallback timing"
                        );
                        HighResTimerSupport::NotAvailable
                    }
                }
            })
        }

        /// Get or create the thread-local cached timer handle
        fn with_cached_timer<R>(f: impl FnOnce(&TimerHandle) -> R) -> Option<R> {
            CACHED_TIMER.with(|cell| {
                let mut opt = cell.borrow_mut();

                // Lazily create the timer on first use
                if opt.is_none() {
                    *opt = TimerHandle::new_high_res();
                }

                opt.as_ref().map(f)
            })
        }

        /// Windows-specific precise sleep implementation
        pub fn windows_precise_sleep(duration: Duration) {
            // Try high-resolution timer first (best option - no system impact)
            if matches!(get_high_res_support(), HighResTimerSupport::Available) {
                if let Some(true) = with_cached_timer(|timer| timer.sleep(duration)) {
                    return;
                }
                // If cached timer failed, fall through to legacy path
            }

            // Fallback: hybrid sleep + spin approach
            // This temporarily affects system timer resolution, but only during active sleep
            let deadline = Instant::now() + duration;

            if duration > SPIN_THRESHOLD {
                // Only elevate timer resolution during this sleep call
                let _guard = TimerResolutionGuard::acquire();
                let sleep_duration = duration - SPIN_THRESHOLD;
                std::thread::sleep(sleep_duration);
            }

            // Spin-wait for the remaining time (precise but CPU-intensive)
            // Cap spin time to avoid runaway CPU usage if something goes wrong
            let spin_start = Instant::now();

            while Instant::now() < deadline {
                if spin_start.elapsed() > MAX_SPIN_TIME {
                    // Something's wrong, bail out to avoid burning CPU forever
                    log::trace!("precise_sleep: spin limit exceeded, breaking");
                    break;
                }
                std::hint::spin_loop();
            }
        }
    }

    #[cfg(windows)]
    use windows_impl::windows_precise_sleep;
}

impl crate::TermWindow {
    pub fn call_draw(&mut self, frame: &mut RenderFrame) -> anyhow::Result<()> {
        match frame {
            RenderFrame::Glium(ref mut frame) => self.call_draw_glium(frame),
            RenderFrame::WebGpu => self.call_draw_webgpu(),
        }
    }

    fn call_draw_webgpu(&mut self) -> anyhow::Result<()> {
        use crate::termwindow::webgpu::WebGpuTexture;

        let webgpu = self.webgpu.as_mut().unwrap();
        let render_state = self.render_state.as_ref().unwrap();

        // Measure time to acquire the swapchain texture
        // This can block waiting for vsync in Fifo mode
        let acquire_start = Instant::now();
        let output = webgpu.surface.get_current_texture()?;
        let acquire_duration = acquire_start.elapsed();
        metrics::histogram!("gui.frame.acquire_texture").record(acquire_duration);
        log::trace!("acquire_texture took {:?}", acquire_duration);
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = webgpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });
        let tex = render_state.glyph_cache.borrow().atlas.texture();
        let tex = tex.downcast_ref::<WebGpuTexture>().unwrap();
        let texture_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let texture_linear_bind_group =
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&webgpu.texture_linear_sampler),
                    },
                ],
                label: Some("linear bind group"),
            });

        let texture_nearest_bind_group =
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&webgpu.texture_nearest_sampler),
                    },
                ],
                label: Some("nearest bind group"),
            });

        let mut cleared = false;
        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = [
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        ];

        let milliseconds = self.created.elapsed().as_millis() as u32;
        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        for layer in render_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let (vertex_count, index_count) = vb.vertex_index_count();
                let vertex_buffer;
                let uniforms;
                if vertex_count > 0 {
                    let mut vertices = vb.current_vb_mut();
                    let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("Render Pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: if cleared {
                                    wgpu::LoadOp::Load
                                } else {
                                    wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.,
                                        g: 0.,
                                        b: 0.,
                                        a: 0.,
                                    })
                                },
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        occlusion_query_set: None,
                        timestamp_writes: None,
                    });
                    cleared = true;

                    uniforms = webgpu.create_uniform(ShaderUniform {
                        foreground_text_hsb,
                        milliseconds,
                        projection,
                    });

                    render_pass.set_pipeline(&webgpu.render_pipeline);
                    render_pass.set_bind_group(0, &uniforms, &[]);
                    render_pass.set_bind_group(1, &texture_linear_bind_group, &[]);
                    render_pass.set_bind_group(2, &texture_nearest_bind_group, &[]);
                    vertex_buffer = vertices.webgpu_mut().recreate();
                    vertex_buffer.unmap();
                    render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                    render_pass
                        .set_index_buffer(vb.indices.webgpu().slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..index_count as _, 0, 0..1);
                }

                vb.next_index();
            }
        }

        // submit will accept anything that implements IntoIter
        webgpu.queue.submit(std::iter::once(encoder.finish()));

        // Measure time spent in present()
        let present_start = Instant::now();
        output.present();
        let present_duration = present_start.elapsed();
        metrics::histogram!("gui.frame.present_wait").record(present_duration);
        log::trace!("present took {:?}", present_duration);

        // Capture frame boundary IMMEDIATELY after present - this is critical
        // for accurate frame timing. We measure from present-to-present, not
        // including our own sleep time.
        let frame_presented_at = Instant::now();

        // Windows-specific vsync handling with DWM
        #[cfg(windows)]
        {
            use wgpu::PresentMode;

            let present_mode = webgpu.config.borrow().present_mode;

            // For FIFO mode, use DwmFlush to sync with the compositor.
            // However, skip it if present() already blocked significantly,
            // which indicates the driver is handling vsync (varies by vendor).
            if matches!(present_mode, PresentMode::Fifo) {
                let driver_likely_synced =
                    present_duration > pacing_constants::DRIVER_VSYNC_THRESHOLD;

                if !driver_likely_synced {
                    let dwm_start = Instant::now();
                    let hr = unsafe { winapi::um::dwmapi::DwmFlush() };
                    let dwm_duration = dwm_start.elapsed();

                    if hr < 0 {
                        // DwmFlush failed - log once and continue
                        // This can happen if DWM is disabled or in some RDP scenarios
                        log::debug!("DwmFlush failed with HRESULT: 0x{:08X}", hr as u32);
                    } else {
                        metrics::histogram!("gui.frame.dwm_flush").record(dwm_duration);
                        log::trace!("DwmFlush took {:?}", dwm_duration);
                    }
                } else {
                    log::trace!(
                        "Skipping DwmFlush, present already blocked for {:?}",
                        present_duration
                    );
                }
            }
        }

        // Cross-platform frame pacing to honor max_fps
        let max_fps = self.config.max_fps;
        if max_fps > 0 {
            let sleep_duration = frame_pacing::pace_frame(self.last_frame_instant, max_fps);
            if sleep_duration > Duration::ZERO {
                metrics::histogram!("gui.frame.pacing_sleep").record(sleep_duration);
            }
        }

        // Update frame timing reference point for next iteration
        self.last_frame_instant = frame_presented_at;

        Ok(())
    }

    fn call_draw_glium(&mut self, frame: &mut glium::Frame) -> anyhow::Result<()> {
        use window::glium::texture::SrgbTexture2d;

        let gl_state = self.render_state.as_ref().unwrap();
        let tex = gl_state.glyph_cache.borrow().atlas.texture();
        let tex = tex.downcast_ref::<SrgbTexture2d>().unwrap();

        frame.clear_color(0., 0., 0., 0.);

        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        let use_subpixel = match self
            .config
            .freetype_render_target
            .unwrap_or(self.config.freetype_load_target)
        {
            FreeTypeLoadTarget::HorizontalLcd | FreeTypeLoadTarget::VerticalLcd => true,
            _ => false,
        };

        let dual_source_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },

            ..Default::default()
        };

        let alpha_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceAlpha,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::One,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },
            ..Default::default()
        };

        // Clamp and use the nearest texel rather than interpolate.
        // This prevents things like the box cursor outlines from
        // being randomly doubled in width or height
        let atlas_nearest_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest);

        let atlas_linear_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Linear)
            .minify_filter(MinifySamplerFilter::Linear);

        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = (
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        );

        let milliseconds = self.created.elapsed().as_millis() as u32;

        let cursor_blink: ColorEaseUniform = (*self.cursor_blink_state.borrow()).into();
        let blink: ColorEaseUniform = (*self.blink_state.borrow()).into();
        let rapid_blink: ColorEaseUniform = (*self.rapid_blink_state.borrow()).into();

        for layer in gl_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let (vertex_count, index_count) = vb.vertex_index_count();
                if vertex_count > 0 {
                    let vertices = vb.current_vb_mut();
                    let subpixel_aa = use_subpixel && idx == 1;

                    let mut uniforms = UniformBuilder::default();

                    uniforms.add("projection", &projection);
                    uniforms.add("atlas_nearest_sampler", &atlas_nearest_sampler);
                    uniforms.add("atlas_linear_sampler", &atlas_linear_sampler);
                    uniforms.add("foreground_text_hsb", &foreground_text_hsb);
                    uniforms.add("subpixel_aa", &subpixel_aa);
                    uniforms.add("milliseconds", &milliseconds);
                    uniforms.add_struct("cursor_blink", &cursor_blink);
                    uniforms.add_struct("blink", &blink);
                    uniforms.add_struct("rapid_blink", &rapid_blink);

                    frame.draw(
                        vertices.glium().slice(0..vertex_count).unwrap(),
                        vb.indices.glium().slice(0..index_count).unwrap(),
                        gl_state.glyph_prog.as_ref().unwrap(),
                        &uniforms,
                        if subpixel_aa {
                            &dual_source_blending
                        } else {
                            &alpha_blending
                        },
                    )?;
                }

                vb.next_index();
            }
        }

        // OpenGL frame pacing - honor max_fps to prevent spinning
        // Note: OpenGL typically handles vsync internally, so we just need max_fps limiting
        let frame_presented_at = Instant::now();
        let max_fps = self.config.max_fps;
        if max_fps > 0 {
            let sleep_duration = frame_pacing::pace_frame(self.last_frame_instant, max_fps);
            if sleep_duration > Duration::ZERO {
                metrics::histogram!("gui.frame.pacing_sleep").record(sleep_duration);
            }
        }
        self.last_frame_instant = frame_presented_at;

        Ok(())
    }
}
