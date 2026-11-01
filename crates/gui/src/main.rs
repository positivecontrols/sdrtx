//! sdrtx-gui: push-to-talk HackRF transceiver.
//!
//! Hold the PTT button (or Space, when no text field has focus) to transmit on the repeater
//! input; release to receive on the output. PTT is disabled whenever the current settings
//! would be refused, and the engine re-checks authorization on every key-up regardless.

use eframe::egui::{self, Color32, Key, RichText, vec2};
use sdrtx_core::freq::{fmt_mhz, parse_freq, parse_offset};
use sdrtx_dsp::dcs::parse_dcs;
use sdrtx_radio::{Mode, Radio, Settings, Tone, audio, presets};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const RED: Color32 = Color32::from_rgb(0xd6, 0x2d, 0x2d);
const GREEN: Color32 = Color32::from_rgb(0x2e, 0xa0, 0x43);
const AMBER: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x00);

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sdrtx").join("settings.json"))
}

/// Everything persisted between runs: the radio's settings plus GUI-only preferences.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Saved {
    radio: Settings,
    ui_scale: f32,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            radio: Settings::default(),
            ui_scale: 1.5,
        }
    }
}

fn load_saved() -> Saved {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(s: &Saved) {
    let Some(p) = config_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(p, json);
    }
}

