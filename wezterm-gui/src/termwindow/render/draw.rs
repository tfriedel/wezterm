use crate::colorease::ColorEaseUniform;
use crate::termwindow::RenderFrame;
use crate::termwindow::webgpu::ShaderUniform;
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use config::{FreeTypeLoadTarget, WebGpuPresentMode};
use std::time::{Duration, Instant};

/// Vsync timing utilities for Windows DWM compositor
#[cfg(windows)]
mod dwm_vsync {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Track whether we've set the timer resolution
    static TIMER_RESOLUTION_SET: AtomicBool = AtomicBool::new(false);

    /// Set Windows timer resolution to 1ms for precise sleeping.
    /// This is a system-wide setting but ref-counted by Windows.
    /// Should be called once at startup.
    pub fn ensure_timer_resolution() {
        if !TIMER_RESOLUTION_SET.swap(true, Ordering::SeqCst) {
            unsafe {
                // Request 1ms timer resolution
                // This improves sleep() precision from ~15.6ms to ~1ms
                winapi::um::timeapi::timeBeginPeriod(1);
            }
            log::debug!("Set Windows timer resolution to 1ms for frame pacing");
        }
    }

    /// Get the time until the next vsync boundary using DWM timing info.
    /// Returns None if timing info couldn't be retrieved.
    ///
    /// This uses DwmGetCompositionTimingInfo to get:
    /// - qpcVBlank: the QPC timestamp of the last vsync
    /// - qpcRefreshPeriod: the refresh period in QPC ticks
    ///
    /// We calculate when the next vsync will occur and return the duration to wait.
    pub fn time_until_next_vsync() -> Option<Duration> {
        use std::mem::MaybeUninit;
        use winapi::shared::minwindef::FALSE;
        use winapi::um::dwmapi::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
        use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};

        unsafe {
            // Get QPC frequency for converting QPC ticks to time
            let mut frequency: i64 = 0;
            if QueryPerformanceFrequency(&mut frequency as *mut i64 as *mut _) == FALSE {
                return None;
            }

            // Get current QPC time
            let mut current_qpc: i64 = 0;
            if QueryPerformanceCounter(&mut current_qpc as *mut i64 as *mut _) == FALSE {
                return None;
            }

            // Get DWM timing info
            // Note: hwnd must be NULL on Windows 8.1+
            let mut timing_info = MaybeUninit::<DWM_TIMING_INFO>::zeroed();
            let timing_ptr = timing_info.as_mut_ptr();
            (*timing_ptr).cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;

            let result = DwmGetCompositionTimingInfo(std::ptr::null_mut(), timing_ptr);
            if result != 0 {
                // HRESULT failure
                return None;
            }

            let timing_info = timing_info.assume_init();
            let qpc_vblank = timing_info.qpcVBlank as i64;
            let qpc_refresh_period = timing_info.qpcRefreshPeriod as i64;

            if qpc_refresh_period <= 0 {
                return None;
            }

            // Calculate time since last vsync
            let time_since_vblank = current_qpc - qpc_vblank;

            // Calculate how many periods have elapsed since the recorded vblank
            let periods_elapsed = time_since_vblank / qpc_refresh_period;

            // Calculate the next vsync time
            let next_vsync = qpc_vblank + (periods_elapsed + 1) * qpc_refresh_period;

            // Time until next vsync in QPC ticks
            let ticks_until_vsync = next_vsync - current_qpc;

            if ticks_until_vsync <= 0 {
                // Already past, present immediately
                return Some(Duration::ZERO);
            }

            // Convert QPC ticks to Duration
            // Duration = ticks * (1 second / frequency)
            let nanos = (ticks_until_vsync as u128 * 1_000_000_000) / frequency as u128;
            Some(Duration::from_nanos(nanos as u64))
        }
    }

    /// Sleep with 1ms timer resolution (no spin-wait to avoid CPU burn).
    /// With timeBeginPeriod(1), Windows sleep is accurate to ~1-2ms.
    pub fn precise_sleep(duration: Duration) {
        if duration.is_zero() {
            return;
        }
        std::thread::sleep(duration);
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
        //
        // The key insight is WHERE we sleep matters for latency:
        // - DwmFlush() waits for the CURRENT frame to be displayed → adds latency
        // - Sleeping here delays starting the NEXT frame → doesn't add latency
        //
        // In Mailbox mode without pacing, we render as fast as possible (~1500 fps),
        // wasting CPU because DWM only picks up one frame per vsync anyway.
        //
        // With pacing: after present(), we calculate time until next vsync and sleep
        // for most of that time, leaving a margin to start rendering the next frame
        // so it's ready just in time for the following vsync.
        #[cfg(windows)]
        {
            match self.config.webgpu_present_mode {
                WebGpuPresentMode::Fifo => {
                    // Fifo mode: use DwmFlush for guaranteed vsync sync
                    // This adds ~1 frame of latency but ensures perfect frame pacing
                    let dwm_start = Instant::now();
                    unsafe {
                        winapi::um::dwmapi::DwmFlush();
                    }
                    let dwm_duration = dwm_start.elapsed();
                    metrics::histogram!("gui.frame.dwm_flush").record(dwm_duration);
                    log::trace!("DwmFlush took {:?}", dwm_duration);
                }
                WebGpuPresentMode::Mailbox | WebGpuPresentMode::AutoNoVsync => {
                    // Mailbox mode: sleep after present to limit frame rate and save CPU.
                    //
                    // This doesn't add input latency because:
                    // - Input arrives → paint happens immediately (no throttle)
                    // - Present completes → we sleep here
                    // - Next frame starts after sleep expires
                    //
                    // Fixed 6ms sleep balances CPU savings vs frame rate:
                    // - With ~2ms render time, cycle is ~8ms = ~125fps max
                    // - DWM picks up frames at vsync rate (60/120/144Hz)
                    // - Extra frames are discarded, but CPU is much lower than no sleep
                    dwm_vsync::ensure_timer_resolution();
                    std::thread::sleep(Duration::from_millis(6));
                }
                WebGpuPresentMode::Immediate => {
                    // Immediate mode: no pacing, lowest latency but highest CPU
                }
            }
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
