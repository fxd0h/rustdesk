// Service-side DRM/KMS read engine. Runs in the ROOT `--service`, which already
// holds CAP_SYS_ADMIN, so libdrmtap reads the scanout in-process (direct mode,
// no helper fork, no setcap). Loaded via the dlopen loader (drmtap_dl) so the
// main binary has no hard libdrm/EGL dependency.
//
// SECURITY (direct-mode mitigation): the scanout parse now runs in the root
// service with no seccomp cage, so we do NOT honor an untrusted device path.
// The caller passes either None (libdrmtap auto-detects /dev/dri/card* by a
// hardcoded pattern) or an explicit path that we realpath-gate to /dev/dri/
// before opening. The DRM_DEVICE env is intentionally NOT consulted here.

use super::drmtap_dl::{self, drmtap_config, drmtap_ctx, drmtap_frame_info, DrmtapLib};
use hbb_common::log;
use std::ffi::CString;
use std::io;

// Largest scanout we will copy; also bounds w*4*h against overflow. 16384 covers
// 8K+ with headroom; anything larger is rejected as a bogus/hostile geometry.
const MAX_DIM: u32 = 16384;

/// Returns true only if `path` canonicalizes to a node directly under /dev/dri/.
/// This is the realpath gate the libdrmtap helper applied but the in-process
/// (direct) path does not, so the service must apply it itself.
fn device_under_dev_dri(path: &str) -> bool {
    match std::fs::canonicalize(path) {
        Ok(p) => p.parent().map_or(false, |d| d == std::path::Path::new("/dev/dri")),
        Err(_) => false,
    }
}

/// An open DRM read context. Not Send/Sync deliberately (the raw ctx is used on
/// one thread, like the old Capturer).
pub struct DrmReader {
    lib: &'static DrmtapLib,
    ctx: *mut drmtap_ctx,
    // grow-once packed-BGRA scratch buffer (preallocated model): resized up to the
    // frame size and never shrunk.
    buf: Vec<u8>,
}

impl DrmReader {
    /// Open the DRM device. `device = None` auto-detects (safe); `Some(path)` is
    /// realpath-gated to /dev/dri/. Returns None if libdrmtap is unavailable
    /// (dlopen failed), the device is not allowed, or the open failed — the
    /// caller then falls back to PipeWire/portal.
    pub fn open(device: Option<&str>) -> Option<DrmReader> {
        let lib = drmtap_dl::get()?;
        let device_cstr = match device {
            None => None,
            Some(d) => {
                if !device_under_dev_dri(d) {
                    log::warn!("DRM device {d:?} is not under /dev/dri; refusing to open");
                    return None;
                }
                match CString::new(d) {
                    Ok(c) => Some(c),
                    Err(_) => return None, // interior NUL
                }
            }
        };
        let cfg = drmtap_config {
            device_path: device_cstr.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            crtc_id: 0,
            helper_path: std::ptr::null(),
            debug: 0,
        };
        // SAFETY: cfg is a valid struct; device_cstr outlives this call.
        let ctx = unsafe { (lib.open)(&cfg) };
        drop(device_cstr);
        if ctx.is_null() {
            log::info!("drmtap_open failed; DRM capture unavailable");
            return None;
        }
        Some(DrmReader {
            lib,
            ctx,
            buf: Vec::new(),
        })
    }

    /// Grab one frame and copy it, tightly packed as BGRA (`w*4*h` bytes), into
    /// the internal buffer. Returns (width, height). The returned slice is valid
    /// until the next grab. A non-32bpp scanout, an oversized/degenerate
    /// geometry, or a stride < w*4 is rejected with a hard error so the caller
    /// falls back to PipeWire (see the codex format finding). Errno failures map
    /// to WouldBlock (retry) or a hard error (tear down) as in the old path.
    pub fn grab(&mut self) -> io::Result<(&[u8], usize, usize)> {
        // SAFETY: self.ctx is a valid context; frame is zeroed before the call
        // and released on every path.
        unsafe {
            let mut frame: drmtap_frame_info = std::mem::zeroed();
            let ret = (self.lib.grab_mapped)(self.ctx, &mut frame);
            if ret < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("drmtap_grab_mapped failed: errno {}", -ret),
                ));
            }
            if frame.data.is_null() || frame.width == 0 || frame.height == 0 {
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let w = frame.width;
            let h = frame.height;
            let stride = frame.stride as usize;
            // 4-bytes-per-pixel-per-row invariant: the row copy reads w*4 bytes
            // from a source that is only stride*height bytes. Reject sub-32bpp /
            // insane geometry to avoid an OOB read (heap disclosure to the peer).
            if w > MAX_DIM || h > MAX_DIM || stride < (w as usize) * 4 {
                log::warn!(
                    "DRM scanout not 32-bit BGRA-compatible ({w}x{h} stride {stride} fourcc {:#010x}); falling back",
                    frame.format
                );
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "unsupported DRM scanout format",
                ));
            }
            let (w, h) = (w as usize, h as usize);
            let frame_size = w * 4 * h;
            if self.buf.len() != frame_size {
                self.buf.resize(frame_size, 0);
            }
            let src = frame.data as *const u8;
            let dst = self.buf.as_mut_ptr();
            if stride == w * 4 {
                std::ptr::copy_nonoverlapping(src, dst, frame_size);
            } else {
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(src.add(y * stride), dst.add(y * w * 4), w * 4);
                }
            }
            (self.lib.frame_release)(self.ctx, &mut frame);
            Ok((&self.buf, w, h))
        }
    }
}

impl Drop for DrmReader {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: ctx came from drmtap_open and is non-null.
            unsafe { (self.lib.close)(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
    }
}
