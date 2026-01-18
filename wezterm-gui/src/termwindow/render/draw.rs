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
#[cfg(windows)]
use std::time::Duration;
use std::time::Instant;

/// Windows-specific frame pacing utilities.
///
/// Provides precise sleeping without permanently affecting the system-wide timer resolution.
/// Uses high-resolution waitable timers on Windows 10 1803+ (per-process, no system impact),
/// falling back to a hybrid sleep+spin approach on older systems.
#[cfg(windows)]
mod frame_pacing {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    // Flag for CREATE_WAITABLE_TIMER_HIGH_RESOLUTION (Windows 10 1803+)
    const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x00000002;

    /// Cached result of whether high-resolution timers are available
    static HIGH_RES_AVAILABLE: AtomicBool = AtomicBool::new(false);
    static HIGH_RES_CHECKED: AtomicBool = AtomicBool::new(false);

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

    /// Check if high-resolution waitable timers are supported.
    /// This is a Windows 10 1803+ feature that allows precise timing
    /// without affecting the system-wide timer resolution.
    fn is_high_res_timer_available() -> bool {
        if HIGH_RES_CHECKED.load(Ordering::Relaxed) {
            return HIGH_RES_AVAILABLE.load(Ordering::Relaxed);
        }

        let available = unsafe {
            // Try to create a high-resolution timer
            let handle = winapi::um::synchapi::CreateWaitableTimerExW(
                std::ptr::null_mut(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                winapi::um::winnt::TIMER_ALL_ACCESS,
            );

            if handle.is_null() {
                false
            } else {
                winapi::um::handleapi::CloseHandle(handle);
                true
            }
        };

        HIGH_RES_AVAILABLE.store(available, Ordering::Relaxed);
        HIGH_RES_CHECKED.store(true, Ordering::Relaxed);

        if available {
            log::debug!("High-resolution waitable timers available (Windows 10 1803+)");
        } else {
            log::debug!("High-resolution timers not available, will use fallback");
        }

        available
    }

    /// Sleep using a high-resolution waitable timer.
    /// Returns true if successful, false if we should fall back.
    fn sleep_high_res(duration: Duration) -> bool {
        unsafe {
            let handle = winapi::um::synchapi::CreateWaitableTimerExW(
                std::ptr::null_mut(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                winapi::um::winnt::TIMER_ALL_ACCESS,
            );

            if handle.is_null() {
                return false;
            }

            // Convert duration to 100-nanosecond intervals (negative = relative time)
            let due_time = -((duration.as_nanos() / 100) as i64);

            let set_result = winapi::um::synchapi::SetWaitableTimer(
                handle,
                &due_time as *const i64 as *const _,
                0,                    // no period (one-shot)
                None,                 // no completion routine
                std::ptr::null_mut(), // no completion arg
                0,                    // don't resume from suspend
            );

            if set_result == 0 {
                winapi::um::handleapi::CloseHandle(handle);
                return false;
            }

            // Wait for the timer
            winapi::um::synchapi::WaitForSingleObject(
                handle,
                winapi::um::winbase::INFINITE,
            );

            winapi::um::handleapi::CloseHandle(handle);
            true
        }
    }

    /// Sleep for the specified duration with high precision.
    ///
    /// This function provides precise timing while minimizing system-wide impact:
    ///
    /// 1. **Windows 10 1803+**: Uses high-resolution waitable timers, which are
    ///    per-process and don't affect the global timer interrupt frequency.
    ///
    /// 2. **Older Windows**: Falls back to a hybrid approach:
    ///    - Temporarily elevates timer resolution to 1ms (only during the sleep)
    ///    - Sleeps for most of the duration
    ///    - Spin-waits for the final portion for precision
    ///
    /// The spin-wait portion is limited to avoid excessive CPU usage.
    pub fn precise_sleep(duration: Duration) {
        // Don't bother with very short sleeps
        if duration < Duration::from_micros(100) {
            return;
        }

        // Try high-resolution timer first (best option - no system impact)
        if is_high_res_timer_available() && sleep_high_res(duration) {
            return;
        }

        // Fallback: hybrid sleep + spin approach
        // This temporarily affects system timer resolution, but only during active sleep
        let spin_threshold = Duration::from_millis(2);
        let deadline = Instant::now() + duration;

        if duration > spin_threshold {
            // Only elevate timer resolution during this sleep call
            let _guard = TimerResolutionGuard::acquire();
            let sleep_duration = duration - spin_threshold;
            std::thread::sleep(sleep_duration);
        }

        // Spin-wait for the remaining time (precise but CPU-intensive)
        // Cap spin time to avoid runaway CPU usage if something goes wrong
        let max_spin = Duration::from_millis(3);
        let spin_start = Instant::now();

        while Instant::now() < deadline {
            if spin_start.elapsed() > max_spin {
                // Something's wrong, bail out to avoid burning CPU forever
                log::trace!("precise_sleep: spin limit exceeded, breaking");
                break;
            }
            std::hint::spin_loop();
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

        let target_frame_time = Duration::from_secs_f64(1.0 / target_fps as f64);
        let elapsed = frame_start.elapsed();

        if elapsed >= target_frame_time {
            // Already past deadline, no sleep needed
            return Duration::ZERO;
        }

        let remaining = target_frame_time - elapsed;

        // Adaptive safety buffer: 12% of frame time, clamped to reasonable bounds
        // 60Hz (16.7ms): 2.0ms buffer
        // 120Hz (8.3ms): 1.0ms buffer
        // 240Hz (4.2ms): 0.5ms buffer (clamped to minimum)
        let buffer_ratio = 0.12;
        let min_buffer = Duration::from_micros(500);
        let max_buffer = Duration::from_millis(3);
        let safety_buffer = Duration::from_secs_f64(target_frame_time.as_secs_f64() * buffer_ratio)
            .clamp(min_buffer, max_buffer);

        let sleep_duration = remaining.saturating_sub(safety_buffer);

        if sleep_duration > Duration::from_micros(100) {
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

        // Frame pacing for Windows to reduce CPU usage while maintaining low latency.
        #[cfg(windows)]
        {
            use wgpu::PresentMode;

            // Capture frame boundary IMMEDIATELY after present - this is critical
            // for accurate frame timing. We measure from present-to-present, not
            // including our own sleep time.
            let frame_presented_at = Instant::now();

            let present_mode = webgpu.config.borrow().present_mode;
            let max_fps = self.config.max_fps;

            // For FIFO mode, use DwmFlush to sync with the compositor.
            // However, skip it if present() already blocked significantly,
            // which indicates the driver is handling vsync (varies by vendor).
            // Note: We only check for Fifo since our config doesn't expose FifoRelaxed.
            if matches!(present_mode, PresentMode::Fifo) {
                // If present took >4ms, driver likely already synced with vsync
                let driver_likely_synced = present_duration > Duration::from_millis(4);

                if !driver_likely_synced {
                    let dwm_start = Instant::now();
                    unsafe {
                        winapi::um::dwmapi::DwmFlush();
                    }
                    let dwm_duration = dwm_start.elapsed();
                    metrics::histogram!("gui.frame.dwm_flush").record(dwm_duration);
                    log::trace!("DwmFlush took {:?}", dwm_duration);
                } else {
                    log::trace!(
                        "Skipping DwmFlush, present already blocked for {:?}",
                        present_duration
                    );
                }
            }

            // Honor max_fps using precise sleep that minimizes system-wide impact.
            // Uses high-resolution waitable timers on Windows 10 1803+, falling
            // back to hybrid sleep+spin on older systems.
            if max_fps > 0 {
                let sleep_duration = frame_pacing::pace_frame(self.last_frame_instant, max_fps);
                if sleep_duration > Duration::ZERO {
                    metrics::histogram!("gui.frame.pacing_sleep").record(sleep_duration);
                }
            }

            // Update frame timing reference point for next iteration
            self.last_frame_instant = frame_presented_at;
        }

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

        Ok(())
    }
}
