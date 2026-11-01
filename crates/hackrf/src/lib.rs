//! Minimal safe wrapper over the system libhackrf.
//!
//! [`HackRf::transmit`] takes a [`TxPermit`] and tunes to the permit's frequency; there is
//! no API that tunes the transmitter to an arbitrary frequency. [`HackRf::receive`] tunes
//! freely - receiving needs no authorization.

use sdrtx_core::policy::TxPermit;
use std::ffi::{CStr, c_int, c_void};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    #[repr(C)]
    pub struct hackrf_device {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct hackrf_transfer {
        pub device: *mut hackrf_device,
        pub buffer: *mut u8,
        pub buffer_length: c_int,
        pub valid_length: c_int,
        pub rx_ctx: *mut c_void,
        pub tx_ctx: *mut c_void,
    }

    pub type SampleBlockCb = unsafe extern "C" fn(*mut hackrf_transfer) -> c_int;

    unsafe extern "C" {
        pub fn hackrf_init() -> c_int;
        pub fn hackrf_exit() -> c_int;
        pub fn hackrf_open(device: *mut *mut hackrf_device) -> c_int;
        pub fn hackrf_close(device: *mut hackrf_device) -> c_int;
        pub fn hackrf_start_tx(
            device: *mut hackrf_device,
            cb: SampleBlockCb,
            ctx: *mut c_void,
        ) -> c_int;
        pub fn hackrf_stop_tx(device: *mut hackrf_device) -> c_int;
        pub fn hackrf_start_rx(
            device: *mut hackrf_device,
            cb: SampleBlockCb,
            ctx: *mut c_void,
        ) -> c_int;
        pub fn hackrf_stop_rx(device: *mut hackrf_device) -> c_int;
        pub fn hackrf_set_lna_gain(device: *mut hackrf_device, value: u32) -> c_int;
        pub fn hackrf_set_vga_gain(device: *mut hackrf_device, value: u32) -> c_int;
        pub fn hackrf_set_freq(device: *mut hackrf_device, freq_hz: u64) -> c_int;
        pub fn hackrf_set_sample_rate(device: *mut hackrf_device, freq_hz: f64) -> c_int;
        pub fn hackrf_compute_baseband_filter_bw(bandwidth_hz: u32) -> u32;
        pub fn hackrf_set_baseband_filter_bandwidth(
            device: *mut hackrf_device,
            bw_hz: u32,
        ) -> c_int;
        pub fn hackrf_set_amp_enable(device: *mut hackrf_device, value: u8) -> c_int;
        pub fn hackrf_set_txvga_gain(device: *mut hackrf_device, value: u32) -> c_int;
        pub fn hackrf_set_antenna_enable(device: *mut hackrf_device, value: u8) -> c_int;
        pub fn hackrf_error_name(errcode: c_int) -> *const c_char;
    }
}

/// HackRF TX IF gain range, dB.
pub const TXVGA_MAX_DB: u32 = 47;

#[derive(Debug, Clone)]
pub struct Error {
    pub call: &'static str,
    pub code: i32,
    pub name: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {} ({})", self.call, self.name, self.code)
    }
}

impl std::error::Error for Error {}

fn check(call: &'static str, code: c_int) -> Result<(), Error> {
    if code == 0 {
        return Ok(());
    }
    // SAFETY: hackrf_error_name returns a static string for any code.
    let name = unsafe { CStr::from_ptr(ffi::hackrf_error_name(code)) }
        .to_string_lossy()
        .into_owned();
    Err(Error { call, code, name })
}

pub struct HackRf {
    dev: *mut ffi::hackrf_device,
}

// The device handle may move between threads; libhackrf serializes access internally.
unsafe impl Send for HackRf {}

impl HackRf {
    /// Open the first HackRF found.
    pub fn open() -> Result<Self, Error> {
        // SAFETY: plain libhackrf calls; hackrf_init is reference counted by the library.
        unsafe {
            check("hackrf_init", ffi::hackrf_init())?;
            let mut dev = std::ptr::null_mut();
            if let Err(e) = check("hackrf_open", ffi::hackrf_open(&mut dev)) {
                ffi::hackrf_exit();
                return Err(e);
            }
            Ok(Self { dev })
        }
    }

