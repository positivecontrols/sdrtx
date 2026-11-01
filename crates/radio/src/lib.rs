//! Half-duplex transceiver engine.
//!
//! One thread owns the HackRF and alternates between receiving on the repeater output and,
//! while push-to-talk is held, transmitting on the input. Every key-up re-runs
//! [`sdrtx_core::policy::authorize`]; a refused request never reaches the radio.

pub mod audio;

use audio::AudioQueue;
use sdrtx_core::policy::{Emission, TxPermit, TxRequest, authorize};
use sdrtx_dsp::AUDIO_RATE;
use sdrtx_dsp::fm::{FmConfig, FmModulator, SubTone};
use sdrtx_dsp::rx::{FmReceiver, RX_LO_OFFSET_HZ, RxSettings};
use sdrtx_hackrf::{EndReason, HackRf, RxConfig, TxConfig};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// IQ sample rate for both directions; an integer multiple of the 48 kHz audio rate.
pub const IQ_RATE: u32 = 2_400_000;
/// Audio per TX block: 20 ms.
const TX_BLOCK: usize = AUDIO_RATE as usize / 50;
/// Carrier + sub-tone sent before mic audio, so the repeater's decoder can lock first.
const PREROLL_BLOCKS: usize = 3;
/// Force the transmitter off this long after PTT release even if the flush hangs.
const UNKEY_BACKSTOP: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Tone {
    None,
    Ctcss(f32),
    Dcs { code: u16, inverted: bool },
}

