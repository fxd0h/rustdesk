// DRM/KMS capture backend for RustDesk — powered by libdrmtap
//
// Reads the compositor's scanout directly from the DRM/KMS subsystem without
// involving xdg-desktop-portal.  libdrmtap-sys statically embeds the C sources
// (no shared library to install) and spawns a privileged helper
// (drmtap-helper, CAP_SYS_ADMIN + seccomp) for capture without running
// rustdesk as root.
//
// Multi-monitor: each Display carries its CRTC id and virtual-FB origin (x, y)
// so the Capturer opens the exact CRTC the user selected, not just the primary.
//
// Tested on:
//   - Intel Meteor Lake (i915) dual 3840×2160 — EGL detiling of the compressed
//     INTEL_4_TILED_MTL_RC_CCS_CC framebuffer modifier
//   - virtio-gpu (QEMU/KVM) — linear framebuffer

use crate::{Frame, TraitCapturer};
use std::{io, time::{Duration, Instant}};
use std::sync::atomic::{AtomicU8, Ordering};
use super::x11::PixelBuffer;
use hbb_common::{libc, log};

// FFI bindings to libdrmtap — struct layouts must match drmtap.h exactly!
// Use libdrmtap-sys crate for static linking
use libdrmtap_sys::{
    drmtap_close, drmtap_config, drmtap_ctx, drmtap_cursor_info, drmtap_cursor_release,
    drmtap_display, drmtap_frame_info, drmtap_get_cursor, drmtap_grab_mapped,
    drmtap_frame_release, drmtap_list_displays, drmtap_open,
};
use std::sync::Mutex;

// Latest hardware cursor captured from the DRM cursor plane (via the privileged
// helper). RustDesk's cursor source on Wayland is XFixes, which only reflects
// the X cursor and is stale over native Wayland apps. The DRM cursor plane, in
// contrast, holds the compositor's actual current cursor and updates when the
// shape changes — so we capture it here and feed it to the cursor service.
#[derive(Clone)]
pub struct DrmCursor {
    pub id: u64, // content hash; changes when the cursor shape changes
    pub width: i32,
    pub height: i32,
    pub hotx: i32,
    pub hoty: i32,
    pub colors: Vec<u8>, // RGBA8888
}

static DRM_CURSOR: Mutex<Option<DrmCursor>> = Mutex::new(None);

/// Current hardware cursor captured from the DRM cursor plane, if any.
pub fn drm_cursor() -> Option<DrmCursor> {
    DRM_CURSOR.lock().unwrap().clone()
}

/// Cheap id-only accessor for the ~33ms cursor poll fast path, which only needs
/// the id to detect shape changes — avoids cloning the pixel buffer every poll.
pub fn drm_cursor_id() -> Option<u64> {
    DRM_CURSOR.lock().unwrap().as_ref().map(|c| c.id)
}

// A grab that returns one of these errnos may succeed on retry (no frame ready,
// helper busy, interrupted syscall). Anything else negative is a permanent
// failure — the helper can't run (EACCES), the CRTC is gone (ENODEV), the format
// is unsupported, etc. — and must NOT be retried as WouldBlock, or the capture
// loop spins forever on a blank frame instead of falling back to PipeWire.
fn is_transient_errno(ret: i32) -> bool {
    matches!(-ret, libc::EAGAIN | libc::EBUSY | libc::EINTR | libc::ETIMEDOUT)
}

// Errnos that mean the privileged helper itself cannot run / is not set up
// (execute denied, missing helper). These are fixed for the process lifetime —
// group membership, the file capability, and the install are all decided before
// the process starts — so a probe seeing them is safe to cache. Everything else
// that is neither success nor transient — notably ENODEV (CRTC inactive /
// display asleep under DPMS) — is a topology condition that can clear, and must
// stay retryable, or a monitor that happens to be asleep at startup would leave
// DRM disabled until the process restarts.
fn is_setup_errno(ret: i32) -> bool {
    matches!(-ret, libc::EACCES | libc::EPERM | libc::ENOENT)
}

enum ProbeResult {
    Ok,
    Permanent,  // helper cannot run / not set up (denied, missing) — cache it
    Transient,  // busy, no-frame-yet, or topology (CRTC asleep) — retry later
}