fn mhz_text(hz: u64) -> String {
    let s = format!("{:.6}", hz as f64 / 1e6);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn offset_text(hz: i64) -> String {
    match hz {
        0 => "0".into(),
        h if h % 1_000_000 == 0 => format!("{:+}M", h / 1_000_000),
        h if h % 1_000 == 0 => format!("{:+}k", h / 1_000),
        h => format!("{h:+}"),
    }
}

fn tone_text(t: Tone) -> String {
    match t {
        Tone::None => String::new(),
        Tone::Ctcss(hz) => format!("{hz:.1}"),
        Tone::Dcs { code, inverted } => format!("{code:03o}{}", if inverted { "I" } else { "N" }),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ToneKind {
    None,
    Ctcss,
    Dcs,
}

struct App {
    radio: Radio,
    s: Settings,
    rx_text: String,
    off_text: String,
    tone_kind: ToneKind,
    tone_text: String,
    edit_err: Option<String>,
    mic_names: Vec<audio::AudioDevice>,
    spk_names: Vec<audio::AudioDevice>,
    ui_scale: f32,
    saved: Saved,
    last_save: Instant,
    last_ptt: bool,
}

impl App {
    fn new() -> Self {
        let saved = load_saved();
        let s = saved.radio.clone();
        let mut app = Self {
            radio: Radio::start(s.clone()),
            ui_scale: saved.ui_scale,
            saved,
            s,
            rx_text: String::new(),
            off_text: String::new(),
            tone_kind: ToneKind::None,
            tone_text: String::new(),
            edit_err: None,
            mic_names: audio::input_devices(),
            spk_names: audio::output_devices(),
            last_save: Instant::now(),
            last_ptt: false,
        };
        app.load_texts();
        app
    }

    fn load_texts(&mut self) {
        self.rx_text = mhz_text(self.s.rx_freq_hz);
        self.off_text = offset_text(self.s.offset_hz);
        self.tone_kind = match self.s.tone {
            Tone::None => ToneKind::None,
            Tone::Ctcss(_) => ToneKind::Ctcss,
            Tone::Dcs { .. } => ToneKind::Dcs,
        };
        self.tone_text = tone_text(self.s.tone);
        self.edit_err = None;
    }

    /// Parse the channel text fields into settings; leaves settings alone on any error.
    fn apply_texts(&mut self) {
        let parsed = (|| -> Result<(u64, i64, Tone), String> {
            let rx = parse_freq(&self.rx_text)?;
            let off = parse_offset(self.off_text.trim())?;
            let tone = match self.tone_kind {
                ToneKind::None => Tone::None,
                ToneKind::Ctcss => Tone::Ctcss(
                    self.tone_text
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad CTCSS tone {:?}", self.tone_text))?,
                ),
                ToneKind::Dcs => {
                    let (code, inverted) = parse_dcs(&self.tone_text)?;
                    Tone::Dcs { code, inverted }
                }
            };
            Ok((rx, off, tone))
        })();
        match parsed {
            Ok((rx, off, tone)) => {
                self.s.rx_freq_hz = rx;
                self.s.offset_hz = off;
                self.s.tone = tone;
                self.edit_err = None;
            }
            Err(e) => self.edit_err = Some(e),
        }
    }

    fn channel_ui(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("channel")
            .num_columns(2)
            .spacing(vec2(12.0, 6.0))
            .show(ui, |ui| {
                ui.label("Preset");
                egui::ComboBox::from_id_salt("preset")
                    .selected_text("choose…")
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for p in presets() {
                            if ui.selectable_label(false, p.name).clicked() {
                                self.s.rx_freq_hz = p.rx_freq_hz;
                                self.s.offset_hz = p.offset_hz;
                                self.s.tone = p.tone;
                                self.s.narrow = p.narrow;
                                self.load_texts();
                            }
                        }
                    });
                ui.end_row();

                let mut changed = false;
                ui.label("RX (output) MHz");
                changed |= ui.text_edit_singleline(&mut self.rx_text).lost_focus();
                ui.end_row();

                ui.label("Offset");
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.off_text)
                            .hint_text("-600k, +5M, -25M"),
                    )
                    .lost_focus();
                ui.end_row();

                ui.label("Tone");
                ui.horizontal(|ui| {
                    for (k, name) in [
                        (ToneKind::None, "none"),
                        (ToneKind::Ctcss, "CTCSS"),
                        (ToneKind::Dcs, "DCS"),
                    ] {
                        changed |= ui.selectable_value(&mut self.tone_kind, k, name).changed();
                    }
                    if self.tone_kind != ToneKind::None {
                        let hint = if self.tone_kind == ToneKind::Ctcss {
                            "146.2"
                        } else {
                            "244N"
                        };
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.tone_text)
                                    .hint_text(hint)
                                    .desired_width(70.0),
                            )
                            .lost_focus();
                    }
                });
                ui.end_row();
                if changed {
                    self.apply_texts();
                }

                ui.label("Bandwidth");
                ui.checkbox(&mut self.s.narrow, "narrow FM (12.5 kHz channel, ±2.5 kHz)");
                ui.end_row();

                ui.label("TX (input)");
                ui.label(self.s.tx_freq_hz().map_or("—".into(), fmt_mhz));
                ui.end_row();
            });
        if let Some(e) = &self.edit_err {
            ui.colored_label(RED, e);
        }
    }

    fn gains_ui(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("gains")
            .num_columns(2)
            .spacing(vec2(12.0, 4.0))
            .show(ui, |ui| {
                ui.label("Volume");
                ui.add(egui::Slider::new(&mut self.s.volume, 0.0..=1.0));
                ui.end_row();
                ui.label("Squelch");
                ui.add(egui::Slider::new(&mut self.s.squelch, 0.0..=0.95));
                ui.end_row();
                ui.label("Mic gain");
                ui.add(egui::Slider::new(&mut self.s.mic_gain, 0.0..=8.0).logarithmic(true));
                ui.end_row();
                ui.label("TX IF gain (dB)");
                ui.add(egui::Slider::new(&mut self.s.txvga_db, 0..=47));
                ui.end_row();
                ui.label("TX internal amp");
                ui.checkbox(&mut self.s.tx_amp, "+11-14 dB");
                ui.end_row();
                ui.label("RX LNA (dB)");
                ui.add(egui::Slider::new(&mut self.s.rx_lna_db, 0..=40).step_by(8.0));
                ui.end_row();
                ui.label("RX VGA (dB)");
                ui.add(egui::Slider::new(&mut self.s.rx_vga_db, 0..=62).step_by(2.0));
                ui.end_row();
                ui.label("RX amp");
                ui.checkbox(&mut self.s.rx_amp, "front-end amp");
                ui.end_row();
                ui.label("Time-out timer (s)");
                ui.add(egui::Slider::new(&mut self.s.tot_s, 10..=600));
                ui.end_row();
            });
    }

    fn devices_ui(&mut self, ui: &mut egui::Ui) {
        let pick = |ui: &mut egui::Ui,
                    salt: &str,
                    cur: &mut Option<String>,
                    devs: &[audio::AudioDevice]| {
            let shown = match cur.as_deref() {
                None => "system default".to_string(),
                Some(id) => devs
                    .iter()
                    .find(|d| d.id == id)
                    .map_or(id.to_string(), |d| d.label.clone()),
            };
            egui::ComboBox::from_id_salt(salt)
                .selected_text(shown)
                .width(420.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(cur, None, "system default");
                    for d in devs {
                        ui.selectable_value(cur, Some(d.id.clone()), &d.label);
                    }
                });
        };
        egui::Grid::new("devices").num_columns(2).show(ui, |ui| {
            ui.label("Microphone");
            pick(ui, "mic", &mut self.s.mic_device, &self.mic_names);
            ui.end_row();
            ui.label("Speaker");
            pick(ui, "spk", &mut self.s.speaker_device, &self.spk_names);
            ui.end_row();
        });
        ui.horizontal(|ui| {
            ui.label("UI scale");
            ui.add(egui::Slider::new(&mut self.ui_scale, 0.75..=3.0).step_by(0.25));
        });
        if ui.button("Rescan devices").clicked() {
            self.mic_names = audio::input_devices();
            self.spk_names = audio::output_devices();
        }
    }
}