impl Tone {
    fn subtone(self) -> Option<SubTone> {
        match self {
            Tone::None => None,
            Tone::Ctcss(hz) => Some(SubTone::Ctcss(hz)),
            Tone::Dcs { code, inverted } => Some(SubTone::Dcs { code, inverted }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Receive frequency: the repeater output.
    pub rx_freq_hz: u64,
    /// Added to `rx_freq_hz` to get the transmit frequency (repeater input).
    pub offset_hz: i64,
    pub tone: Tone,
    /// Narrowband FM (12.5 kHz channel, +/-2.5 kHz) instead of +/-5 kHz.
    pub narrow: bool,
    /// Operator asserts: HackRF output into a dummy load, no amplification. Needed outside
    /// amateur privileges in `offband-test` builds, ignored otherwise; not persisted.
    #[serde(skip)]
    pub dummy_load: bool,
    pub tx_amp: bool,
    pub txvga_db: u32,
    pub rx_lna_db: u32,
    pub rx_vga_db: u32,
    pub rx_amp: bool,
    pub volume: f32,
    pub squelch: f32,
    pub mic_gain: f32,
    pub tot_s: u64,
    pub mic_device: Option<String>,
    pub speaker_device: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let kk1rz = &presets()[0];
        Self {
            rx_freq_hz: kk1rz.rx_freq_hz,
            offset_hz: kk1rz.offset_hz,
            tone: kk1rz.tone,
            narrow: kk1rz.narrow,
            dummy_load: false,
            tx_amp: false,
            txvga_db: 0,
            rx_lna_db: 16,
            rx_vga_db: 24,
            rx_amp: false,
            volume: 0.5,
            squelch: 0.3,
            mic_gain: 1.0,
            tot_s: 180,
            mic_device: None,
            speaker_device: None,
        }
    }
}

impl Settings {
    pub fn tx_freq_hz(&self) -> Option<u64> {
        u64::try_from(self.rx_freq_hz as i64 + self.offset_hz).ok()
    }

    pub fn fm_config(&self) -> FmConfig {
        if self.narrow {
            FmConfig::nbfm_narrow(IQ_RATE, self.tone.subtone())
        } else {
            FmConfig::nbfm(IQ_RATE, self.tone.subtone())
        }
    }

    /// What transmitting with these settings would be allowed to do, or why not.
    pub fn authorize(&self) -> Result<TxPermit, String> {
        let freq_hz = self
            .tx_freq_hz()
            .ok_or("offset puts TX frequency below zero")?;
        authorize(&TxRequest {
            freq_hz,
            emission: Emission::FmVoice {
                occupied_bw_hz: self.fm_config().occupied_bw_hz(),
            },
            internal_amp: self.tx_amp,
            dummy_load_ack: self.dummy_load,
        })
        .map_err(|r| r.to_string())
    }

    fn rf_key(&self) -> (u64, u32, u32, bool) {
        (self.rx_freq_hz, self.rx_lna_db, self.rx_vga_db, self.rx_amp)
    }
}

pub struct Preset {
    pub name: &'static str,
    pub rx_freq_hz: u64,
    pub offset_hz: i64,
    pub tone: Tone,
    pub narrow: bool,
}

/// MMRA machines near Boston/Brockton (mmra.org repeater index, 2026-10-10) + simplex.
pub fn presets() -> Vec<Preset> {
    let p = |name, rx_freq_hz, offset_hz, tone| Preset {
        name,
        rx_freq_hz,
        offset_hz,
        tone,
        narrow: false,
    };
    vec![
        // Narrowband per the household wiki's 33cm notes (hackrf-repeater-chain).
        Preset {
            narrow: true,
            ..p(
                "KK1RZ Boston Waterfront 33cm",
                927_062_500,
                -25_000_000,
                Tone::Dcs {
                    code: 0o244,
                    inverted: false,
                },
            )
        },
        p(
            "K1BOS Back Bay 2m",
            146_820_000,
            -600_000,
            Tone::Ctcss(146.2),
        ),
        p("K1IW Milton 2m", 146_670_000, -600_000, Tone::Ctcss(146.2)),
        p(
            "N1KUG Milton 1.25m",
            224_400_000,
            -1_600_000,
            Tone::Ctcss(103.5),
        ),
        p(
            "W1MRA Weston 1.25m",
            224_700_000,
            -1_600_000,
            Tone::Ctcss(103.5),
        ),
        p(
            "N1DCH Weston 70cm",
            442_700_000,
            5_000_000,
            Tone::Ctcss(88.5),
        ),
        p("2m calling simplex", 146_520_000, 0, Tone::None),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Starting,
    /// No HackRF found; retrying.
    NoRadio,
    Rx,
    Tx,
    /// PTT held but the request was refused; nothing is transmitted.
    Refused,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub mode: Mode,
    pub rssi_dbfs: f32,
    /// Squelch noise measure, ~1.0 = no signal.
    pub noise: f32,
    pub squelch_open: bool,
    pub mic_peak: f32,
    pub speaker_peak: f32,
    pub tx_started: Option<Instant>,
    pub tot_latched: bool,
    pub message: Option<String>,
    pub audio_error: Option<String>,
    pub rx_dropped: u64,
    pub tx_underruns: u64,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            mode: Mode::Starting,
            rssi_dbfs: -120.0,
            noise: 1.0,
            squelch_open: false,
            mic_peak: 0.0,
            speaker_peak: 0.0,
            tx_started: None,
            tot_latched: false,
            message: None,
            audio_error: None,
            rx_dropped: 0,
            tx_underruns: 0,
        }
    }
}

struct Shared {
    settings: Mutex<Settings>,
    status: Mutex<Status>,
    ptt: AtomicBool,
    released_at: Mutex<Instant>,
    tot_latched: AtomicBool,
    shutdown: AtomicBool,
    mic: Arc<AudioQueue>,
    speaker: Arc<AudioQueue>,
}

impl Shared {
    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }
    fn status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }
    fn down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
    fn wants_tx(&self) -> bool {
        self.ptt.load(Ordering::Acquire) && !self.tot_latched.load(Ordering::Acquire)
    }
}