// Whether DRM/KMS capture actually works on this host, i.e. the privileged
// helper is reachable and a scanout grab succeeds.
//
// Display enumeration (drmtap_list_displays) only reads connectors/CRTCs, which
// is unprivileged and succeeds even when the helper cannot run — e.g. the user
// is not in the `rustdesk-capture` group, or `setcap` was never applied. So the
// display list alone is not proof that capture will work. This probes an actual
// grab and caches only *definitive* outcomes for the process lifetime (helper
// reachability and group membership are fixed when the process starts, and we
// must not fork a helper on every enumeration). An inconclusive/transient probe
// is not cached, so a momentary compositor blip at init can't lock DRM off for
// the whole process — it just uses PipeWire this once and re-probes next time.
// The dispatcher (common/linux.rs) gates the DRM path on this so an unusable
// helper falls back to PipeWire/portal cleanly instead of streaming blank.
pub fn capture_available() -> bool {
    match PROBE.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    match unsafe { capture_probe() } {
        ProbeResult::Ok => {
            PROBE.store(1, Ordering::Relaxed);
            log::info!("DRM/KMS capture probe succeeded");
            true
        }
        ProbeResult::Permanent => {
            PROBE.store(2, Ordering::Relaxed);
            log::info!("DRM/KMS capture probe failed (helper unavailable); using PipeWire/portal");
            false
        }
        ProbeResult::Transient => {
            log::debug!(
                "DRM/KMS capture probe inconclusive (transient); using PipeWire/portal for now, will re-probe"
            );
            false
        }
    }
}

// Process-wide DRM availability cache. Written once by the startup probe and
// DOWNGRADED by runtime failures (see `Capturer::frame`): a setup failure sets it
// to 2 (permanently unavailable — the helper can't run at all); any other hard
// grab failure resets it to 0 so the next enumeration re-probes instead of
// trusting a stale OK. 0 = unknown, 1 = ok, 2 = permanently unavailable.
static PROBE: AtomicU8 = AtomicU8::new(0);

// CRTCs that returned a permanent, *attributable* grab failure at runtime (e.g. a
// secondary output whose scanout format can't be captured). Keyed by connector
// name + crtc id — a bare crtc id isn't stable across a replug, so pairing it with
// the connector name avoids a stale id later matching a healthy monitor. Such a
// display is DROPPED from DRM enumeration so the rest keep DRM (whole-session
// fallback to PipeWire is useless on an unattended host — nobody clicks consent).
// Cleared whenever the enumerated topology changes (replug/wake), re-testing all.
// ENODEV (asleep/gone) never enters this set: the enumeration `active` filter
// already excludes those, and bad-marking a nap would outlive it for no benefit.
static BAD_CRTCS: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());
static LAST_TOPOLOGY: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());

fn mark_bad_crtc(name: &str, crtc_id: u32) {
    let mut bad = BAD_CRTCS.lock().unwrap();
    if !bad.iter().any(|(n, c)| n == name && *c == crtc_id) {
        bad.push((name.to_string(), crtc_id));
    }
}

fn is_bad_crtc(name: &str, crtc_id: u32) -> bool {
    BAD_CRTCS
        .lock()
        .unwrap()
        .iter()
        .any(|(n, c)| n == name && *c == crtc_id)
}

// A replug/wake/reconfigure changes the enumerated (connector, crtc) set; when it
// does, forget past per-CRTC failures and re-test everything.
fn reset_bad_crtcs_on_topology_change(current: &[(String, u32)]) {
    let mut last = LAST_TOPOLOGY.lock().unwrap();
    if last.as_slice() != current {
        BAD_CRTCS.lock().unwrap().clear();
        *last = current.to_vec();
    }
}

// Build a drmtap_config for the given CRTC. Returns the owned DRM_DEVICE CString
// alongside the config that borrows it: the caller MUST keep the CString alive
// for as long as it uses the config (bind it, don't drop it). Centralised so the
// three open sites (probe, enumerate, capture) can't drift on device/debug
// handling. DRM_DEVICE is user-controlled, so an interior NUL is treated as unset
// (null device_path) rather than panicking.
fn build_drm_config(crtc_id: u32) -> (Option<std::ffi::CString>, drmtap_config) {
    let device_cstr = std::env::var("DRM_DEVICE")
        .ok()
        .and_then(|s| std::ffi::CString::new(s).ok());
    let cfg = drmtap_config {
        device_path: device_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null()),
        crtc_id,
        helper_path: std::ptr::null(),
        debug: if std::env::var("DRMTAP_DEBUG").is_ok() { 1 } else { 0 },
    };
    (device_cstr, cfg)
}

