use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use sdrtx_core::bandplan::US_GENERAL;
use sdrtx_core::freq::{fmt_mhz, parse_freq, parse_offset};
use sdrtx_core::policy::{
    Authority, Emission, OFFBAND_TEST_ENABLED, TxPermit, TxRequest, authorize,
};
use sdrtx_dsp::AUDIO_RATE;
use sdrtx_dsp::dcs::parse_dcs;
use sdrtx_dsp::fm::{FmConfig, FmModulator, SubTone};
use sdrtx_dsp::sources::{AudioSource, Silence, Take, Tone, WavFile};
use sdrtx_hackrf::{EndReason, HackRf, TXVGA_MAX_DB, TxConfig};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// IQ sample rate. An integer multiple of the 48 kHz audio rate.
const IQ_RATE: u32 = 2_400_000;
/// Audio samples per block handed to the radio (100 ms).
const BLOCK: usize = AUDIO_RATE as usize / 10;

#[derive(Parser)]
#[command(
    name = "sdrtx",
    version,
    about = "HackRF transmitter with amateur band-plan guardrails"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the amateur allocations sdrtx treats as licensed.
    Bands,
    /// Show whether a transmission would be authorized, without keying up.
    Check(TxArgs),
    /// Transmit.
    Tx(TxArgs),
    /// List audio devices as the GUI names them.
    #[command(hide = true)]
    AudioDevices,
    /// Drive the same transceiver engine the GUI uses: receive, hold PTT for --ptt-secs,
    /// then receive again. Uses the GUI's saved settings (~/.config/sdrtx/settings.json).
    #[command(hide = true)]
    EngineTest {
        #[arg(long, default_value_t = 3.0)]
        ptt_secs: f64,
        #[arg(long)]
        dummy_load: bool,
    },
    /// Demodulate an IQ file (as written by `tx --iq-out`, channel at 0 Hz) to a WAV file.
    /// For live receive with PTT, use sdrtx-gui.
    Rx {
        #[arg(long)]
        iq_in: PathBuf,
        #[arg(long)]
        wav_out: PathBuf,
        /// Squelch 0 (open) - 0.95 (tight).
        #[arg(long, default_value_t = 0.0)]
        squelch: f32,
    },
}

#[derive(Args)]
struct TxArgs {
    /// Frequency: the repeater OUTPUT if --offset is given, otherwise the TX frequency.
    /// e.g. 146.94M, 446.0, 1282500k
    #[arg(short, long, value_parser = parse_freq)]
    freq: u64,
    /// Repeater offset added to --freq to get the TX frequency, e.g. -600k, +5M.
    #[arg(long, value_parser = parse_offset, allow_hyphen_values = true)]
    offset: Option<i64>,
    /// CTCSS (PL) tone in Hz, e.g. 146.2
    #[arg(long, conflicts_with = "dcs")]
    ctcss: Option<f32>,
    /// DCS code, three octal digits with optional N(ormal)/I(nverted) suffix, e.g. 244, D244N
    #[arg(long, value_parser = parse_dcs)]
    dcs: Option<(u16, bool)>,
    /// What to send: tone:<Hz>, wav:<path>, silence (FM carrier, plus CTCSS/DCS if set),
    /// or carrier (unmodulated test carrier).
    #[arg(short, long, default_value = "tone:1000")]
    source: String,
    /// Seconds to send for tone/silence/carrier sources.
    #[arg(short, long, default_value_t = 5.0)]
    duration: f64,
    /// Peak FM deviation in Hz (voice + sub-tone). Default 5000, or 2500 with --narrow.
    #[arg(long)]
    deviation: Option<f32>,
    /// Narrowband FM: 12.5 kHz channel, +/-2.5 kHz, sub-tone at 500 Hz.
    #[arg(long)]
    narrow: bool,
    /// Audio gain into the limiter (linear).
    #[arg(long, default_value_t = 1.0)]
    audio_gain: f32,
    /// HackRF TX IF gain, 0-47 dB.
    #[arg(long, default_value_t = 0)]
    txvga: u32,
    /// Enable the HackRF internal RF amplifier (~+11-14 dB).
    #[arg(long)]
    amp: bool,
    /// I confirm the HackRF output goes into a DUMMY LOAD or shielded fixture with NO
    /// amplification. Required for any frequency outside amateur privileges.
    #[arg(long, hide = !OFFBAND_TEST_ENABLED)]
    dummy_load: bool,
    /// Write cs8 IQ to this file instead of transmitting.
    #[arg(long)]
    iq_out: Option<PathBuf>,
    /// Time-out timer: unkey after this many seconds no matter what.
    #[arg(long, default_value_t = 180)]
    tot: u64,
}

enum Source {
    Tone(f32),
    Wav(PathBuf),
    Silence,
    Carrier,
}

impl TxArgs {
    fn tx_freq(&self) -> Result<u64> {
        let f = self.freq as i64 + self.offset.unwrap_or(0);
        if f <= 0 {
            bail!("offset puts TX frequency below zero");
        }
        Ok(f as u64)
    }

