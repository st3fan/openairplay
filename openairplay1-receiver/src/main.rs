//! The standalone Linux/ALSA AirPlay 1 receiver: a CLI over the
//! `openairplay1` library's public API (it is embedder #1), with an ALSA
//! sink and the dB → linear gain volume model.

use std::process::ExitCode;

use log::{debug, info};

mod player;

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
}

fn usage() -> ! {
    eprintln!(
        "usage: openairplay1-receiver [--name NAME] [--port PORT] [--mac AA:BB:CC:DD:EE:FF] \
         [--alsa-device DEV] [--no-audio] [--no-avahi]"
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

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();

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
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                Event::Volume { db } => {
                    debug!("volume {db} dB");
                    gain.set(volume_to_gain(db));
                }
                Event::SessionStarted { rate, channels } => {
                    info!("session started ({rate} Hz, {channels}ch)");
                }
                Event::SessionEnded => info!("session ended"),
                Event::Flushed => debug!("flushed"),
                _ => {}
            }
        }
    });

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