// Open a context on the auto-selected active CRTC (crtc_id 0) and attempt a
// single grab, tolerating a few transient errors before the first frame.
unsafe fn capture_probe() -> ProbeResult {
    let (_device_cstr, cfg) = build_drm_config(0);
    let ctx = drmtap_open(&cfg);
    if ctx.is_null() {
        // open() can fail because there is no active CRTC yet (DPMS/topology)
        // just as easily as for a real setup problem, so don't cache it — the
        // next enumeration re-probes.
        return ProbeResult::Transient;
    }
    let mut result = ProbeResult::Transient;
    for _ in 0..8 {
        let mut frame: drmtap_frame_info = std::mem::zeroed();
        let ret = drmtap_grab_mapped(ctx, &mut frame);
        if ret == 0 {
            drmtap_frame_release(ctx, &mut frame);
            result = ProbeResult::Ok;
            break;
        }
        if is_transient_errno(ret) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        // Non-transient: cache only a genuine helper/setup failure; leave a
        // topology failure (ENODEV = CRTC asleep/gone) retryable so a display
        // that is merely asleep at startup doesn't disable DRM for good.
        result = if is_setup_errno(ret) {
            ProbeResult::Permanent
        } else {
            ProbeResult::Transient
        };
        break;
    }
    drmtap_close(ctx);
    result
}

pub struct Display {
    name: String,
    // Logical origin in the compositor's coordinate space (from the matching
    // Wayland output). Falls back to the physical CRTC offset when no compositor
    // output matches (e.g. capturing the login screen with no compositor).
    x: i32,
    y: i32,
    // Physical pixel size of the captured framebuffer (DRM mode).
    w: usize,
    h: usize,
    // Logical size + scale (physical/logical) from the matching Wayland output;
    // fall back to physical size and scale 1.0 when unknown.
    logical_w: usize,
    logical_h: usize,
    scale: f64,
    crtc_id: u32,
    primary: bool,
}

// Match a DRM connector to its Wayland output (by connector name) to obtain the
// compositor's LOGICAL geometry and scale factor.
//
// Rationale: video is captured by DRM in PHYSICAL pixels, but RustDesk's Wayland
// input path injects the cursor in the compositor's LOGICAL coordinate space
// (the uinput device range and the per-display `scale`/`origin` reported to the
// peer are all logical). If the DRM backend reported scale 1.0 and the physical
// origin, the client would send physical coordinates while uinput expects logical
// ones, mis-mapping the cursor under fractional/HiDPI scaling or multi-monitor
// layouts. Matching the Wayland output keeps both coordinate systems consistent
// for any client/server configuration.
//
// Falls back to the physical geometry (scale 1.0) when no compositor output
// matches — e.g. an X11 session, or the GDM/SDDM login screen with no compositor.
// Normalize a connector name so DRM and compositor naming line up. DRM exposes
// sub-typed names like "HDMI-A-1" / "DVI-I-1", while compositors (e.g. Mutter)
// often shorten them to "HDMI-1" / "DVI-1". We collapse to "<type>-<index>" by
// dropping any middle segments, and lowercase for a case-insensitive compare.
fn normalize_connector(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [] => name.to_lowercase(),
        [single] => single.to_lowercase(),
        // Keep the leading type and the trailing index, drop the middle (the
        // sub-type letter such as the "A" in "HDMI-A-1").
        [first, .., last] => format!("{}-{}", first, last).to_lowercase(),
    }
}

fn logical_geometry_for(
    name: &str,
    phys_x: i32,
    phys_y: i32,
    phys_w: usize,
    phys_h: usize,
) -> (i32, i32, usize, usize, f64) {
    let displays = crate::wayland::display::get_displays();
    // Prefer an exact name match; fall back to a normalized connector match.
    let want = normalize_connector(name);
    let matched = displays
        .displays
        .iter()
        .find(|d| d.name == name)
        .or_else(|| {
            displays
                .displays
                .iter()
                .find(|d| normalize_connector(&d.name) == want)
        });
    if let Some(d) = matched {
        let (lw, lh) = d.logical_size.unwrap_or((d.width, d.height));
        let lw = (lw.max(1)) as usize;
        let lh = (lh.max(1)) as usize;
        let scale = if d.width > 0 {
            d.width as f64 / lw as f64
        } else {
            1.0
        };
        return (d.x, d.y, lw, lh, scale);
    }
    // No matching compositor output — use physical geometry, no scaling.
    // (e.g. an X11 session, or a login screen with no compositor running.)
    (phys_x, phys_y, phys_w, phys_h, 1.0)
}

