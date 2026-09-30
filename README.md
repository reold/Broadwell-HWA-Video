<div align="center">

# 🎬 HWA-Video

**Zero-copy hardware video pipeline on Intel HD 5500 (Broadwell, Gen8).**

[![Rust](https://img.shields.io/badge/Rust-2024-ed6f22?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![wgpu](https://img.shields.io/badge/wgpu-30.0-4dabf7?style=for-the-badge)](https://wgpu.rs/)
[![FFmpeg](https://img.shields.io/badge/FFmpeg-8.1-007808?style=for-the-badge&logo=ffmpeg&logoColor=white)](https://ffmpeg.org/)
[![Vulkan](https://img.shields.io/badge/Vulkan-1.3-AC162C?style=for-the-badge&logo=vulkan&logoColor=white)](https://www.vulkan.org/)
[![License](https://img.shields.io/badge/License-MPL--2.0-blue?style=for-the-badge)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20Wayland-9cf?style=for-the-badge&logo=linux&logoColor=black)]()

A proof-of-concept demonstrating H.264 hardware decode, GPU color grading, and
live preview on a 2014 15W laptop iGPU — with the frame never touching the
CPU between decode and display.

</div>

---

## ⚡ What it does

```
H.264 file
  ↓  i965 VA-API decode                (video engine)
  ↓  scale_vaapi NV12 → BGRA           (VPP hardware block, not the 3D engine)
  ↓  DMA-BUF export, Y-tile modifier
  ↓  VK_EXT_image_drm_format_modifier import  (grafting, foreign-queue acquire)
  ↓  wgpu compute shader grade
  ↓  blit to winit window
```

Every pixel movement is GPU-direct. The only CPU work per frame is FFmpeg
bookkeeping, one DRM ioctl for the DMA-BUF acquire barrier, and command
buffer construction.

## 📊 Measured on a Broadwell 15W laptop

| Metric | Value |
|---|---|
| Resolution | 1280×720 |
| Codec | H.264 High profile |
| Decode | 0.22 ms/frame (hardware) |
| Filter (NV12 → BGRA) | 0.12 ms/frame (VPP hardware) |
| DMA-BUF import + acquire | 1.25 ms/frame (cache-hit) |
| Compute + blit + present | 1.9 ms/frame |
| **Total pipeline** | **~3.5 ms/frame = ~285 fps** |
| Live preview | 30 fps, source framerate, negligible CPU |

Raw throughput measured with the display pacing removed. In normal operation
the preview plays at the source framerate.

## 🧩 Why this is interesting

Every layer of the software stack individually says this is not supported:

| Layer | Complaint | Resolution |
|---|---|---|
| Chromium WebCodecs | GPU process SIGTRAP on hardware encode | Abandoned browser path |
| wgpu | No external-memory import API | Used `grafting` |
| `grafting` 0.6 | Rejects `DRM_FORMAT_BGRA8888` | Local patch (below) |
| `wgpu` 29 | Foreign-queue import requires wgpu 30 | Feature flag `wgpu-30` |
| i965 VPP | Only accepts `format=bgra` for RGB output | Accept BGRA output, patch grafting |
| wgpu 30 | API churn (present, poll, pipeline layout, color_space) | Ported |
| `grafting`'s queue-wide wait | `vkQueueWaitIdle` blocks on compositor too | Patched to fence |

The result runs on 2014-era hardware with a **15W TDP** — no discrete GPU
required.

## 🚀 Getting started

### Prerequisites

```bash
# CachyOS / Arch Linux
sudo pacman -S mesa vulkan-intel libva-intel-driver ffmpeg clang
```

You also need a Rust toolchain supporting **edition 2024** (Rust 1.85+).

### Build and run

```bash
git clone https://github.com/reold/HWA-Video.git
cd HWA-Video
cargo run --release
```

By default it plays `/tmp/h264_test.mp4`. Generate one if you need:

```bash
ffmpeg -f lavfi -i testsrc=size=1280x720:rate=30 \
       -f lavfi -i sine=frequency=1000 \
       -t 10 -c:v libx264 -profile:v high -pix_fmt yuv420p \
       -c:a aac -shortest /tmp/h264_test.mp4
```

### Try your own video

```bash
cargo run --release -- /path/to/your_video.mp4
```

Requires an Intel Gen8 GPU (Broadwell) with the i965 VA-API driver. Haswell
(Gen7.5) may work with minor adjustments; newer Intel iGPUs using the `iHD`
driver should work if the fourcc mapping in the local patch is adapted.

## 📁 Project layout

```
HWA-Video/
├── Cargo.toml
├── README.md
├── LICENSE                  MPL-2.0
├── src/
│   ├── main.rs              entry point, winit event loop, app builder
│   ├── app.rs               App struct, per-frame render loop
│   ├── ffmpeg.rs            FFmpeg Handles RAII wrapper, filter graph
│   ├── gpu.rs               pipelines, cache types
│   ├── grade.wgsl           compute shader (saturation + gamma)
│   └── blit.wgsl            fullscreen blit to surface
└── vendor/
    └── grafting/            patched copy of grafting 0.6.0
        ├── Cargo.toml
        ├── LICENSE          original MPL-2.0
        └── src/
            └── vulkan_dmabuf.rs   patched
```

Most future changes touch one file. Adding an effect is `gpu.rs` plus a
new `.wgsl`. Timeline seeking is `ffmpeg.rs` plus a small hook in `app.rs`.

## 🔧 The `grafting` patch

`vendor/grafting` is a local copy of `grafting` 0.6.0 with two changes in
`src/vulkan_dmabuf.rs`:

1. **`map_drm_format` accepts `DRM_FORMAT_BGRA8888`** (`0x34324142`) and maps
   it to `vk::Format::B8G8R8A8_UNORM`. The i965 VA-API driver emits this
   fourcc for `format=bgra` output; grafting 0.6.0 does not accept it.

2. **`acquire_from_foreign_queue` uses a fence** instead of
   `vkQueueWaitIdle`. The queue-wide idle blocked on the compositor's
   in-flight submissions too, adding ~1 ms of pure latency to every frame.
   Waiting on a fence tied to just our barrier removes that.

Both changes are small and localized. The vendored copy retains its original
MPL-2.0 license and copyright.

## 🧠 Design notes

### The DMA-BUF import must run every frame

The most tempting optimization — caching the imported texture and skipping
`av_hwframe_map` on a cache hit — was tried and rejected. The map triggers
graft's foreign-queue acquire barrier, which is what synchronizes VAAPI's
writes to the DMA-BUF with Vulkan's reads from it. Skipping the map on hits
produced visible black tearing at the top of the frame: Vulkan was reading
memory while VAAPI was still writing it.

The correct split is:

- **Import** (once per DMA-BUF): `vkCreateImage`, `vkAllocateMemory`,
  `vkBindImageMemory`, `create_texture_from_hal`. **Cache these.**
- **Acquire** (every frame): submit the `ImageMemoryBarrier` that transitions
  the image from `VK_QUEUE_FAMILY_FOREIGN_EXT` to our queue family. **Do not
  cache this.**

In the current code they are fused inside graft's `import_dmabuf`, so a
cache hit still pays ~1.25 ms for the map. See *Future optimizations* below.

### Cache identity on Broadwell

The VAAPI pool on Broadwell exposes a small number of surface slots (observed
as 1–37 depending on driver decisions). The cache is keyed on `fstat` of the
underlying GEM object — `(dev, ino, offset, stride, modifier, format)`. The
`fd` field is deliberately **not** part of the key: it is a fresh dup each
frame and would defeat caching.

### Why not `av_hwframe_map` once and hold the frame?

Holding a cloned `AVFrame` per cache entry starves the VAAPI pool. The pool
cannot recycle a slot while our clone references it, so every frame allocates
a fresh surface. That collapses the cache hit rate to zero and *slows down*
the pipeline. The `CachedTexture` struct therefore holds only the wgpu
texture and view; it does not hold an AVFrame.

## ⚠️ Known limitations

- **H.264 only.** Broadwell has no H.265 or VP9 hardware decode.
- **One effect wired up.** A saturation + gamma grade in `grade.wgsl`. The
  pipeline is extensible but not yet a full editor.
- **Preview pacing is locked to source framerate.** The pipeline can run at
  ~285 fps, but a `std::thread::sleep` in `render_one_frame` caps it at
  1/fps so the video plays in real time. Remove the sleep to see the raw
  throughput number.
- **Wayland only.** X11 should work via `WINIT_UNIX_BACKEND=x11` but is
  untested.
- **Locale warnings from `xkbcommon`** on some systems (`en_IN.ISO8859-1`)
  are cosmetic and unrelated to the pipeline.

## 🔮 Future optimizations

### Reacquire without map (deferred)

The cache-hit import path is dominated by `av_hwframe_map` (~1.15 ms), which
is called solely to trigger graft's foreign-queue acquire barrier. Exposing a
`reacquire(&wgpu::Texture, &HostWgpuContext)` function from graft — roughly
30 lines that fetch the underlying `VkImage` and submit the same barrier —
would reduce per-frame import cost to **~0.15 ms** and raise the pipeline
ceiling from ~285 fps to **~415 fps**.

This is deferred because:

- It buys nothing observable at the current preview settings (30 fps cap).
- It adds a second unsafe barrier path that needs testing on multiple drivers.
- The real gains arrive when stacking many compute effects, where import cost
  is paid once and amortized over all effects.

The measurement is recorded here so a future contributor knows the tradeoff.

### Multi-frame pipeline

Currently we `poll(Wait)` after every frame, blocking until the compositor
recycles the swapchain image. Pipelining two or three frames in flight,
synchronizing on previous frames' fences instead, would remove the compositor
from the per-frame critical path. Expect another 30–50% throughput once the
compositor is decoupled.

## 🗺️ Roadmap

- [x] VA-API decode → DMA-BUF → wgpu import
- [x] Compute shader grading pass
- [x] Live preview window
- [x] Texture cache (import-once)
- [x] Grafting patch: fence instead of queue-wide wait
- [ ] Hardware H.264 encode for export (VA-API encode via libva or FFmpeg sidecar)
- [ ] Timeline scrub / seek (keyframe decode)
- [ ] Multi-effect pipeline (chained compute passes)
- [ ] Svelte + Tauri UI shell
- [ ] Reacquire-without-map optimization (if needed)

## 🙏 Acknowledgements

[`grafting`](https://github.com/gfx-rs/grafting) by the gfx-rs community —
the zero-copy import machinery that made this possible. Without its
`VulkanDmaBufImport` and `create_dmabuf_host_context`, this project would
have needed several hundred lines of raw `ash` code just to open a DMA-BUF
as a Vulkan image.

The `vendor/grafting` directory retains its original MPL-2.0 license and
copyright.

## 📜 License

Mozilla Public License 2.0. See [`LICENSE`](LICENSE) for the full text.
