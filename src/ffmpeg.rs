use anyhow::{Result, bail};
use ffmpeg_sys_next::*;
use std::ffi::CString;
use std::ptr;
use std::time::Duration;

pub fn check(rc: i32, what: &str) -> Result<()> {
    if rc < 0 {
        bail!("{} failed: {}", what, rc);
    }
    Ok(())
}

/// All FFmpeg state for one open file. Owned by App.
pub struct Handles {
    pub fmt_ctx: *mut AVFormatContext,
    pub codec_ctx: *mut AVCodecContext,
    pub graph: *mut AVFilterGraph,
    pub buffersrc_ctx: *mut AVFilterContext,
    pub buffersink_ctx: *mut AVFilterContext,
    pub frames_ref: *mut AVBufferRef,
    pub hw_device_ctx: *mut AVBufferRef,
    pub packet: *mut AVPacket,
    pub decoded: *mut AVFrame,
    pub filtered: *mut AVFrame,
    pub drm_frame: *mut AVFrame,
    pub video_stream: i32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

impl Handles {
    /// Safety: caller must ensure the returned Handles is dropped before any
    /// other FFmpeg use in this thread that could touch the same contexts.
    pub unsafe fn open(path: &str, device: &str) -> Result<Self> {
        let cpath = CString::new(path.to_string())?;
        let device_name = CString::new(device)?;

        unsafe {
            let mut fmt_ctx: *mut AVFormatContext = ptr::null_mut();
            check(
                avformat_open_input(
                    &mut fmt_ctx,
                    cpath.as_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "avformat_open_input",
            )?;
            check(
                avformat_find_stream_info(fmt_ctx, ptr::null_mut()),
                "avformat_find_stream_info",
            )?;

            let video_stream = av_find_best_stream(
                fmt_ctx,
                AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                ptr::null_mut(),
                0,
            );
            if video_stream < 0 {
                bail!("no video stream");
            }
            let stream = *(*fmt_ctx).streams.add(video_stream as usize);
            let codecpar = (*stream).codecpar;

            let width = (*codecpar).width as u32;
            let height = (*codecpar).height as u32;
            let fps_val = av_q2d((*stream).r_frame_rate);
            let fps = if fps_val > 0.0 { fps_val } else { 30.0 };

            let codec = avcodec_find_decoder((*codecpar).codec_id);
            if codec.is_null() {
                bail!("no decoder");
            }

            let codec_ctx = avcodec_alloc_context3(codec);
            check(
                avcodec_parameters_to_context(codec_ctx, codecpar),
                "avcodec_parameters_to_context",
            )?;

            let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
            check(
                av_hwdevice_ctx_create(
                    &mut hw_device_ctx,
                    AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    device_name.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "av_hwdevice_ctx_create",
            )?;
            (*codec_ctx).hw_device_ctx = av_buffer_ref(hw_device_ctx);

            check(
                avcodec_open2(codec_ctx, codec, ptr::null_mut()),
                "avcodec_open2",
            )?;

            let frames_ref = av_hwframe_ctx_alloc(hw_device_ctx);
            if frames_ref.is_null() {
                bail!("av_hwframe_ctx_alloc failed");
            }
            {
                let frames = (*frames_ref).data as *mut AVHWFramesContext;
                (*frames).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*frames).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
                (*frames).width = width as i32;
                (*frames).height = height as i32;
                (*frames).initial_pool_size = 20;
            }
            check(av_hwframe_ctx_init(frames_ref), "av_hwframe_ctx_init")?;

            let (graph, buffersrc_ctx, buffersink_ctx) =
                build_filter_graph(frames_ref, width, height)?;

            let packet = av_packet_alloc();
            let decoded = av_frame_alloc();
            let filtered = av_frame_alloc();
            let drm_frame = av_frame_alloc();

            Ok(Handles {
                fmt_ctx,
                codec_ctx,
                graph,
                buffersrc_ctx,
                buffersink_ctx,
                frames_ref,
                hw_device_ctx,
                packet,
                decoded,
                filtered,
                drm_frame,
                video_stream,
                width,
                height,
                fps,
            })
        }
    }

    pub fn frame_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps)
    }

    /// Seek back to the start and drain decoder state — used to loop the video.
    pub unsafe fn rewind(&mut self) {
        unsafe {
            av_seek_frame(self.fmt_ctx, self.video_stream, 0, AVSEEK_FLAG_BACKWARD);
            avcodec_flush_buffers(self.codec_ctx);
            av_frame_unref(self.decoded);
            av_frame_unref(self.filtered);
            av_packet_unref(self.packet);
        }
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            if !self.drm_frame.is_null() {
                av_frame_free(&mut self.drm_frame);
            }
            if !self.filtered.is_null() {
                av_frame_free(&mut self.filtered);
            }
            if !self.decoded.is_null() {
                av_frame_free(&mut self.decoded);
            }
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet);
            }
            if !self.graph.is_null() {
                avfilter_graph_free(&mut self.graph);
            }
            if !self.frames_ref.is_null() {
                av_buffer_unref(&mut self.frames_ref);
            }
            if !self.codec_ctx.is_null() {
                avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.hw_device_ctx.is_null() {
                av_buffer_unref(&mut self.hw_device_ctx);
            }
            if !self.fmt_ctx.is_null() {
                avformat_close_input(&mut self.fmt_ctx);
            }
        }
    }
}