fn meter(ui: &mut egui::Ui, label: &str, frac: f32, text: String, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_sized(vec2(70.0, 18.0), egui::Label::new(label));
        ui.add(
            egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                .text(text)
                .fill(color)
                .desired_width(320.0),
        );
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.zoom_factor() != self.ui_scale {
            ctx.set_zoom_factor(self.ui_scale);
        }
        let st = self.radio.status();
        let have_radio = matches!(st.mode, Mode::Rx | Mode::Tx | Mode::Refused);
        let auth = self.s.authorize();

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Header: mode badge and the two frequencies.
                ui.horizontal(|ui| {
                    let (txt, col) = match st.mode {
                        Mode::Tx => ("TX", RED),
                        Mode::Rx if st.squelch_open => ("RX ●", GREEN),
                        Mode::Rx => ("RX", Color32::GRAY),
                        Mode::Refused => ("REFUSED", AMBER),
                        Mode::NoRadio => ("NO RADIO", Color32::DARK_GRAY),
                        Mode::Starting => ("…", Color32::DARK_GRAY),
                    };
                    ui.label(RichText::new(txt).size(26.0).strong().color(col));
                    ui.separator();
                    ui.label(
                        RichText::new(mhz_text(self.s.rx_freq_hz))
                            .size(26.0)
                            .monospace(),
                    );
                    if let Some(tx) = self.s.tx_freq_hz().filter(|&t| t != self.s.rx_freq_hz) {
                        ui.label(
                            RichText::new(format!("→ {}", mhz_text(tx)))
                                .size(18.0)
                                .monospace(),
                        );
                    }
                    ui.label(RichText::new(tone_text(self.s.tone)).size(18.0));
                });
                ui.separator();

                self.channel_ui(ui);
                ui.add_space(6.0);

                match &auth {
                    Ok(p) => {
                        ui.colored_label(
                            if p.internal_amp() { AMBER } else { GREEN },
                            format!("✔ {p}"),
                        );
                        for w in p.warnings() {
                            ui.label(RichText::new(format!("⚠ {w}")).small());
                        }
                    }
                    Err(e) => {
                        ui.colored_label(RED, format!("✖ {e}"));
                    }
                }
                if sdrtx_core::policy::OFFBAND_TEST_ENABLED {
                    ui.checkbox(
                        &mut self.s.dummy_load,
                        "HackRF output is into a DUMMY LOAD with NO amplification \
                     (required outside amateur privileges)",
                    );
                }
                ui.add_space(8.0);

                // PTT
                let tx_secs = st.tx_started.map(|t| t.elapsed().as_secs_f32());
                let label = match (tx_secs, &auth, st.tot_latched) {
                    _ if !have_radio => "PTT disabled — no HackRF".to_string(),
                    (_, _, true) => "TIME-OUT — release".to_string(),
                    (Some(t), _, _) => format!("TRANSMITTING  {t:.0} / {} s", self.s.tot_s),
                    (None, Ok(_), _) => "PUSH TO TALK  (hold, or hold Space)".to_string(),
                    (None, Err(_), _) => "PTT disabled — not authorized".to_string(),
                };
                let fill = if st.mode == Mode::Tx {
                    RED
                } else {
                    Color32::from_rgb(0x33, 0x3a, 0x44)
                };
                let btn = egui::Button::new(RichText::new(label).size(22.0).color(Color32::WHITE))
                    .fill(fill)
                    .min_size(vec2(ui.available_width(), 80.0));
                let resp = ui.add_enabled(auth.is_ok() && have_radio, btn);
                let space = ctx
                    .input(|i| i.key_down(Key::Space) && i.viewport().focused.unwrap_or(true))
                    && !ctx.egui_wants_keyboard_input();
                let ptt = auth.is_ok() && have_radio && (resp.is_pointer_button_down_on() || space);
                if std::env::var_os("SDRTX_DEBUG_KEYS").is_some() {
                    ctx.input(|i| {
                        for e in &i.events {
                            if let egui::Event::Key { key, pressed, .. } = e {
                                eprintln!(
                                    "key {key:?} pressed={pressed} focused={:?} key_down(Space)={} wants_kbd={} auth={} radio={} mode={:?}",
                                    i.viewport().focused,
                                    i.key_down(Key::Space),
                                    ctx.egui_wants_keyboard_input(),
                                    auth.is_ok(),
                                    have_radio,
                                    st.mode
                                );
                            }
                        }
                    });
                }
                if ptt != self.last_ptt {
                    eprintln!("PTT {}", if ptt { "ON" } else { "off" });
                    self.last_ptt = ptt;
                }
                self.radio.set_ptt(ptt);
                ui.add_space(8.0);

                // Meters: RX meters only mean something while receiving.
                if st.mode == Mode::Rx {
                    let db = st.rssi_dbfs;
                    meter(
                        ui,
                        "Signal",
                        (db + 90.0) / 90.0,
                        format!("{db:.0} dBFS"),
                        GREEN,
                    );
                    meter(
                        ui,
                        "Quieting",
                        1.0 - st.noise,
                        format!("noise {:.2}", st.noise),
                        GREEN,
                    );
                } else {
                    meter(ui, "Signal", 0.0, "—".into(), GREEN);
                    meter(ui, "Quieting", 0.0, "—".into(), GREEN);
                }
                meter(
                    ui,
                    "Mic",
                    st.mic_peak,
                    format!("{:.0}%", st.mic_peak * 100.0),
                    AMBER,
                );
                meter(
                    ui,
                    "Speaker",
                    st.speaker_peak,
                    format!("{:.0}%", st.speaker_peak * 100.0),
                    GREEN,
                );

                if let Some(m) = &st.message {
                    ui.colored_label(AMBER, m);
                }
                if let Some(m) = &st.audio_error {
                    ui.colored_label(AMBER, m);
                }
                ui.add_space(6.0);
                egui::CollapsingHeader::new("Levels & gains")
                    .default_open(true)
                    .show(ui, |ui| self.gains_ui(ui));
                egui::CollapsingHeader::new("Audio devices").show(ui, |ui| self.devices_ui(ui));
                ui.label(
                    RichText::new(format!(
                        "RX dropped blocks {} · TX underruns {}",
                        st.rx_dropped, st.tx_underruns
                    ))
                    .small()
                    .weak(),
                );
            });
        });

        if self.radio.settings() != self.s {
            self.radio.set_settings(self.s.clone());
        }
        let now = Saved {
            radio: self.s.clone(),
            ui_scale: self.ui_scale,
        };
        if now != self.saved && self.last_save.elapsed() > Duration::from_secs(1) {
            save(&now);
            self.saved = now;
            self.last_save = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("sdrtx")
            .with_inner_size([960.0, 1350.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native("sdrtx", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
