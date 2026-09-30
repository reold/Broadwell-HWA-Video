use crate::ffmpeg;
use crate::gpu::{CacheKey, CachedTexture, Pipelines};
use anyhow::{Result, bail};
use ffmpeg_sys_next::*;
use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::window::Window;

const MAX_CACHE_ENTRIES: usize = 64;

pub struct App {
    pub window: Arc<Window>,
    pub surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub host: grafting::HostWgpuContext,
    pub pipelines: Pipelines,
    pub ff: ffmpeg::Handles,

    // Dropped before ff, so wgpu textures release GEM refs before the
    // FFmpeg pool tries to free its surfaces.
    pub texture_cache: HashMap<CacheKey, CachedTexture>,

    pub frames: u64,
    pub last_log: Instant,
    pub timing_decode: Duration,
    pub timing_filter: Duration,
    pub timing_import: Duration,
    pub timing_gpu: Duration,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl App {
    pub fn render_one_frame(&mut self) -> Result<()> {
        let frame_start = Instant::now();
        let vw = self.ff.width;
        let vh = self.ff.height;

        unsafe {
            // ---- decode ----
            let t = Instant::now();
            let rc = av_read_frame(self.ff.fmt_ctx, self.ff.packet);
            if rc < 0 {
                self.ff.rewind();
                self.frames = 0;
                return Ok(());
            }
            if (*self.ff.packet).stream_index != self.ff.video_stream {
                av_packet_unref(self.ff.packet);
                return Ok(());
            }
            if avcodec_send_packet(self.ff.codec_ctx, self.ff.packet) < 0 {
                av_packet_unref(self.ff.packet);
                return Ok(());
            }
            av_packet_unref(self.ff.packet);

            let rc = avcodec_receive_frame(self.ff.codec_ctx, self.ff.decoded);
            if rc == -libc::EAGAIN {
                return Ok(());
            }
            if rc != 0 {
                bail!("avcodec_receive_frame rc={}", rc);
            }
            self.timing_decode += t.elapsed();

            // ---- filter ----
            let t = Instant::now();
            ffmpeg::check(
                av_buffersrc_add_frame_flags(self.ff.buffersrc_ctx, self.ff.decoded, 0),
                "av_buffersrc_add_frame_flags",
            )?;
            av_frame_unref(self.ff.decoded);
            ffmpeg::check(
                av_buffersink_get_frame(self.ff.buffersink_ctx, self.ff.filtered),
                "av_buffersink_get_frame",
            )?;
            self.timing_filter += t.elapsed();

            // ---- import (map every frame for the acquire barrier) ----
            let t = Instant::now();

            // The map must run every frame: it is what submits the foreign-queue
            // acquire barrier, synchronizing VAAPI's writes with our reads.
            (*self.ff.drm_frame).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            ffmpeg::check(
                av_hwframe_map(
                    self.ff.drm_frame,
                    self.ff.filtered,
                    AV_HWFRAME_MAP_READ as i32,
                ),
                "av_hwframe_map",
            )?;

            let desc = (*self.ff.drm_frame).data[0] as *const AVDRMFrameDescriptor;
            if desc.is_null() {
                av_frame_unref(self.ff.drm_frame);
                av_frame_unref(self.ff.filtered);
                bail!("null DRM descriptor");
            }
            let obj = &(*desc).objects[0];
            let layer = &(*desc).layers[0];
            let plane = &layer.planes[0];

            let key: crate::gpu::CacheKey = {
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(obj.fd, &mut stat) != 0 {
                    av_frame_unref(self.ff.drm_frame);
                    av_frame_unref(self.ff.filtered);
                    bail!("fstat failed: {}", std::io::Error::last_os_error());
                }
                crate::gpu::CacheKey {
                    dev: stat.st_dev as u64,
                    ino: stat.st_ino as u64,
                    offset: plane.offset as u64,
                    stride: plane.pitch as u64,
                    modifier: obj.format_modifier,
                    format: layer.format,
                }
            };

            let in_view = if let Some(entry) = self.texture_cache.get(&key) {
                self.cache_hits += 1;
                entry.view.clone()
            } else {
                self.cache_misses += 1;

                let owned_fd: OwnedFd = {
                    let raw = libc::dup(obj.fd);
                    if raw < 0 {
                        av_frame_unref(self.ff.drm_frame);
                        av_frame_unref(self.ff.filtered);
                        bail!("dup(fd) failed: {}", std::io::Error::last_os_error());
                    }
                    OwnedFd::from_raw_fd(raw)
                };

                let import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
                    dpi::PhysicalSize::new(vw, vh),
                    wgpu::TextureFormat::Bgra8Unorm,
                    layer.format,
                    obj.format_modifier,
                    vec![owned_fd],
                    vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
                        buffer_index: 0,
                        offset: plane.offset as u64,
                        stride: plane.pitch as u64,
                    }],
                    grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
                )
                .map_err(|e| {
                    av_frame_unref(self.ff.drm_frame);
                    av_frame_unref(self.ff.filtered);
                    anyhow::anyhow!("VulkanDmaBufImport::new: {e:?}")
                })?;

                let texture =
                    grafting::vulkan_dmabuf::import_dmabuf(import, &self.host).map_err(|e| {
                        av_frame_unref(self.ff.drm_frame);
                        av_frame_unref(self.ff.filtered);
                        anyhow::anyhow!("import_dmabuf: {e:?}")
                    })?;

                if self.texture_cache.len() < MAX_CACHE_ENTRIES {
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    self.texture_cache
                        .insert(key, CachedTexture { texture, view });
                    self.texture_cache.get(&key).unwrap().view.clone()
                } else {
                    texture.create_view(&wgpu::TextureViewDescriptor::default())
                }
            };