/// Peek at a file's video dimensions and framerate without keeping it open.
pub unsafe fn peek_video_info(path: &str) -> Result<(u32, u32, f64)> {
    let cpath = CString::new(path.to_string())?;
    unsafe {
        let mut fmt_ctx: *mut AVFormatContext = ptr::null_mut();
        check(
            avformat_open_input(
                &mut fmt_ctx,
                cpath.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            "peek: avformat_open_input",
        )?;
        check(
            avformat_find_stream_info(fmt_ctx, ptr::null_mut()),
            "peek: avformat_find_stream_info",
        )?;
        let vs = av_find_best_stream(
            fmt_ctx,
            AVMediaType::AVMEDIA_TYPE_VIDEO,
            -1,
            -1,
            ptr::null_mut(),
            0,
        );
        if vs < 0 {
            avformat_close_input(&mut fmt_ctx);
            bail!("peek: no video stream");
        }
        let stream = *(*fmt_ctx).streams.add(vs as usize);
        let w = (*(*stream).codecpar).width as u32;
        let h = (*(*stream).codecpar).height as u32;
        let fps_val = av_q2d((*stream).r_frame_rate);
        let fps = if fps_val > 0.0 { fps_val } else { 30.0 };
        avformat_close_input(&mut fmt_ctx);
        Ok((w, h, fps))
    }
}

unsafe fn build_filter_graph(
    frames_ref: *mut AVBufferRef,
    width: u32,
    height: u32,
) -> Result<(
    *mut AVFilterGraph,
    *mut AVFilterContext,
    *mut AVFilterContext,
)> {
    unsafe {
        let graph = avfilter_graph_alloc();
        if graph.is_null() {
            bail!("avfilter_graph_alloc returned null");
        }

        let buffersrc = avfilter_get_by_name(b"buffer\0".as_ptr() as *const _);
        if buffersrc.is_null() {
            bail!("buffer filter not found");
        }

        let args = CString::new(format!(
            "video_size={}x{}:pix_fmt=nv12:time_base=1/30:pixel_aspect=1/1",
            width, height
        ))?;

        let mut buffersrc_ctx: *mut AVFilterContext = ptr::null_mut();
        check(
            avfilter_graph_create_filter(
                &mut buffersrc_ctx,
                buffersrc,
                b"in\0".as_ptr() as *const _,
                args.as_ptr(),
                ptr::null_mut(),
                graph,
            ),
            "avfilter_graph_create_filter(buffer)",
        )?;

        let par = av_buffersrc_parameters_alloc();
        if par.is_null() {
            bail!("av_buffersrc_parameters_alloc returned null");
        }
        (*par).format = AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*par).width = width as i32;
        (*par).height = height as i32;
        (*par).hw_frames_ctx = av_buffer_ref(frames_ref);
        check(
            av_buffersrc_parameters_set(buffersrc_ctx, par),
            "av_buffersrc_parameters_set",
        )?;
        av_free(par as *mut _);

        let buffersink = avfilter_get_by_name(b"buffersink\0".as_ptr() as *const _);
        if buffersink.is_null() {
            bail!("buffersink filter not found");
        }
        let mut buffersink_ctx: *mut AVFilterContext = ptr::null_mut();
        check(
            avfilter_graph_create_filter(
                &mut buffersink_ctx,
                buffersink,
                b"out\0".as_ptr() as *const _,
                ptr::null(),
                ptr::null_mut(),
                graph,
            ),
            "avfilter_graph_create_filter(buffersink)",
        )?;

        let scale = avfilter_get_by_name(b"scale_vaapi\0".as_ptr() as *const _);
        if scale.is_null() {
            bail!("scale_vaapi filter not found");
        }
        let mut scale_ctx: *mut AVFilterContext = ptr::null_mut();
        check(
            avfilter_graph_create_filter(
                &mut scale_ctx,
                scale,
                b"scale\0".as_ptr() as *const _,
                b"format=bgra\0".as_ptr() as *const _,
                ptr::null_mut(),
                graph,
            ),
            "avfilter_graph_create_filter(scale_vaapi)",
        )?;

        check(
            avfilter_link(buffersrc_ctx, 0, scale_ctx, 0),
            "avfilter_link(in→scale)",
        )?;
        check(
            avfilter_link(scale_ctx, 0, buffersink_ctx, 0),
            "avfilter_link(scale→out)",
        )?;
        check(
            avfilter_graph_config(graph, ptr::null_mut()),
            "avfilter_graph_config",
        )?;

        Ok((graph, buffersrc_ctx, buffersink_ctx))
    }
}
