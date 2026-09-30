<div align="center">

# 🎬 HWA-Video

**Zero-copy hardware video pipeline on Intel HD 5500 (Broadwell, Gen8).**

[![Rust](https://img.shields.io/badge/Rust-2024-ed6f22?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![wgpu](https://img.shields.io/badge/wgpu-30.0-4dabf7?style=for-the-badge)](https://wgpu.rs/)
[![FFmpeg](https://img.shields.io/badge/FFmpeg-8.1-007808?style=for-the-badge&logo=ffmpeg&logoColor=white)](https://ffmpeg.org/)
[![Vulkan](https://img.shields.io/badge/Vulkan-1.3-AC162C?style=for-the-badge&logo=vulkan&logoColor=white)](https://www.vulkan.org/)
[![License](https://img.shields.io/badge/License-MPL--2.0-blue?style=for-the-badge)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20Wayland-9cf?style=for-the-badge&logo=linux&logoColor=black)]()

A proof-of-concept demonstrating H.264 hardware decode, GPU color grading, live
preview, and hardware H.264 export on a 2014 15W laptop iGPU — with the frame
never touching the CPU between decode and grade.

</div>

---

## ⚡ What It Does

```
H.264 file
  ↓  i965 VA-API decode                (video engine)
  ↓  NV12 surface, two-plane DMA-BUF   (no VPP pass)
  ↓  Vulkan import of Y (R8) and UV (Rg8) planes via
     VK_EXT_image_drm_format_modifier  (grafting)
  ↓  compute shader: BT.709 YUV→RGB, grade, output
       — preview: Rgba8Unorm
       — export:  Y/UV packed as R32Uint
  ↓  preview: blit to winit surface
     export:  readback → ffmpeg h264_vaapi
```

No VPP block, no BGRA round-trip, no software color conversion on the export
path. The decoder's native NV12 output goes straight into Vulkan.

## 📊 Measured on a Broadwell 15W Laptop

| Workload | Result |
|---|---|
| Preview pipeline (1080p) | **167 fps** |
| Preview pipeline (720p) | **290+ fps** |
| 1080p60 export, CQP22 h264_vaapi, noise 15 Mbps | **75 fps** |
| 1080p30 export, CQP22 h264_vaapi, real content 15 Mbps | **77 fps** |
| Decode cost (i965 VAAPI, per frame) | 0.25 ms |
| Import + acquire (per frame, cache hit) | 2.0 ms |
| Compute grade (per frame) | 0.5 ms |
| Readback (per frame, packed) | 5.5 ms |
| Encoder backpressure (per frame) | 3 ms |

Export runs **2.5–3× faster than real time** on real 1080p60 content.

## 🧠 Why This Is Interesting

Every layer of the software stack individually says this is not supported:

| Layer | Complaint | Resolution |
|---|---|---|
| Chromium WebCodecs | GPU process SIGTRAP on hardware encode | Abandoned browser path |
| wgpu | No external-memory import API | Used `grafting` |
| `grafting` 0.6 | Rejects `DRM_FORMAT_BGRA8888`, `R8`, `GR88` | Local patch (below) |
| `wgpu` 29 | Foreign-queue import requires wgpu 30 | Feature flag `wgpu-30` |
| i965 VPP | Produces corrupted output on 1080p60 incompressible content | **Bypassed entirely** |
| wgpu 30 | API churn (present, poll, pipeline layout, color_space) | Ported |
| `R8Unorm` / `Rg8Unorm` | Not storage-capable on HasVK | Pack into `R32Uint` |

The result runs on 2014-era hardware with a **15W TDP** — no discrete GPU
required.

## 🚀 Getting Started

### Prerequisites

```bash
# CachyOS / Arch Linux
sudo pacman -S mesa vulkan-intel libva-intel-driver ffmpeg clang
```

Rust toolchain must support **edition 2024** (Rust 1.85+).

### Build and Run

```bash
git clone https://github.com/reold/HWA-Video.git
cd HWA-Video
cargo run --release
```

Default input is `/tmp/h264_test.mp4`. Generate one if needed:

```bash
ffmpeg -f lavfi -i testsrc=size=1920x1080:rate=60 \
       -f lavfi -i sine=frequency=1000 \
       -t 10 -c:v libx264 -profile:v high -pix_fmt yuv420p \
       -c:a aac -shortest /tmp/h264_test.mp4
```

### Preview

```bash
cargo run --release -- /path/to/video.mp4
```

### Export

```bash
cargo run --release -- /path/to/video.mp4 export /path/to/output.mp4
```

The output is 1080p H.264 High profile, hardware encoded, in MP4.

## 📁 Project Layout

```
HWA-Video/
├── Cargo.toml
├── README.md
├── LICENSE                  MPL-2.0
├── src/
│   ├── main.rs              entry point, winit event loop, app builder
│   ├── app.rs               App struct, per-frame render loop
│   ├── ffmpeg.rs            FFmpeg Handles RAII wrapper (no filter graph)
│   ├── gpu.rs               pipelines, bind group layouts, cache types
│   ├── export.rs            FFmpeg child process wrapper (h264_vaapi)
│   ├── grade.wgsl           preview: YUV→RGB + grade → Rgba8Unorm
│   ├── grade_y.wgsl         export: YUV→RGB + grade → packed Y (R32Uint)
│   ├── grade_uv.wgsl        export: YUV→RGB + grade → packed UV (R32Uint)
│   ├── noop.wgsl            diagnostic: identity effect for chain testing
│   └── blit.wgsl            fullscreen blit to surface
└── vendor/
    └── grafting/            patched copy of grafting 0.6.0
        ├── Cargo.toml
        ├── LICENSE          original MPL-2.0
        └── src/
            └── vulkan_dmabuf.rs   patched
```

## 🔧 The `grafting` Patch

`vendor/grafting` is a local copy of `grafting` 0.6.0 with three changes in
`src/vulkan_dmabuf.rs`:

1. **`map_drm_format` accepts `DRM_FORMAT_BGRA8888`** (`0x34324142`) and maps
   it to `vk::Format::B8G8R8A8_UNORM`. Kept for compatibility with any path
   that still needs BGRA, though the main pipeline no longer uses it.

2. **`map_drm_format` accepts `DRM_FORMAT_R8`** (`0x20203852`) and
   **`DRM_FORMAT_GR88`** (`0x38385247`), and `map_format` accepts `R8Unorm` /
   `Rg8Unorm`. These are the NV12 Y and UV plane formats that i965 VAAPI
   emits for direct decoder output.

3. **`acquire_from_foreign_queue` uses a fence** instead of
   `vkQueueWaitIdle`. The queue-wide idle blocked on the compositor's
   in-flight submissions, adding ~1 ms of pure latency per frame.

The vendored copy retains its original MPL-2.0 license and copyright.

## 🧩 Design Notes

### Why the VPP is bypassed

The i965 VAAPI VPP (Video Post-Processing) block converts NV12 to BGRA on
dedicated hardware. In theory this is free. In practice, on Broadwell with
Mesa 26.2 and 1080p60 incompressible content, it produces corrupted output —
visible as black bands that cycle through the frame.

Three FFmpeg CLI tests isolated the fault:

| Test | Pipeline | Result |
|---|---|---|
| A | software decode → VAAPI encode | clean |
| B | VAAPI decode → VPP BGRA → libx264 | **corrupted** |
| C | VAAPI decode → hwdownload NV12 → libx264 | clean |

Test C proves VAAPI decode is fine; Test B proves VPP is not; Test A proves
the encoder is not. The fix is to skip the VPP entirely and do the YUV→RGB
conversion in the compute shader we were already running.

### Why NV12 is imported as two separate DMA-BUFs

The decoder exports the NV12 surface as a single DMA-BUF with two layers
(Y in R8, UV in GR88) via `av_hwframe_map`. Vulkan's
`VK_EXT_image_drm_format_modifier` accepts multi-layer imports, but wgpu has
no safe API for that. We import each plane as its own texture, which is
simpler and, on this hardware, just as fast.

### Why export textures are packed

`R8Unorm` and `Rg8Unorm` are not storage-capable on HasVK, even with
`TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES` enabled. We use `R32Uint` and pack
4 Y values (or 2 UV pairs) per texel in the shader. This has a side benefit:
the readback bytes are already in NV12 layout, so no byte-extraction loop is
needed. Readback drops from 14 ms to 5.5 ms per frame.

## ⚠️ Known Limitations

- **H.264 only.** Broadwell has no H.265 or VP9 hardware decode.
- **No VPP.** Any pipeline that needs VPP-side scaling or color conversion
  would have to do it on the compute queue instead.
- **Encoder throughput cap.** The HD 5500 hardware encoder sustains roughly
  75–90 fps at 1080p CQP22. Real-time (60 fps) export works for content with
  normal complexity; extreme bitrates may dip below.
- **`EFFECT_PASSES` env var is a diagnostic.** It chains N no-op compute
  passes to measure the cost of an effect chain. Not intended for users.
- **Wayland only.** X11 should work via `WINIT_UNIX_BACKEND=x11` but is
  untested.

## 📈 Effect Budget

Measured marginal cost of a chained compute pass at 1080p on the HD 5500:

| Chained passes | Pipeline fps | 60 fps preview? |
|---|---|---|
| 0 | 167 | ✅ 2.8× headroom |
| 1 | 112 | ✅ |
| 2 | 101 | ✅ |
| 4 | 86 | ✅ |
| 6 | ~70 | ⚠️ |
| 8 | 54 | ❌ |

**Almost no editing effect is a chained pass.** Point operations — saturation,
contrast, curves, LUT lookup, white balance, vignette, film grain, chroma key
— operate per-pixel and can be fused into the existing grade shader. Fused
cost is roughly +0.05 ms per operation. Only spatial effects (blur, sharpen,
warp) require dedicated passes.

Practical preview budget: a full color-grade stack + one or two spatial
effects at 60 fps, at 1080p, on a 15W 2014 iGPU.

## 🗺️ Roadmap

- [x] VA-API decode → DMA-BUF → wgpu import
- [x] Compute shader grading pass
- [x] Live preview window
- [x] Texture cache (import-once)
- [x] Bypass VPP, import native NV12
- [x] Hardware H.264 export
- [x] Packed export textures (4× readback reduction)
- [x] Effect budget quantified
- [ ] Tauri v2 shell with native preview surface + Svelte UI
- [ ] Timeline scrub / seek (keyframe decode)
- [ ] Effect chain editor (fused color ops + chained spatial ops)
- [ ] Audio integration (playback, waveform, gain)

## 🙏 Acknowledgements

[`grafting`](https://github.com/merely-made/wgpu-graft) — the core texture
interop library of the Servo `wgpu-graft` workspace. Its
`VulkanDmaBufImport` and `create_dmabuf_host_context` provide the
zero-copy DMA-BUF import machinery that made this project possible. Without
them, several hundred lines of raw `ash` code would have been necessary just
to open a DMA-BUF as a Vulkan image.

The `vendor/grafting` directory is a locally patched copy of the crates.io
release (v0.6.0) and retains its original MPL-2.0 license and copyright.

## 📜 License

Mozilla Public License 2.0. See [`LICENSE`](LICENSE) for the full text.