    /// Stream cs8 blocks from `blocks` until the sender hangs up and everything queued has
    /// been sent, `stop()` returns true, or `timeout` (the time-out timer) expires.
    pub fn transmit(
        &mut self,
        permit: &TxPermit,
        cfg: &TxConfig,
        blocks: Receiver<Vec<i8>>,
        stop: &dyn Fn() -> bool,
        timeout: Duration,
    ) -> Result<TxStats, Error> {
        let d = self.dev;
        // SAFETY: d is a live handle owned by self.
        unsafe {
            // Never put DC on the antenna port: the RX chain shares it with a USB-powered LNA.
            check("set_antenna_enable", ffi::hackrf_set_antenna_enable(d, 0))?;
            check(
                "set_sample_rate",
                ffi::hackrf_set_sample_rate(d, f64::from(cfg.sample_rate)),
            )?;
            let bw = ffi::hackrf_compute_baseband_filter_bw((cfg.sample_rate as f32 * 0.75) as u32);
            check(
                "set_baseband_filter_bandwidth",
                ffi::hackrf_set_baseband_filter_bandwidth(d, bw),
            )?;
            check("set_freq", ffi::hackrf_set_freq(d, permit.freq_hz()))?;
            check(
                "set_amp_enable",
                ffi::hackrf_set_amp_enable(d, u8::from(permit.internal_amp())),
            )?;
            check(
                "set_txvga_gain",
                ffi::hackrf_set_txvga_gain(d, cfg.txvga_db.min(TXVGA_MAX_DB)),
            )?;
        }

        // Queue up what libhackrf will ask for immediately (its ~4 in-flight transfers) so
        // key-up does not start with a burst of underruns (zero IQ = no carrier).
        let prefill = 4 * 262_144;
        let mut current = Vec::with_capacity(prefill);
        let deadline = Instant::now() + Duration::from_secs(1);
        while current.len() < prefill {
            match blocks.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(b) => current.extend_from_slice(&b),
                Err(_) => break,
            }
        }
        let ctx = Arc::new(TxCtx {
            state: Mutex::new(Feed {
                blocks,
                current,
                pos: 0,
            }),
            drained: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        let raw = Arc::into_raw(ctx.clone()) as *mut c_void;
        let started = Instant::now();
        // SAFETY: raw stays valid until we reclaim it below, after hackrf_stop_tx returns,
        // at which point libhackrf no longer calls tx_callback.
        let start = unsafe { check("start_tx", ffi::hackrf_start_tx(d, tx_callback, raw)) };
        let mut ended_by = EndReason::Finished;
        if start.is_ok() {
            loop {
                if ctx.drained.load(Ordering::Acquire) {
                    // Let the in-flight USB transfers reach the radio before stopping:
                    // libhackrf keeps ~4 x 256 KiB queued, ~220 ms at 2.4 Msps.
                    let in_flight = 4.0 * 262_144.0 / 2.0 / f64::from(cfg.sample_rate);
                    std::thread::sleep(Duration::from_secs_f64(in_flight + 0.05));
                    break;
                }
                if stop() {
                    ended_by = EndReason::Stopped;
                    break;
                }
                if started.elapsed() >= timeout {
                    ended_by = EndReason::TimeOut;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // SAFETY: as above. Amp is switched off again so an idle device never sits amplified.
        let stopped = unsafe {
            let r = check("stop_tx", ffi::hackrf_stop_tx(d));
            ffi::hackrf_set_amp_enable(d, 0);
            drop(Arc::from_raw(raw as *const TxCtx));
            r
        };
        start?;
        stopped?;
        Ok(TxStats {
            duration: started.elapsed(),
            bytes: ctx.bytes.load(Ordering::Relaxed),
            underruns: ctx.underruns.load(Ordering::Relaxed),
            ended_by,
        })
    }
}

impl HackRf {
    /// Receive at `center_hz`, sending each USB block of cs8 IQ to `sink` until `stop()`
    /// returns true. Blocks are dropped (and counted) if the consumer falls behind.
    pub fn receive(
        &mut self,
        center_hz: u64,
        cfg: &RxConfig,
        sink: SyncSender<Vec<i8>>,
        stop: &dyn Fn() -> bool,
    ) -> Result<RxStats, Error> {
        let d = self.dev;
        // SAFETY: d is a live handle owned by self.
        unsafe {
            check("set_antenna_enable", ffi::hackrf_set_antenna_enable(d, 0))?;
            check(
                "set_sample_rate",
                ffi::hackrf_set_sample_rate(d, f64::from(cfg.sample_rate)),
            )?;
            let bw = ffi::hackrf_compute_baseband_filter_bw((cfg.sample_rate as f32 * 0.75) as u32);
            check(
                "set_baseband_filter_bandwidth",
                ffi::hackrf_set_baseband_filter_bandwidth(d, bw),
            )?;
            check("set_freq", ffi::hackrf_set_freq(d, center_hz))?;
            check(
                "set_amp_enable",
                ffi::hackrf_set_amp_enable(d, u8::from(cfg.amp)),
            )?;
            check(
                "set_lna_gain",
                ffi::hackrf_set_lna_gain(d, cfg.lna_db.min(40) / 8 * 8),
            )?;
            check(
                "set_vga_gain",
                ffi::hackrf_set_vga_gain(d, cfg.vga_db.min(62) / 2 * 2),
            )?;
        }
        let ctx = Arc::new(RxCtx {
            sink,
            dropped: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        let raw = Arc::into_raw(ctx.clone()) as *mut c_void;
        // SAFETY: raw outlives the stream; reclaimed only after hackrf_stop_rx returns.
        let start = unsafe { check("start_rx", ffi::hackrf_start_rx(d, rx_callback, raw)) };
        if start.is_ok() {
            while !stop() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // SAFETY: as above.
        let stopped = unsafe {
            let r = check("stop_rx", ffi::hackrf_stop_rx(d));
            ffi::hackrf_set_amp_enable(d, 0);
            drop(Arc::from_raw(raw as *const RxCtx));
            r
        };
        start?;
        stopped?;
        Ok(RxStats {
            bytes: ctx.bytes.load(Ordering::Relaxed),
            dropped: ctx.dropped.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RxConfig {
    pub sample_rate: u32,
    /// RF LNA, 0-40 dB in 8 dB steps.
    pub lna_db: u32,
    /// Baseband VGA, 0-62 dB in 2 dB steps.
    pub vga_db: u32,
    /// Front-end RF amp (~11 dB). Leave off with an external preamp or strong signals.
    pub amp: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RxStats {
    pub bytes: u64,
    /// USB blocks discarded because the demodulator fell behind.
    pub dropped: u64,
}

struct RxCtx {
    sink: SyncSender<Vec<i8>>,
    dropped: AtomicU64,
    bytes: AtomicU64,
}

unsafe extern "C" fn rx_callback(t: *mut ffi::hackrf_transfer) -> c_int {
    // SAFETY: libhackrf passes a valid transfer whose rx_ctx is our Arc<RxCtx> pointer and
    // whose buffer holds valid_length readable bytes.
    let (ctx, buf) = unsafe {
        let t = &*t;
        (
            &*(t.rx_ctx as *const RxCtx),
            std::slice::from_raw_parts(t.buffer as *const i8, t.valid_length as usize),
        )
    };
    ctx.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
    match ctx.sink.try_send(buf.to_vec()) {
        Ok(()) => 0,
        Err(TrySendError::Full(_)) => {
            ctx.dropped.fetch_add(1, Ordering::Relaxed);
            0
        }
        // Consumer gone: ask libhackrf to stop streaming.
        Err(TrySendError::Disconnected(_)) => -1,
    }
}

impl Drop for HackRf {
    fn drop(&mut self) {
        // SAFETY: dev is a live handle; close also stops any stream.
        unsafe {
            ffi::hackrf_close(self.dev);
            ffi::hackrf_exit();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TxConfig {
    pub sample_rate: u32,
    /// IF gain 0-47 dB. Not the RF amp: that is controlled by the permit.
    pub txvga_db: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Finished,
    Stopped,
    TimeOut,
}

#[derive(Debug, Clone, Copy)]
pub struct TxStats {
    pub duration: Duration,
    pub bytes: u64,
    /// USB transfers padded with silence because the modulator fell behind.
    pub underruns: u64,
    pub ended_by: EndReason,
}

struct Feed {
    blocks: Receiver<Vec<i8>>,
    current: Vec<i8>,
    pos: usize,
}

struct TxCtx {
    state: Mutex<Feed>,
    drained: AtomicBool,
    underruns: AtomicU64,
    bytes: AtomicU64,
}

unsafe extern "C" fn tx_callback(t: *mut ffi::hackrf_transfer) -> c_int {
    // SAFETY: libhackrf passes a valid transfer whose tx_ctx is the Arc<TxCtx> pointer we
    // gave hackrf_start_tx, and whose buffer holds buffer_length writable bytes.
    let (ctx, buf) = unsafe {
        let t = &mut *t;
        t.valid_length = t.buffer_length;
        (
            &*(t.tx_ctx as *const TxCtx),
            std::slice::from_raw_parts_mut(t.buffer as *mut i8, t.buffer_length as usize),
        )
    };
    let Ok(mut feed) = ctx.state.lock() else {
        buf.fill(0);
        return 0;
    };
    let mut filled = 0;
    while filled < buf.len() {
        if feed.pos == feed.current.len() {
            match feed.blocks.try_recv() {
                Ok(b) => {
                    feed.current = b;
                    feed.pos = 0;
                    continue;
                }
                Err(TryRecvError::Empty) => {
                    ctx.underruns.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    ctx.drained.store(true, Ordering::Release);
                    break;
                }
            }
        }
        let n = (buf.len() - filled).min(feed.current.len() - feed.pos);
        buf[filled..filled + n].copy_from_slice(&feed.current[feed.pos..feed.pos + n]);
        feed.pos += n;
        filled += n;
    }
    // Pad with zeros: no RF output rather than garbage.
    buf[filled..].fill(0);
    ctx.bytes.fetch_add(filled as u64, Ordering::Relaxed);
    0
}
