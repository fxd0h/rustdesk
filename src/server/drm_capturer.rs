// Server-side (`--server`, unprivileged) consumer of the root `--service`'s DRM/KMS capture stream.
//
// The architecture pivot moved the scanout read into the root service; this process no longer
// links or dlopens libdrmtap. It connects to the service's `_drm` channel, learns the display
// geometry from the service, and pulls packed-BGRA frames. This mirrors the Windows
// `portable_service` CapturerPortable split (a privileged process captures, this process presents),
// but over rustdesk's own IPC instead of shared memory.
//
// `TraitCapturer::frame()` is synchronous (the encoder loop calls it) while the IPC receive is
// async, so a dedicated background thread runs the receive loop and keeps only the newest frame
// (latest-wins, so a slow encoder never backs the socket up). `frame()` returns that frame as a
// borrowed `PixelBuffer`, `WouldBlock` when nothing new arrived within the timeout, and a hard
// `Err` once the stream ends (the caller then rebuilds the capturer or falls back to PipeWire).

use crate::ipc::{connect_drm, Data, DrmDisplayInfo};
use hbb_common::{anyhow::anyhow, log, message_proto::DisplayInfo, tokio, ResultType};
use scrap::{Frame, Pixfmt, PixelBuffer, TraitCapturer};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// Upper bound on how long `new()` waits for the service to answer with the display list before
// giving up and letting the caller fall back.
const HANDSHAKE_TIMEOUT_MS: u64 = 3000;

struct FrameSlot {
    // (width, height, packed-BGRA) of the newest frame not yet consumed by `frame()`; latest-wins.
    latest: Option<(usize, usize, Vec<u8>)>,
    // Set once the stream ends so `frame()` returns a hard error (triggers a capturer rebuild).
    ended: Option<String>,
}

struct Shared {
    slot: Mutex<FrameSlot>,
    cv: Condvar,
}

pub struct IpcDrmCapturer {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    // The buffer `frame()` hands out a borrow of; kept across calls (grow-once) and only replaced
    // when a new frame is taken from the slot.
    cur: Vec<u8>,
    cur_w: usize,
    cur_h: usize,
}