    fn parse_source(&self) -> Result<Source> {
        let s = self.source.as_str();
        Ok(match s.split_once(':') {
            Some(("tone", hz)) => Source::Tone(hz.parse().context("tone frequency")?),
            Some(("wav", p)) => Source::Wav(p.into()),
            None if s == "silence" => Source::Silence,
            None if s == "carrier" => {
                if self.ctcss.is_some() || self.dcs.is_some() {
                    bail!("an unmodulated carrier cannot carry CTCSS/DCS; use --source silence");
                }
                Source::Carrier
            }
            _ => bail!("unknown source {s:?}: expected tone:<Hz>, wav:<path>, silence or carrier"),
        })
    }

    fn fm_config(&self, source: &Source) -> FmConfig {
        let subtone = match (self.ctcss, self.dcs) {
            (Some(hz), _) => Some(SubTone::Ctcss(hz)),
            (_, Some((code, inverted))) => Some(SubTone::Dcs { code, inverted }),
            _ => None,
        };
        let mut cfg = if self.narrow {
            FmConfig::nbfm_narrow(IQ_RATE, subtone)
        } else {
            FmConfig::nbfm(IQ_RATE, subtone)
        };
        if let Some(d) = self.deviation {
            cfg.peak_deviation_hz = d;
        }
        cfg.audio_gain = self.audio_gain;
        if matches!(source, Source::Carrier) {
            cfg.peak_deviation_hz = 0.0;
        }
        cfg
    }

    fn request(&self, source: &Source, fm: &FmConfig) -> Result<TxRequest> {
        let emission = match source {
            Source::Carrier => Emission::Carrier,
            _ => Emission::FmVoice {
                occupied_bw_hz: fm.occupied_bw_hz(),
            },
        };
        Ok(TxRequest {
            freq_hz: self.tx_freq()?,
            emission,
            internal_amp: self.amp,
            dummy_load_ack: self.dummy_load,
        })
    }
}

fn print_permit(p: &TxPermit) {
    eprintln!("PERMIT  {p}");
    for w in p.warnings() {
        eprintln!("  ! {w}");
    }
    if let Authority::Amateur(_) = p.authority() {
        eprintln!("  ! identify with your callsign every 10 minutes and at the end (97.119)");
    }
}

fn open_source(src: &Source, secs: f64) -> Result<Box<dyn AudioSource>> {
    Ok(match src {
        Source::Tone(hz) => Box::new(Take::new(Tone::new(*hz, 0.7), secs)),
        Source::Wav(p) => {
            let w = WavFile::open(p).with_context(|| format!("opening {}", p.display()))?;
            eprintln!("wav: {:.1} s", w.duration_secs());
            Box::new(w)
        }
        Source::Silence | Source::Carrier => Box::new(Take::new(Silence, secs)),
    })
}

