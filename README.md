# RustDesk — DRM/KMS Capture Fork

> ⚠️ **This is an experimental fork** of [RustDesk](https://github.com/rustdesk/rustdesk) with a DRM/KMS screen capture backend for Linux.
> An upstream PR is planned once AMD GPU testing is completed.

## What's Different

This fork adds a **DRM/KMS capture backend** powered by [libdrmtap](https://github.com/fxd0h/libdrmtap), replacing PipeWire/portal-based capture on Linux. This means:

- **No user consent popup** — no "Select the screen to be shared" dialog
- **Works on login screen** — GDM, SDDM, LightDM
- **Works headless** — no display server needed
- **Works in VMs** — virtio-gpu, QEMU, cloud instances
- **No PipeWire dependency** — works on minimal systems
- **Handles GPU tiling** — Intel CCS, NVIDIA tiled, AMD automatically via EGL

The DRM backend is tried **first**; if it fails (e.g., no DRM device, or on X11 with proprietary drivers that don't expose CRTCs), it falls back to the standard PipeWire → X11 pipeline. **No existing functionality is broken.**

## Tested Platforms

| GPU | Driver | Resolution | Detiling | CPU (idle) | CPU (active) | Status |
|---|---|---|---|---|---|---|
| Intel Gen12 (Alder Lake) | i915 | 3840×2160 | EGL (CCS) | ~5% | ~30% | ✅ Working |
| NVIDIA Jetson Orin Nano | nvidia-drm 540.4.0 | 1920×1080 | EGL (tiled) | **8%** | **14%** | ✅ Working |
| virtio-gpu (QEMU/KVM) | virtio_gpu | 1920×1080 | None (linear) | ~3% | ~20% | ✅ Working |
| AMD (amdgpu) | amdgpu | — | — | — | — | ⏳ Pending |

## Performance Optimizations

The capture loop includes two key optimizations for low-power devices:

1. **Rate limiter (16ms)** — Caps capture at ~60 FPS, prevents CPU spinning
2. **fb_id skip** — Detects static screens by comparing framebuffer IDs, skipping expensive EGL detiling and memory copies when nothing changed

On the NVIDIA Jetson Orin Nano (6-core ARM, no hardware encoder), these brought CPU from **97% → 8% idle, 14% active**.

## Building (Linux)

### Prerequisites

```bash
# Install libdrmtap
git clone https://github.com/fxd0h/libdrmtap
cd libdrmtap
meson setup build && meson compile -C build && sudo meson install -C build
sudo ldconfig

# Grant capture permissions (one of):
sudo setcap cap_sys_admin+ep /path/to/rustdesk    # Direct access
# OR install the drmtap-helper with setcap         # Helper binary
```

### Build RustDesk

```bash
git clone https://github.com/fxd0h/rustdesk -b feature/drm-capture
cd rustdesk
cargo build --release --features linux-pkg-config
```

### Environment Variables

| Variable | Description | Example |
|---|---|---|
| `DRM_DEVICE` | Override DRM device path | `/dev/dri/card1` |
| `DRMTAP_DEBUG` | Enable debug logging | `1` |

## Changed Files

| File | Change |
|---|---|
| `libs/scrap/src/common/drm.rs` | **[NEW]** DRM/KMS capture backend (inline FFI to libdrmtap) |
| `libs/scrap/src/common/linux.rs` | Added `DRM` variant, DRM-first fallback logic |
| `libs/scrap/src/common/mod.rs` | Added `mod drm;` declaration |
| `libs/scrap/build.rs` | Added libdrmtap link search paths |

## Upstream Status

- [ ] Open feature request issue on [rustdesk/rustdesk](https://github.com/rustdesk/rustdesk)
- [ ] AMD GPU testing
- [ ] Submit PR

---

*For the original RustDesk README, see [upstream](https://github.com/rustdesk/rustdesk).*
*libdrmtap is MIT licensed: [github.com/fxd0h/libdrmtap](https://github.com/fxd0h/libdrmtap)*