/// Index of the display matching the compositor's primary output (by connector
/// name), or 0 if the primary is unknown or doesn't match an enumerated
/// connector. Keeps the DRM primary consistent with the Wayland/compositor
/// primary instead of just libdrmtap's enumeration order.
fn primary_display_index(displays: &[Display]) -> usize {
    #[cfg(feature = "wayland")]
    if let Some(name) = crate::wayland::display::get_primary_monitor() {
        if let Some(idx) = displays.iter().position(|d| d.name == name) {
            return idx;
        }
        // The compositor often shortens connector names (Mutter reports "HDMI-1"
        // while DRM enumerates "HDMI-A-1"), so an exact compare misses. Fall back
        // to the same normalized match used for logical geometry before giving up.
        let want = normalize_connector(&name);
        if let Some(idx) = displays
            .iter()
            .position(|d| normalize_connector(&d.name) == want)
        {
            return idx;
        }
        log::debug!(
            "DRM: compositor primary '{name}' not among enumerated connectors; using first"
        );
    }
    let _ = displays;
    0
}

impl Display {
    pub fn all() -> io::Result<Vec<Display>> {
        // SAFETY: All FFI calls use valid pointers and check return values.
        // The drmtap context is opened and closed within this function scope.
        unsafe {
            let (_device_cstr, cfg) = build_drm_config(0);
            let ctx = drmtap_open(&cfg);
            if ctx.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "drmtap_open failed",
                ));
            }

            let mut raw_displays = vec![std::mem::zeroed::<drmtap_display>(); 8];
            let cap = raw_displays.len() as i32;
            let n = drmtap_list_displays(ctx, raw_displays.as_mut_ptr(), cap);
            drmtap_close(ctx);

            if n <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "No DRM displays found",
                ));
            }

            // drmtap_list_displays returns the total connected-connector count,
            // which may exceed the buffer capacity (only `cap` entries are
            // written). Clamp before indexing so >8 connectors can't read past
            // the end of the Vec.
            let count = (n as usize).min(raw_displays.len());

            let mut idxs: Vec<usize> =
                (0..count).filter(|&i| raw_displays[i].active != 0).collect();
            if idxs.is_empty() {
                // nvidia-drm doesn't flag the connector active via the legacy API
                // even with a live scanout; fall back to all enumerated displays
                // and let the capturer auto-select the active CRTC (crtc_id 0).
                idxs = (0..count).collect();
            }
            let mut displays: Vec<Display> = idxs
                .into_iter()
                .map(|i| {
                    let name_bytes: Vec<u8> = raw_displays[i]
                        .name
                        .iter()
                        .take_while(|&&c| c != 0)
                        .map(|&c| c as u8)
                        .collect();
                    let name = String::from_utf8_lossy(&name_bytes).to_string();
                    let phys_w = raw_displays[i].width as usize;
                    let phys_h = raw_displays[i].height as usize;
                    let (ox, oy, logical_w, logical_h, scale) = logical_geometry_for(
                        &name,
                        raw_displays[i].x as i32,
                        raw_displays[i].y as i32,
                        phys_w,
                        phys_h,
                    );
                    Display {
                        name,
                        x: ox,
                        y: oy,
                        w: phys_w,
                        h: phys_h,
                        logical_w,
                        logical_h,
                        scale,
                        crtc_id: raw_displays[i].crtc_id,
                        primary: false,
                    }
                })
                .collect();

            if displays.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "No active DRM displays",
                ));
            }

            // Drop CRTCs that a runtime grab marked permanently ungrabbable, so the
            // rest keep DRM rather than the whole session dropping to PipeWire
            // (useless unattended). Clear the set first when the enumerated topology
            // changed, so a replug/wake re-tests everything.
            let topology: Vec<(String, u32)> =
                displays.iter().map(|d| (d.name.clone(), d.crtc_id)).collect();
            reset_bad_crtcs_on_topology_change(&topology);
            displays.retain(|d| !is_bad_crtc(&d.name, d.crtc_id));
            if displays.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "All DRM CRTCs ungrabbable; falling back to PipeWire",
                ));
            }

            // Mark the primary to match the compositor's primary output (by
            // connector name) rather than libdrmtap's enumeration order. Falls
            // back to the first display if the compositor primary is unknown or
            // its name doesn't match an enumerated connector.
            let primary_idx = primary_display_index(&displays);
            if let Some(d) = displays.get_mut(primary_idx) {
                d.primary = true;
            }

            Ok(displays)
        }
    }

    pub fn primary() -> io::Result<Display> {
        let mut all = Self::all()?;
        let idx = all.iter().position(|d| d.primary).unwrap_or(0);
        Ok(all.remove(idx))
    }

    pub fn width(&self) -> usize { self.w }
    pub fn height(&self) -> usize { self.h }
    pub fn scale(&self) -> f64 { self.scale }
    pub fn logical_width(&self) -> usize { self.logical_w }
    pub fn logical_height(&self) -> usize { self.logical_h }
    pub fn origin(&self) -> (i32, i32) { (self.x, self.y) }
    pub fn is_online(&self) -> bool { true }
    pub fn is_primary(&self) -> bool { self.primary }
    pub fn name(&self) -> String { self.name.clone() }
}

