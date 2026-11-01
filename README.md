# sdrtx

Half-duplex FM transceiver for the HackRF One, with a push-to-talk GUI and a command-line tool.
It refuses to transmit outside US amateur bands above 50 MHz, and the code's types enforce that
rule.

**Status: experimental, provided as-is.** Issues are welcome, but replies may be slow and support
isn't promised.

## Built with AI

sdrtx was written by [Claude Code](https://claude.com/claude-code) working with KJ4PRT, who set the
requirements, made the design decisions, and ran the on-air tests as control operator.

## What has been tested

On-air tests ran on a Raspberry Pi 5 on 10 October 2026, with no external amplifier and an RTL-SDR
as a separate receiver:

| Band | Frequency (MHz) |
|---|---|
| 6m | 52.54 |
| 2m | 146.55 |
| 1.25m | 223.46 |
| 70cm | 446.075 |
| 33cm | 927.100 |
| 23cm | 1294.7 |

Frequency error was -0.06 to -0.38 ppm. Recovered voice correlated 0.87 to 0.93 with the source.
On 33cm the receiver decoded DCS 244N.

**Not tested yet:**

- Harmonics and spurious emissions. They haven't been measured, so don't assume the output meets
  47 CFR 97.307.
- Opening a real repeater.
- Live microphone audio over the air. The tests above used recorded audio.
- External amplifiers, and the 13cm and 5cm bands.

Unit tests cover the band plan, transmit policy, and DSP. The hardware, audio, GUI, and CLI code
has no tests.

## Transmit policy

| Where | Allowed | Amplification |
|---|---|---|
| Whole emission inside a US amateur allocation above 50 MHz (`sdrtx bands`) | yes | yes |
| Anywhere else, including HF | **refused** | n/a |

The check is structural: `HackRf::transmit` takes a `TxPermit`, tunes to the permit's frequency,
and the only constructor for a permit is `sdrtx_core::policy::authorize`.

sdrtx doesn't know your license class, your location, or your local band plan. A permit only means
the frequency is inside the table, and it doesn't make a transmission legal.

You need an amateur license, and you are the control operator for everything you transmit. sdrtx
doesn't send a station ID (47 CFR 97.119), so you have to do that yourself.

### Optional off-band test mode

The default build can't transmit off-band. Building with the `offband-test` feature adds a
`--dummy-load` flag and a GUI switch, for bench tests into a dummy load with no amplification:

```sh
cargo build --release --features sdrtx-core/offband-test
```

The switch is only the operator's statement. Software can't confirm that a dummy load is connected,
and radiating outside your privileges is illegal.

## GUI

`target/release/sdrtx-gui`: hold the PUSH TO TALK button (or Space) to transmit on the repeater
input, and release to receive on the output. PTT is disabled whenever the settings would be refused
or no HackRF is attached. Settings persist in `~/.config/sdrtx/settings.json`.

The presets are Boston-area MMRA repeaters. Edit `presets()` in `crates/radio/src/lib.rs` for your
area.

You need a microphone and speaker (a USB headset works). Pick them under *Audio devices*. RX tunes
the HackRF 250 kHz low to keep its DC spike out of the channel.

## Usage

```sh
sdrtx bands
# 2m repeater: output 146.82, -600 kHz, PL 146.2 -> transmits on 146.22
sdrtx check -f 146.82M --offset -600k --ctcss 146.2
sdrtx tx    -f 146.82M --offset -600k --ctcss 146.2 -s wav:id.wav --txvga 20
# 33cm repeater: output 927.0625, -25 MHz, DCS 244 -> transmits on 902.0625
sdrtx check -f 927.0625M --offset -25M --dcs 244
# Write IQ instead of transmitting (2.4 Msps cs8, playable with hackrf_transfer -t)
sdrtx tx -f 146.52M -s tone:1000 -d 5 --iq-out test.cs8
```

Sources: `tone:<Hz>`, `wav:<path>`, `silence` (FM carrier + CTCSS/DCS), `carrier` (unmodulated).
Sub-tones: `--ctcss <Hz>` or `--dcs <octal>[N|I]` (e.g. `--dcs 244`).
Defaults: TX IF gain 0 dB, internal amp off, ±5 kHz deviation, 180 s time-out timer.

## Hardware notes

- Raw HackRF output has strong harmonics and spurs. Use a band filter after it, and always before
  any external amplifier (47 CFR 97.307).
- The HackRF antenna-port bias tee is forced off on every transmission.
- Never transmit through a receive chain, such as a preamp or LNA.

## Build

Needs `libhackrf-dev` and `libasound2-dev`.

```sh
cargo build --release && cargo test --release
```

Built and tested with rustc 1.99 on a Raspberry Pi 5 running Debian 13. The GUI uses the glow
renderer because wgpu exceeds the Pi 5 GPU's shader limits.

## License

Public domain, under the [Unlicense](LICENSE). sdrtx links against the system libhackrf and uses
Rust crates that keep their own licenses.