/// Pull audio through the modulator, handing each IQ block to `sink` until the source ends
/// or `sink` returns false.
fn run_modulator(
    mut src: Box<dyn AudioSource>,
    mut fm: FmModulator,
    mut sink: impl FnMut(Vec<i8>) -> bool,
) {
    let mut audio = vec![0.0f32; BLOCK];
    loop {
        let n = src.read(&mut audio);
        if n == 0 {
            return;
        }
        let mut iq = Vec::new();
        fm.process(&audio[..n], &mut iq);
        if !sink(iq) {
            return;
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Bands => {
            for b in US_GENERAL {
                println!(
                    "{:<10} {:>16} - {:<16} {}",
                    b.name,
                    fmt_mhz(b.lo_hz),
                    fmt_mhz(b.hi_hz),
                    b.note
                );
            }
            if OFFBAND_TEST_ENABLED {
                println!("\nAnything else: TX only with --dummy-load (no amplification).");
            } else {
                println!("\nAnything else: TX refused.");
            }
            Ok(())
        }
        Cmd::Check(a) => {
            let src = a.parse_source()?;
            let fm = a.fm_config(&src);
            let permit = authorize(&a.request(&src, &fm)?)?;
            print_permit(&permit);
            Ok(())
        }
        Cmd::Tx(a) => tx(a),
        Cmd::Rx {
            iq_in,
            wav_out,
            squelch,
        } => rx_file(&iq_in, &wav_out, squelch),
        Cmd::AudioDevices => {
            println!("inputs:");
            for d in sdrtx_radio::audio::input_devices() {
                println!("  {:<40} {}", d.id, d.label);
            }
            println!("outputs:");
            for d in sdrtx_radio::audio::output_devices() {
                println!("  {:<40} {}", d.id, d.label);
            }
            Ok(())
        }
        Cmd::EngineTest {
            ptt_secs,
            dummy_load,
        } => engine_test(ptt_secs, dummy_load),
    }
}

fn rx_file(iq_in: &std::path::Path, wav_out: &std::path::Path, squelch: f32) -> Result<()> {
    use sdrtx_dsp::rx::{FmReceiver, RxSettings};
    let raw = std::fs::read(iq_in).with_context(|| format!("reading {}", iq_in.display()))?;
    let iq: Vec<i8> = raw.into_iter().map(|b| b as i8).collect();
    let mut rx = FmReceiver::new(
        IQ_RATE,
        0,
        RxSettings {
            volume: 0.25,
            squelch,
        },
    );
    let started = std::time::Instant::now();
    let mut audio = Vec::new();
    rx.process(&iq, &mut audio);
    let secs = iq.len() as f64 / 2.0 / f64::from(IQ_RATE);
    let took = started.elapsed().as_secs_f64();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(wav_out, spec)?;
    for s in &audio {
        w.write_sample((s * 32767.0) as i16)?;
    }
    w.finalize()?;
    eprintln!(
        "{:.1} s of IQ demodulated in {took:.2} s ({:.1}x real time); squelch {}, {:.1} dBFS -> {}",
        secs,
        secs / took,
        if rx.squelch_open() { "open" } else { "closed" },
        rx.power_dbfs(),
        wav_out.display()
    );
    Ok(())
}

fn tx(a: TxArgs) -> Result<()> {
    if a.txvga > TXVGA_MAX_DB {
        bail!("--txvga must be 0-{TXVGA_MAX_DB}");
    }
    let src = a.parse_source()?;
    let fm_cfg = a.fm_config(&src);
    let permit = authorize(&a.request(&src, &fm_cfg)?)?;
    print_permit(&permit);
    let audio = open_source(&src, a.duration)?;
    let fm = FmModulator::new(fm_cfg);

    if let Some(path) = &a.iq_out {
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
        );
        let mut err = None;
        run_modulator(audio, fm, |iq| {
            let bytes: Vec<u8> = iq.iter().map(|&s| s as u8).collect();
            match f.write_all(&bytes) {
                Ok(()) => true,
                Err(e) => {
                    err = Some(e);
                    false
                }
            }
        });
        if let Some(e) = err {
            return Err(e.into());
        }
        f.flush()?;
        eprintln!(
            "wrote {} ({} Msps cs8, centre {})",
            path.display(),
            f64::from(IQ_RATE) / 1e6,
            fmt_mhz(permit.freq_hz())
        );
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::Release))?;
    }
    let mut radio = HackRf::open().context("opening HackRF (is it plugged in?)")?;
    // ~2 s of IQ buffered ahead of the radio.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<i8>>(20);
    let producer = std::thread::spawn(move || run_modulator(audio, fm, |iq| tx.send(iq).is_ok()));

    eprintln!(
        "TX ON   {}  (Ctrl-C to unkey, TOT {} s)",
        fmt_mhz(permit.freq_hz()),
        a.tot
    );
    let stats = radio.transmit(
        &permit,
        &TxConfig {
            sample_rate: IQ_RATE,
            txvga_db: a.txvga,
        },
        rx,
        &|| stop.load(Ordering::Acquire),
        Duration::from_secs(a.tot),
    )?;
    eprintln!(
        "TX OFF  {:.1} s, {} underruns, {}",
        stats.duration.as_secs_f64(),
        stats.underruns,
        match stats.ended_by {
            EndReason::Finished => "source finished",
            EndReason::Stopped => "stopped by operator",
            EndReason::TimeOut => "TIME-OUT TIMER",
        }
    );
    // The receiver is gone now, so the producer's next send fails and it exits.
    let _ = producer.join();
    Ok(())
}

fn engine_test(ptt_secs: f64, dummy_load: bool) -> Result<()> {
    use sdrtx_radio::{Mode, Radio, Settings};
    #[derive(serde::Deserialize)]
    struct Saved {
        radio: Settings,
    }
    let path = std::env::var("HOME")? + "/.config/sdrtx/settings.json";
    let mut s = serde_json::from_str::<Saved>(&std::fs::read_to_string(&path)?)?.radio;
    s.dummy_load = dummy_load;
    eprintln!(
        "settings from {path}: {}",
        s.authorize()
            .map_or_else(|e| format!("REFUSED: {e}"), |p| p.to_string())
    );
    let radio = Radio::start(s);
    let t0 = std::time::Instant::now();
    let log = |what: &str| {
        let st = radio.status();
        eprintln!(
            "{:5.2}s {what:<9} mode={:?} rssi={:.0} noise={:.2} msg={:?} underruns={} dropped={}",
            t0.elapsed().as_secs_f64(),
            st.mode,
            st.rssi_dbfs,
            st.noise,
            st.message,
            st.tx_underruns,
            st.rx_dropped
        );
        st.mode
    };
    let wait = |secs: f64, what: &str| {
        let end = std::time::Instant::now() + Duration::from_secs_f64(secs);
        while std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(500));
            log(what);
        }
    };
    wait(3.0, "rx");
    radio.set_ptt(true);
    eprintln!("{:5.2}s PTT ON", t0.elapsed().as_secs_f64());
    wait(ptt_secs, "ptt held");
    radio.set_ptt(false);
    eprintln!("{:5.2}s PTT off", t0.elapsed().as_secs_f64());
    wait(2.0, "released");
    if log("final") == Mode::Tx {
        bail!("still transmitting 2 s after release");
    }
    Ok(())
}