pub struct Capturer {
    ctx: *mut drmtap_ctx,
    w: usize,
    h: usize,
    buffer: Vec<u8>,
    frame_count: u64,
    cursor_tick: u64,
    last_grab_time: Instant,
    // Identity of the CRTC being captured, so a runtime grab failure can be
    // attributed to a specific display (see frame()). crtc_id 0 = auto-select,
    // not a stable identity, so failures on it are treated as global, not per-CRTC.
    name: String,
    crtc_id: u32,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        // SAFETY: FFI call to drmtap_open with valid config struct.
        // The returned pointer is checked for null before use.
        unsafe {
            let (_device_cstr, cfg) = build_drm_config(display.crtc_id);
            let ctx = drmtap_open(&cfg);
            if ctx.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "drmtap_open failed",
                ));
            }
            Ok(Capturer {
                ctx,
                w: display.w,
                h: display.h,
                buffer: Vec::new(),
                frame_count: 0,
                cursor_tick: 0,
                last_grab_time: Instant::now(),
                name: display.name,
                crtc_id: display.crtc_id,
            })
        }
    }

    pub fn width(&self) -> usize { self.w }
    pub fn height(&self) -> usize { self.h }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, _timeout: Duration) -> io::Result<Frame<'a>> {
        // SAFETY: All FFI calls use the valid self.ctx pointer (checked non-null
        // in new()). Frame data pointer is validated before dereferencing.
        // drmtap_frame_release is always called before returning.
        unsafe {
            // Rate limit: minimum 16ms between grabs (~60 FPS max)
            let elapsed = self.last_grab_time.elapsed();
            let min_interval = Duration::from_millis(16);
            if elapsed < min_interval {
                std::thread::sleep(min_interval - elapsed);
            }

            let mut frame: drmtap_frame_info = std::mem::zeroed();
            let ret = drmtap_grab_mapped(self.ctx, &mut frame);
            if ret < 0 {
                if is_transient_errno(ret) {
                    std::thread::sleep(Duration::from_millis(16));
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                // Non-transient hard failure. Classify so the next enumeration
                // does the right thing, then surface a hard error so the capture
                // loop tears down now instead of spinning WouldBlock on a frame
                // that will never arrive:
                //   - setup errno (helper can't run at all) -> permanently
                //     unavailable; PROBE=2 so DRM is not re-selected this process.
                //   - otherwise -> reset PROBE so the next enumeration re-probes
                //     (a healthy primary comes right back), and if the failure is
                //     attributable to a specific CRTC (not ENODEV/topology, and a
                //     real crtc id, not auto-select 0), mark that CRTC bad so it is
                //     dropped from DRM enumeration while the rest keep DRM.
                if is_setup_errno(ret) {
                    PROBE.store(2, Ordering::Relaxed);
                } else {
                    PROBE.store(0, Ordering::Relaxed);
                    if -ret != libc::ENODEV && self.crtc_id != 0 {
                        log::warn!(
                            "DRM/KMS: grab on '{}' (crtc {}) failed permanently (errno {}); dropping it from DRM, keeping DRM for the rest",
                            self.name, self.crtc_id, -ret
                        );
                        mark_bad_crtc(&self.name, self.crtc_id);
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("drmtap_grab_mapped failed: errno {}", -ret),
                ));
            }

            if frame.data.is_null() || frame.width == 0 || frame.height == 0 {
                drmtap_frame_release(self.ctx, &mut frame);
                std::thread::sleep(Duration::from_millis(16));
                return Err(io::ErrorKind::WouldBlock.into());
            }

            self.last_grab_time = Instant::now();

            // Poll the hardware cursor plane independently of framebuffer changes.
            // The cursor plane is a separate DRM plane — its shape can change even
            // when the scanout framebuffer is unchanged (e.g. while the desktop is
            // idle but the user is moving the mouse).
            self.cursor_tick += 1;
            if self.cursor_tick % 4 == 0 {
                self.update_cursor();
            }

            // NB: we do NOT skip on an unchanged fb_id. A constant KMS framebuffer
            // id does not imply constant pixels — a compositor can re-render into
            // (or reuse) the current scanout buffer without allocating a new one,
            // so gating the copy on fb_id froze the remote display after the first
            // frame on those compositors. Deliver every grabbed frame and let the
            // encoder collapse static content into near-empty inter-frames; the
            // 16ms rate limit above (plus adaptive QoS FPS) bounds the cost.
            let w = frame.width as usize;
            let h = frame.height as usize;
            let stride = frame.stride as usize;
            let frame_size = w * 4 * h;

            if self.buffer.len() != frame_size {
                self.buffer.resize(frame_size, 0);
            }

            let src = frame.data as *const u8;
            if stride == w * 4 {
                std::ptr::copy_nonoverlapping(src, self.buffer.as_mut_ptr(), frame_size);
            } else {
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(
                        src.add(y * stride),
                        self.buffer.as_mut_ptr().add(y * w * 4),
                        w * 4,
                    );
                }
            }

            drmtap_frame_release(self.ctx, &mut frame);

            self.frame_count += 1;
            self.w = w;
            self.h = h;
            Ok(Frame::PixelBuffer(PixelBuffer::new(
                &self.buffer,
                crate::Pixfmt::BGRA,
                w,
                h,
            )))
        }
    }

}

