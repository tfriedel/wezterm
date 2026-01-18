use crate::colorease::ColorEaseUniform;
use crate::termwindow::webgpu::ShaderUniform;
use crate::termwindow::RenderFrame;
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
    use std::time::Duration;

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

    /// Query the current refresh period using DwmGetCompositionTimingInfo.
    pub fn query_refresh_period() -> Option<Duration> {
        use std::mem::MaybeUninit;
        use winapi::shared::minwindef::FALSE;
        use winapi::um::dwmapi::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
        use winapi::um::profileapi::QueryPerformanceFrequency;

        unsafe {
            let mut frequency: i64 = 0;
            if QueryPerformanceFrequency(&mut frequency as *mut i64 as *mut _) == FALSE {
                return None;
            }

            let mut timing_info = MaybeUninit::<DWM_TIMING_INFO>::zeroed();
            let timing_ptr = timing_info.as_mut_ptr();
            (*timing_ptr).cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;

            let result = DwmGetCompositionTimingInfo(std::ptr::null_mut(), timing_ptr);
            if result != 0 {
                return None;
            }

            let timing_info = timing_info.assume_init();
            let qpc_refresh_period = timing_info.qpcRefreshPeriod as i64;
            if qpc_refresh_period <= 0 {
                return None;
            }

            let nanos = (qpc_refresh_period as u128 * 1_000_000_000) / frequency as u128;
            Some(Duration::from_nanos(nanos as u64))
        }
    }
}

#[cfg(windows)]
const DEFAULT_REFRESH_PERIOD: Duration = Duration::from_nanos(16_666_667);

#[cfg(windows)]
pub struct FramePacer {
    refresh_period: Duration,
    last_refresh_query: Instant,
    last_frame_end: Instant,
    overshoot_streak: u8,
}

#[cfg(windows)]
impl FramePacer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            refresh_period: DEFAULT_REFRESH_PERIOD,
            last_refresh_query: now,
            last_frame_end: now,
            overshoot_streak: 0,
        }
    }

    fn refresh_period_secs(&self) -> f64 {
        self.refresh_period.as_secs_f64()
    }

    fn maybe_update_refresh_period(&mut self) {
        const REFRESH_QUERY_INTERVAL: Duration = Duration::from_secs(1);

        if self.last_refresh_query.elapsed() < REFRESH_QUERY_INTERVAL {
            return;
        }
        self.last_refresh_query = Instant::now();

        if let Some(new_period) = dwm_vsync::query_refresh_period() {
            // Ignore obviously bogus values
            if new_period < Duration::from_millis(2) || new_period > Duration::from_millis(25) {
                return;
            }

            let current = self.refresh_period;
            let diff = if new_period > current {
                new_period - current
            } else {
                current - new_period
            };

            if diff >= Duration::from_micros(200) {
                log::debug!(
                    "FramePacer refresh period update {:?} -> {:?}",
                    current,
                    new_period
                );
                self.refresh_period = new_period;
            }
        }
    }

    fn clamp_margin(target_secs: f64) -> Duration {
        // Aim to start rendering slightly before vsync. Keep the margin small on
        // fast panels but cap it for 60Hz to avoid excessive idle time.
        let min_margin = 0.0005;
        let max_margin = 0.006;
        let desired = (target_secs * 0.2).clamp(min_margin, max_margin);
        Duration::from_secs_f64(desired)
    }

    fn clamp_sleep_bounds(target: Duration) -> (f64, f64) {
        let min_sleep = Duration::from_micros(500);
        let max_sleep = target
            .saturating_sub(Duration::from_micros(500))
            .max(min_sleep);
        (min_sleep.as_secs_f64(), max_sleep.as_secs_f64())
    }

    pub fn pace(&mut self, frame_start: Instant, frame_end: Instant) {
        dwm_vsync::ensure_timer_resolution();
        self.maybe_update_refresh_period();

        let target = self.refresh_period;
        let target_secs = self.refresh_period_secs();
        let frame_render_time = frame_end
            .checked_duration_since(frame_start)
            .unwrap_or_default();
        let frame_interval = frame_end
            .checked_duration_since(self.last_frame_end)
            .unwrap_or_default();
        self.last_frame_end = frame_end;

        let safety_margin = Self::clamp_margin(target_secs);
        let baseline_sleep = target.saturating_sub(frame_render_time + safety_margin);
        let baseline_secs = baseline_sleep.as_secs_f64();
        let error_secs = target_secs - frame_interval.as_secs_f64();
        let mut sleep_secs = baseline_secs + error_secs * 0.5;

        let (min_sleep, max_sleep) = Self::clamp_sleep_bounds(target);
        if !sleep_secs.is_finite() {
            sleep_secs = min_sleep;
        }
        sleep_secs = sleep_secs.clamp(min_sleep, max_sleep);
        let mut sleep_duration = Duration::from_secs_f64(sleep_secs);

        let overshoot_threshold = Duration::from_millis(3);
        if frame_interval > target + overshoot_threshold {
            self.overshoot_streak = self.overshoot_streak.saturating_add(1).min(10);
        } else {
            self.overshoot_streak = self.overshoot_streak.saturating_sub(1);
        }

        if self.overshoot_streak >= 4 {
            sleep_duration = Duration::from_millis(1);
            self.overshoot_streak = 0;
        }

        metrics::histogram!("gui.frame.pacer.sleep").record(sleep_duration);
        std::thread::sleep(sleep_duration);
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

        #[cfg(windows)]
        let frame_start = Instant::now();

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

        #[cfg(windows)]
        {
            let frame_end = Instant::now();
            if matches!(
                self.config.webgpu_present_mode,
                WebGpuPresentMode::Mailbox | WebGpuPresentMode::AutoNoVsync
            ) {
                self.frame_pacer.pace(frame_start, frame_end);
            }
        }

        // Frame pacing for Windows to reduce CPU usage while maintaining low latency.
        //
        // Mailbox/AutoNoVsync present modes rely on FramePacer above, which pauses after
        // present() based on the measured refresh interval. Fifo continues to use DwmFlush
        // to guarantee vsync synchronization at the cost of one frame of latency.
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
                    // Mailbox mode pacing handled by FramePacer; nothing additional here.
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