            av_frame_unref(self.ff.drm_frame);
            av_frame_unref(self.ff.filtered);
            self.timing_import += t.elapsed();

            // ---- gpu: acquire surface, dispatch compute, blit, present ----
            let t = Instant::now();
            let surface_tex = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                other => {
                    eprintln!("surface acquire: {other:?}, reconfiguring");
                    self.surface
                        .configure(&self.host.device, &self.surface_config);
                    return Ok(());
                }
            };
            let surface_view = surface_tex
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let grade_bg = self
                .host
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("grade-bg"),
                    layout: &self.pipelines.grade_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&in_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&self.pipelines.out_view),
                        },
                    ],
                });

            let mut enc =
                self.host
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("frame-encoder"),
                    });

            {
                let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("grade-pass"),
                    timestamp_writes: None,
                });
                cp.set_pipeline(&self.pipelines.grade_pipeline);
                cp.set_bind_group(0, &grade_bg, &[]);
                cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
            }

            {
                let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("blit-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(&self.pipelines.blit_pipeline);
                rp.set_bind_group(0, &self.pipelines.blit_bg, &[]);
                rp.draw(0..3, 0..1);
            }

            self.host.queue.submit([enc.finish()]);
            self.host.queue.present(surface_tex);

            self.host
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .ok();

            drop(grade_bg);
            self.timing_gpu += t.elapsed();
        }

        self.frames += 1;
        if self.last_log.elapsed() >= Duration::from_secs(2) {
            let secs = self.last_log.elapsed().as_secs_f64();
            let n = self.frames as f64;
            let pipeline_ms =
                (self.timing_decode + self.timing_filter + self.timing_import + self.timing_gpu)
                    .as_secs_f64()
                    * 1000.0
                    / n;
            println!(
                "fps {:.1} | dec {:.2} filt {:.2} imp {:.2} gpu {:.2} | pipe {:.2}ms = {:.0}fps | cache h{} m{} sz{}",
                n / secs,
                self.timing_decode.as_secs_f64() * 1000.0 / n,
                self.timing_filter.as_secs_f64() * 1000.0 / n,
                self.timing_import.as_secs_f64() * 1000.0 / n,
                self.timing_gpu.as_secs_f64() * 1000.0 / n,
                pipeline_ms,
                1000.0 / pipeline_ms,
                self.cache_hits,
                self.cache_misses,
                self.texture_cache.len(),
            );
            self.frames = 0;
            self.last_log = Instant::now();
            self.timing_decode = Duration::ZERO;
            self.timing_filter = Duration::ZERO;
            self.timing_import = Duration::ZERO;
            self.timing_gpu = Duration::ZERO;
            self.cache_hits = 0;
            self.cache_misses = 0;
        }

        let elapsed = frame_start.elapsed();
        let period = self.ff.frame_period();
        if elapsed < period {
            std::thread::sleep(period - elapsed);
        }

        Ok(())
    }
}