impl Capturer {
    // Capture the hardware cursor from the DRM cursor plane and update DRM_CURSOR.
    // The cursor plane is independent of the scanout framebuffer, so this is called
    // on every cursor_tick even when the framebuffer hasn't changed.
    unsafe fn update_cursor(&mut self) {
        let mut c: drmtap_cursor_info = std::mem::zeroed();
        let cret = drmtap_get_cursor(self.ctx, &mut c);
        if cret == 0
            && c.visible != 0
            && !c.pixels.is_null()
            && c.width > 0
            && c.height > 0
            && (c.width as i64) * (c.height as i64) <= 256 * 256
        {
            let cw = c.width as i32;
            let ch = c.height as i32;
            let n = (cw * ch) as usize;
            let src = std::slice::from_raw_parts(c.pixels, n);
            let mut hash: u64 = 1469598103934665603;
            let mut colors = Vec::with_capacity(n * 4);
            let (mut minx, mut miny, mut maxx, mut maxy) = (cw, ch, -1i32, -1i32);
            for (i, &p) in src.iter().enumerate() {
                let a = ((p >> 24) & 0xff) as u8;
                let r = ((p >> 16) & 0xff) as u8;
                let g = ((p >> 8) & 0xff) as u8;
                let b = (p & 0xff) as u8;
                colors.push(r);
                colors.push(g);
                colors.push(b);
                colors.push(a);
                hash ^= p as u64;
                hash = hash.wrapping_mul(1099511628211);
                if a >= 128 {
                    let x = (i as i32) % cw;
                    let y = (i as i32) / cw;
                    if x < minx { minx = x; }
                    if x > maxx { maxx = x; }
                    if y < miny { miny = y; }
                    if y > maxy { maxy = y; }
                }
            }
            let (hotx, hoty) = if c.hot_x != 0 || c.hot_y != 0 {
                (c.hot_x, c.hot_y)
            } else if maxx >= minx && maxy >= miny {
                let bw = maxx - minx + 1;
                let bh = maxy - miny + 1;
                if bh > bw * 2 {
                    ((minx + maxx) / 2, (miny + maxy) / 2)
                } else {
                    (minx, miny)
                }
            } else {
                (0, 0)
            };
            let mut lock = DRM_CURSOR.lock().unwrap();
            let changed = lock.as_ref().map_or(true, |old| old.id != hash);
            if changed {
                *lock = Some(DrmCursor {
                    id: hash,
                    width: cw,
                    height: ch,
                    hotx,
                    hoty,
                    colors,
                });
            }
        }
        drmtap_cursor_release(self.ctx, &mut c);
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: ctx was obtained from drmtap_open and is non-null.
            unsafe { drmtap_close(self.ctx); }
            self.ctx = std::ptr::null_mut();
        }
    }
}