impl IpcDrmCapturer {
    /// Connect to the service `_drm` channel, complete the handshake (receive the display list, then
    /// request `display`), and start streaming on a background thread. Returns the capturer plus the
    /// enumerated displays so the caller can populate `display_service`. `Err` if the service has no
    /// DRM capture available or the handshake fails — the caller then falls back to PipeWire/portal.
    pub fn new(display: i32) -> ResultType<(IpcDrmCapturer, Vec<DrmDisplayInfo>)> {
        let shared = Arc::new(Shared {
            slot: Mutex::new(FrameSlot {
                latest: None,
                ended: None,
            }),
            cv: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<ResultType<Vec<DrmDisplayInfo>>>();
        {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::spawn(move || recv_thread(display, shared, stop, tx));
        }
        let displays = rx
            .recv_timeout(Duration::from_millis(HANDSHAKE_TIMEOUT_MS + 500))
            .map_err(|_| anyhow!("drm capture handshake timed out"))??;
        Ok((
            IpcDrmCapturer {
                shared,
                stop,
                cur: Vec::new(),
                cur_w: 0,
                cur_h: 0,
            },
            displays,
        ))
    }
}

impl Drop for IpcDrmCapturer {
    fn drop(&mut self) {
        // Signal the receive thread to exit; it also exits on its own when the connection drops.
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl TraitCapturer for IpcDrmCapturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        let deadline = Instant::now() + timeout;
        {
            let mut slot = self.shared.slot.lock().unwrap();
            loop {
                if slot.latest.is_some() || slot.ended.is_some() {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let (guard, _timed_out) =
                    self.shared.cv.wait_timeout(slot, deadline - now).unwrap();
                slot = guard;
            }
            // Deliver a pending frame before surfacing an end, so the last frame is not dropped.
            if let Some((w, h, buf)) = slot.latest.take() {
                drop(slot);
                self.cur = buf;
                self.cur_w = w;
                self.cur_h = h;
            } else {
                let err = slot
                    .ended
                    .clone()
                    .unwrap_or_else(|| "drm stream ended".to_owned());
                return Err(io::Error::new(io::ErrorKind::Other, err));
            }
        }
        Ok(Frame::PixelBuffer(PixelBuffer::new(
            &self.cur,
            Pixfmt::BGRA,
            self.cur_w,
            self.cur_h,
        )))
    }
}

// Background receive loop. Owns the `_drm` connection and the async runtime; keeps the newest frame
// in `shared.slot`. Runs on its own thread because `frame()` is sync and one blocking consumer is
// enough for DRM.
#[tokio::main(flavor = "current_thread")]
async fn recv_thread(
    display: i32,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<ResultType<Vec<DrmDisplayInfo>>>,
) {
    // Handshake: connect, receive the display list, request the display.
    let mut conn = match connect_drm(1000).await {
        Ok(c) => c,
        Err(err) => {
            let _ = tx.send(Err(err));
            return;
        }
    };
    let displays = match conn.next_timeout(HANDSHAKE_TIMEOUT_MS).await {
        Ok(Some(Data::DrmDisplayList(v))) => v,
        Ok(other) => {
            let _ = tx.send(Err(anyhow!("expected DrmDisplayList, got {:?}", other)));
            return;
        }
        Err(err) => {
            let _ = tx.send(Err(err));
            return;
        }
    };
    if let Err(err) = conn.send(&Data::DrmStart { display }).await {
        let _ = tx.send(Err(err));
        return;
    }
    let _ = tx.send(Ok(displays));

    // Stream until stopped or the connection ends.
    let end_reason = loop {
        if stop.load(Ordering::SeqCst) {
            break "stopped".to_owned();
        }
        match conn.next().await {
            Ok(Some(Data::DrmFrame { width, height })) => match conn.next_raw().await {
                Ok(raw) => {
                    let mut slot = shared.slot.lock().unwrap();
                    slot.latest = Some((width as usize, height as usize, raw.to_vec()));
                    shared.cv.notify_one();
                }
                Err(err) => break format!("frame body: {err}"),
            },
            Ok(Some(Data::DrmCursor {
                id,
                width,
                height,
                hotx,
                hoty,
            })) => match conn.next_raw().await {
                Ok(raw) => set_drm_cursor(DrmCursorData {
                    id,
                    width: width as i32,
                    height: height as i32,
                    hotx,
                    hoty,
                    colors: raw.to_vec(),
                }),
                Err(err) => break format!("cursor body: {err}"),
            },
            Ok(Some(_)) => {} // ignore any unexpected control message
            Ok(None) => break "desynchronized frame".to_owned(),
            Err(err) => break format!("recv: {err}"),
        }
    };
    log::info!("drm capture stream ended: {end_reason}");
    // Drop the last hardware-cursor snapshot so it does not linger after teardown.
    clear_drm_cursor();
    let mut slot = shared.slot.lock().unwrap();
    slot.ended = Some(format!("drm stream ended ({end_reason})"));
    shared.cv.notify_one();
}

// The latest DRM hardware-cursor snapshot, published by recv_thread and read by the cursor service
// (platform::linux::get_cursor / get_cursor_data). One global shared by all active streams: a
// multi-monitor client runs one recv_thread per display, and the hardware cursor lives on whichever
// CRTC the pointer is over (the others report the hidden sentinel), so last-writer-wins here tracks
// the pointer as it moves between monitors. Per-CRTC cursor routing is a later refinement.
#[derive(Clone)]
pub struct DrmCursorData {
    pub id: u64,
    pub width: i32,
    pub height: i32,
    pub hotx: i32,
    pub hoty: i32,
    pub colors: Vec<u8>,
}

static DRM_CURSOR: Mutex<Option<DrmCursorData>> = Mutex::new(None);

fn set_drm_cursor(c: DrmCursorData) {
    *DRM_CURSOR.lock().unwrap() = Some(c);
}

fn clear_drm_cursor() {
    *DRM_CURSOR.lock().unwrap() = None;
}

/// The id of the latest DRM hardware cursor (None if no stream/cursor). The cursor service polls
/// this to detect shape changes (a change triggers a `get_cursor_data` fetch).
pub fn drm_cursor_id() -> Option<u64> {
    DRM_CURSOR.lock().unwrap().as_ref().map(|c| c.id)
}

/// The latest DRM hardware-cursor snapshot (RGBA), or None.
pub fn drm_cursor() -> Option<DrmCursorData> {
    DRM_CURSOR.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Server capture-path integration (the parallel, gated DRM path)
//
// The `--server` selects DRM/KMS capture over PipeWire when the root service offers the `_drm`
// channel. Availability + the display list are probed once and cached: the `_drm` listener now
// serves consumers concurrently (one connection per captured display), but re-probing on every
// enumeration still churns connections needlessly and briefly tripped a restart loop in testing, so
// the result is cached durably. The cache is seeded before capture starts (display enumeration) and
// by the capturer handshake, and only reset by `clear()` on teardown.
// ---------------------------------------------------------------------------

enum ProbeState {
    Unknown,
    Unavailable,
    Available(Vec<DrmDisplayInfo>),
}

static DRM_STATE: Mutex<ProbeState> = Mutex::new(ProbeState::Unknown);

/// Query the service for the current DRM display list without starting a stream: connect, read the
/// list the service sends on connect, then drop the connection (the service closes it when we do
/// not send `DrmStart`). Runs the async work on a throwaway thread so it is safe to call from any
/// context (a nested `#[tokio::main]` would panic when called from inside a runtime).
fn query_displays() -> ResultType<Vec<DrmDisplayInfo>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(query_displays_async());
    });
    rx.recv_timeout(Duration::from_millis(HANDSHAKE_TIMEOUT_MS + 1000))
        .map_err(|_| anyhow!("drm display query timed out"))?
}

#[tokio::main(flavor = "current_thread")]
async fn query_displays_async() -> ResultType<Vec<DrmDisplayInfo>> {
    let mut conn = connect_drm(1000).await?;
    match conn.next_timeout(HANDSHAKE_TIMEOUT_MS).await? {
        Some(Data::DrmDisplayList(v)) => Ok(v),
        other => Err(anyhow!("expected DrmDisplayList, got {:?}", other)),
    }
}

/// Whether the root service offers DRM/KMS capture. Probed once and cached (both the positive and
/// negative result) until `clear()`.
pub(super) fn is_available() -> bool {
    // Serialize the probe under the lock: the enumeration hooks all call this, and a burst of
    // concurrent callers would otherwise open several redundant `_drm` probe connections. Holding
    // the lock does exactly one probe; it is fast (the listener answers the display list on connect)
    // and the result is cached durably.
    let mut st = DRM_STATE.lock().unwrap();
    match &*st {
        ProbeState::Available(_) => true,
        ProbeState::Unavailable => false,
        ProbeState::Unknown => match query_displays() {
            Ok(list) if !list.is_empty() => {
                *st = ProbeState::Available(list);
                true
            }
            Ok(_) => {
                *st = ProbeState::Unavailable;
                false
            }
            Err(err) => {
                log::debug!("drm capture not available: {err}");
                *st = ProbeState::Unavailable;
                false
            }
        },
    }
}

/// The cached DRM displays as protobuf `DisplayInfo` (physical geometry; logical scale needs the
/// user session so it stays 1.0 for now — refined in P6). `None` until probed/available.
pub(super) fn get_display_infos() -> Option<Vec<DisplayInfo>> {
    match &*DRM_STATE.lock().unwrap() {
        ProbeState::Available(list) => Some(list.iter().map(display_info_from_drm).collect()),
        _ => None,
    }
}

/// Reset the probe cache so the next session re-probes (called on capture teardown).
pub(super) fn clear() {
    *DRM_STATE.lock().unwrap() = ProbeState::Unknown;
}

fn display_info_from_drm(d: &DrmDisplayInfo) -> DisplayInfo {
    let original_resolution =
        super::display_service::get_original_resolution(&d.name, d.width as usize, d.height as usize);
    DisplayInfo {
        x: d.x,
        y: d.y,
        width: d.width as i32,
        height: d.height as i32,
        name: d.name.clone(),
        online: d.active,
        cursor_embedded: false,
        original_resolution,
        scale: 1.0,
        ..Default::default()
    }
}

/// Build a `CapturerInfo` backed by a DRM-IPC capturer for `display_idx`, refreshing the cached
/// display list from the capturer's handshake so mid-capture enumeration uses fresh geometry.
pub(super) fn get_capturer_info(
    display_idx: usize,
) -> ResultType<super::video_service::CapturerInfo> {
    let (capturer, displays) = IpcDrmCapturer::new(display_idx as i32)?;
    let ndisplay = displays.len();
    let d = displays
        .get(display_idx)
        .ok_or_else(|| anyhow!("drm display index {display_idx} out of range ({ndisplay})"))?
        .clone();
    *DRM_STATE.lock().unwrap() = ProbeState::Available(displays);
    Ok(super::video_service::CapturerInfo {
        origin: (d.x, d.y),
        width: d.width as usize,
        height: d.height as usize,
        ndisplay,
        current: display_idx,
        privacy_mode_id: 0,
        _capturer_privacy_mode_id: 0,
        capturer: Box::new(capturer),
    })
}