pub struct Radio {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Radio {
    pub fn start(settings: Settings) -> Self {
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings),
            status: Mutex::new(Status::default()),
            ptt: AtomicBool::new(false),
            released_at: Mutex::new(Instant::now()),
            tot_latched: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            mic: AudioQueue::new(0.5),
            speaker: AudioQueue::new(0.3),
        });
        let sh = shared.clone();
        let thread = thread::Builder::new()
            .name("sdrtx-engine".into())
            .spawn(move || engine(sh))
            .expect("spawn engine thread");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    pub fn settings(&self) -> Settings {
        self.shared.settings()
    }

    pub fn set_settings(&self, s: Settings) {
        *self.shared.settings.lock().unwrap() = s;
    }

    pub fn set_ptt(&self, on: bool) {
        if !on && self.shared.ptt.load(Ordering::Acquire) {
            *self.shared.released_at.lock().unwrap() = Instant::now();
            self.shared.tot_latched.store(false, Ordering::Release);
        }
        self.shared.ptt.store(on, Ordering::Release);
    }

    pub fn status(&self) -> Status {
        let mut s = self.shared.status.lock().unwrap().clone();
        s.mic_peak = self.shared.mic.peak();
        s.speaker_peak = self.shared.speaker.peak();
        s.tot_latched = self.shared.tot_latched.load(Ordering::Acquire);
        s
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.shared.ptt.store(false, Ordering::Release);
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn engine(sh: Arc<Shared>) {
    let mut devices: Option<(Option<String>, Option<String>)> = None;
    let mut mic_stream = None;
    let mut _speaker_stream = None;
    let mut radio: Option<HackRf> = None;
    let mut last_open: Option<Instant> = None;

    while !sh.down() {
        let s = sh.settings();
        let wanted = (s.mic_device.clone(), s.speaker_device.clone());
        if devices.as_ref() != Some(&wanted) {
            mic_stream = None;
            _speaker_stream = None;
            let mut errs = Vec::new();
            match audio::open_mic(wanted.0.as_deref(), sh.mic.clone()) {
                Ok(st) => mic_stream = Some(st),
                Err(e) => errs.push(format!("mic: {e}")),
            }
            match audio::open_speaker(wanted.1.as_deref(), sh.speaker.clone()) {
                Ok(st) => _speaker_stream = Some(st),
                Err(e) => errs.push(format!("speaker: {e}")),
            }
            sh.status(|st| st.audio_error = (!errs.is_empty()).then(|| errs.join("; ")));
            devices = Some(wanted);
        }

        let Some(r) = radio.as_mut() else {
            if last_open.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                last_open = Some(Instant::now());
                match HackRf::open() {
                    Ok(r) => {
                        radio = Some(r);
                        sh.status(|st| st.message = None);
                        continue;
                    }
                    Err(e) => sh.status(|st| {
                        st.mode = Mode::NoRadio;
                        st.message = Some(format!("{e} - is the HackRF plugged in?"));
                    }),
                }
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        };

        let res = if sh.wants_tx() {
            run_tx(&sh, r, mic_stream.is_some())
        } else {
            run_rx(&sh, r)
        };
        if let Err(e) = res {
            // Most likely unplugged; drop the handle and let the loop reopen it.
            radio = None;
            sh.status(|st| {
                st.mode = Mode::NoRadio;
                st.message = Some(e);
            });
        }
    }
}

fn run_rx(sh: &Arc<Shared>, r: &mut HackRf) -> Result<(), String> {
    let s = sh.settings();
    let key = s.rf_key();
    let Some(center) = s.rx_freq_hz.checked_sub(RX_LO_OFFSET_HZ as u64) else {
        thread::sleep(Duration::from_millis(100));
        return Ok(());
    };
    sh.status(|st| {
        st.mode = Mode::Rx;
        st.tx_started = None;
    });

    let (tx, blocks) = sync_channel::<Vec<i8>>(16);
    let sh2 = sh.clone();
    let rx_settings = RxSettings {
        volume: s.volume,
        squelch: s.squelch,
    };
    let demod = thread::spawn(move || {
        let mut fm = FmReceiver::new(IQ_RATE, RX_LO_OFFSET_HZ, rx_settings);
        let mut audio = Vec::new();
        for block in blocks {
            {
                let s = sh2.settings.lock().unwrap();
                fm.settings = RxSettings {
                    volume: s.volume,
                    squelch: s.squelch,
                };
            }
            audio.clear();
            fm.process(&block, &mut audio);
            sh2.speaker.push(audio.iter().copied());
            sh2.status(|st| {
                st.rssi_dbfs = fm.power_dbfs();
                st.noise = fm.noise();
                st.squelch_open = fm.squelch_open();
            });
        }
    });

    let cfg = RxConfig {
        sample_rate: IQ_RATE,
        lna_db: s.rx_lna_db,
        vga_db: s.rx_vga_db,
        amp: s.rx_amp,
    };
    let stop = || sh.down() || sh.wants_tx() || sh.settings.lock().unwrap().rf_key() != key;
    let res = r.receive(center, &cfg, tx, &stop);
    let _ = demod.join();
    sh.speaker.clear();
    sh.status(|st| st.squelch_open = false);
    let stats = res.map_err(|e| e.to_string())?;
    sh.status(|st| st.rx_dropped += stats.dropped);
    Ok(())
}

fn run_tx(sh: &Arc<Shared>, r: &mut HackRf, mic_alive: bool) -> Result<(), String> {
    let s = sh.settings();
    let permit = match s.authorize() {
        Ok(p) => p,
        Err(why) => {
            sh.status(|st| {
                st.mode = Mode::Refused;
                st.message = Some(why);
            });
            while sh.ptt.load(Ordering::Acquire) && !sh.down() {
                thread::sleep(Duration::from_millis(20));
            }
            return Ok(());
        }
    };
    sh.status(|st| {
        st.mode = Mode::Tx;
        st.tx_started = Some(Instant::now());
        st.message = None;
    });
    sh.mic.clear();

    let (tx, blocks) = sync_channel::<Vec<i8>>(4);
    let sh2 = sh.clone();
    let fm_cfg = s.fm_config();
    let producer = thread::spawn(move || {
        let mut fm = FmModulator::new(fm_cfg);
        let mut buf = vec![0.0f32; TX_BLOCK];
        let mut iq = Vec::new();
        let mut send = |fm: &mut FmModulator, audio: &[f32]| {
            iq.clear();
            fm.process(audio, &mut iq);
            tx.send(iq.clone()).is_ok()
        };
        for _ in 0..PREROLL_BLOCKS {
            if !send(&mut fm, &buf) {
                return;
            }
        }
        // After release: only the audio captured before release is still owed.
        let mut owed: Option<usize> = None;
        loop {
            if owed.is_none() && !sh2.ptt.load(Ordering::Acquire) {
                owed = Some(sh2.mic.len());
            }
            if mic_alive && owed.is_none() {
                let deadline = Instant::now() + Duration::from_millis(25);
                while sh2.mic.len() < TX_BLOCK && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(2));
                }
                // Clock drift between sound card and HackRF: never let latency creep up.
                let mut scratch = [0.0f32; 480];
                while sh2.mic.len() > AUDIO_RATE as usize / 5 {
                    sh2.mic.pop_into(&mut scratch);
                }
            }
            let want = owed.map_or(TX_BLOCK, |o| o.min(TX_BLOCK));
            if want == 0 {
                return;
            }
            let n = if mic_alive {
                sh2.mic.pop_into(&mut buf[..want])
            } else {
                0
            };
            if let Some(o) = owed.as_mut() {
                *o -= want;
                if n == 0 {
                    return;
                }
            }
            buf[n..].fill(0.0);
            let gain = sh2.settings.lock().unwrap().mic_gain;
            buf.iter_mut().for_each(|v| *v *= gain);
            if !send(&mut fm, &buf) {
                return;
            }
        }
    });

    let released_long_ago = || {
        !sh.ptt.load(Ordering::Acquire) && sh.released_at.lock().unwrap().elapsed() > UNKEY_BACKSTOP
    };
    let stop = || sh.down() || released_long_ago();
    let res = r.transmit(
        &permit,
        &TxConfig {
            sample_rate: IQ_RATE,
            txvga_db: s.txvga_db,
        },
        blocks,
        &stop,
        Duration::from_secs(s.tot_s),
    );
    let _ = producer.join();
    let stats = res.map_err(|e| e.to_string())?;
    if stats.ended_by == EndReason::TimeOut {
        sh.tot_latched.store(true, Ordering::Release);
        sh.status(|st| st.message = Some("time-out timer expired: release PTT".into()));
    }
    sh.status(|st| {
        st.mode = Mode::Rx;
        st.tx_started = None;
        st.tx_underruns += stats.underruns;
    });
    Ok(())
}
