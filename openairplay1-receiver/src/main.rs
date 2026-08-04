//! The standalone Linux/ALSA AirPlay 1 receiver: a CLI over the
//! `openairplay1` library's public API (it is embedder #1), with an ALSA
//! sink and the dB → linear gain volume model.

use std::process::ExitCode;

use log::{debug, info};

mod images;
mod player;
mod tui;

use crate::images::Protocol;
use crate::player::{volume_to_gain, AlsaSink, NullSink, SharedGain};
use openairplay1::{AudioSink, Event, Receiver};

const DEFAULT_ALSA_DEVICE: &str = "default";

struct Args {
    /// `None` → the library's defaults (name "OpenAirPlay", port 5000).
    name: Option<String>,
    port: Option<u16>,
    mac: Option<[u8; 6]>,
    avahi: bool,
    /// ALSA device, or `None` for `--no-audio`.
    alsa_device: Option<String>,
    /// Full-screen now-playing display instead of log output.
    tui: bool,
    /// Where log output goes; stderr when `None` (and nowhere at all under
    /// `--tui`, which owns the screen).
    log_file: Option<String>,
    /// Forced terminal-graphics protocol, or `None` to detect one.
    images: Option<Protocol>,
}

fn usage() -> ! {
    eprintln!(
        "usage: openairplay1-receiver [--name NAME] [--port PORT] [--mac AA:BB:CC:DD:EE:FF] \
         [--alsa-device DEV] [--no-audio] [--no-avahi] [--tui] [--log-file PATH] \
         [--tui-images auto|kitty|iterm2|none]"
    );
    std::process::exit(2);
}

/// Parse the `--mac` argument, e.g. `aa:bb:cc:dd:ee:ff`.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.trim().split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

fn parse_args() -> Args {
    let mut args = Args {
        name: None,
        port: None,
        mac: None,
        avahi: true,
        alsa_device: Some(DEFAULT_ALSA_DEVICE.to_string()),
        tui: false,
        log_file: None,
        images: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => args.name = Some(it.next().unwrap_or_else(|| usage())),
            "--port" => {
                args.port = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "--mac" => {
                args.mac = Some(
                    it.next()
                        .as_deref()
                        .and_then(parse_mac)
                        .unwrap_or_else(|| usage()),
                )
            }
            "--alsa-device" => args.alsa_device = Some(it.next().unwrap_or_else(|| usage())),
            "--no-audio" => args.alsa_device = None,
            "--tui" => args.tui = true,
            "--log-file" => args.log_file = Some(it.next().unwrap_or_else(|| usage())),
            "--tui-images" => {
                let value = it.next().unwrap_or_else(|| usage());
                args.images = match value.as_str() {
                    "auto" => None,
                    other => Some(Protocol::parse(other).unwrap_or_else(|| usage())),
                };
            }
            "--no-avahi" => args.avahi = false,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    args
}

/// Point the logger at the right sink. `--log-file` wins; otherwise stderr,
/// except under `--tui`, where stderr would shred the display and the log is
/// dropped unless the user asked for a file.
fn init_logging(args: &Args) -> Result<(), String> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    match (&args.log_file, args.tui) {
        (Some(path), _) => {
            let file = std::fs::File::create(path)
                .map_err(|e| format!("cannot write log file {path:?}: {e}"))?;
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
        (None, true) => {
            builder.target(env_logger::Target::Pipe(Box::new(std::io::sink())));
        }
        (None, false) => {}
    }
    builder.init();
    Ok(())
}

/// The non-TUI display: one line per event.
fn log_event(event: Event) {
    match event {
        Event::SessionStarted {
            rate,
            channels,
            peer,
            ..
        } => info!("session started ({rate} Hz, {channels}ch) from {peer}"),
        Event::Progress { elapsed, duration } => debug!(
            "progress {:.0}s / {:.0}s",
            elapsed.as_secs_f32(),
            duration.as_secs_f32()
        ),
        Event::Metadata {
            title,
            artist,
            album,
        } => {
            let unknown = || "?".to_string();
            info!(
                "now playing: {} — {} ({})",
                artist.unwrap_or_else(unknown),
                title.unwrap_or_else(unknown),
                album.unwrap_or_else(unknown)
            );
        }
        Event::Artwork { content_type, data } => {
            if data.is_empty() {
                info!("artwork cleared ({content_type})");
            } else {
                info!("artwork: {content_type}, {} bytes", data.len());
            }
        }
        Event::SessionEnded => info!("session ended"),
        Event::Flushed => debug!("flushed"),
        // Volume is logged where the gain is applied.
        _ => {}
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse_args();
    if let Err(e) = init_logging(&args) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }

    let mut builder = Receiver::builder().advertise(args.avahi);
    if let Some(name) = args.name {
        builder = builder.name(name);
    }
    if let Some(port) = args.port {
        builder = builder.port(port);
    }
    if let Some(mac) = args.mac {
        builder = builder.mac(mac);
    }
    let receiver = match builder.build() {
        Ok(receiver) => receiver,
        Err(e) => {
            eprintln!("cannot build the receiver: {e}");
            return ExitCode::FAILURE;
        }
    };

    info!(
        "starting AirPlay 1 receiver \"{}\" (mac {}, rtsp port {})",
        receiver.config().name,
        receiver.config().mac_hex(),
        receiver.config().port
    );
    match &args.alsa_device {
        Some(dev) => info!("audio output: ALSA \"{dev}\""),
        None => info!("audio output: disabled (--no-audio)"),
    }

    // The sink seam: the library delivers PCM to an AlsaSink per stream and
    // reports session events; the volume path is ours (dB → linear gain,
    // shared with the sink so slider moves apply live).
    let gain = SharedGain::new();
    let sink_gain = gain.clone();
    let device = args.alsa_device;
    let sink_factory = move |rate: u32, channels: u8| -> Box<dyn AudioSink> {
        match &device {
            Some(dev) => Box::new(AlsaSink::open(dev, rate, channels, sink_gain.clone())),
            None => Box::new(NullSink),
        }
    };
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    // Events drive two things: our gain (always) and the display — the log
    // normally, the TUI when it owns the screen.
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let tui_mode = args.tui;
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if let Event::Volume { db } = &event {
                debug!("volume {db} dB");
                gain.set(volume_to_gain(*db));
            }
            if tui_mode {
                if ui_tx.send(event).is_err() {
                    break; // the display is gone; nothing left to update
                }
            } else {
                log_event(event);
            }
        }
    });

    if args.tui {
        // Ctrl-C is deliberately not selected on here: the terminal is in
        // raw mode, so it arrives as a key event inside the TUI, and
        // cancelling the TUI future from outside would skip its restore.
        let name = receiver.config().name.clone();
        // Detection has to happen before ratatui owns the terminal: the
        // probe writes a query and reads the answer itself.
        let images = args.images.unwrap_or_else(|| {
            let probe = images::probe_kitty(std::time::Duration::from_millis(100));
            images::detect(|name| std::env::var(name).ok(), probe)
        });
        info!("terminal graphics: {images:?}");
        tokio::select! {
            result = receiver.run(sink_factory, event_tx) => {
                if let Err(e) = result {
                    eprintln!("server error: {e}");
                    return ExitCode::FAILURE;
                }
            }
            exit = tui::run(ui_rx, name, images) => {
                if let Err(e) = exit {
                    eprintln!("display error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    tokio::select! {
        result = receiver.run(sink_factory, event_tx) => {
            if let Err(e) = result {
                eprintln!("server error: {e}");
                return ExitCode::FAILURE;
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
        }
    }
    ExitCode::SUCCESS
}
