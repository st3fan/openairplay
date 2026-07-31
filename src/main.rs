use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;

use log::{info, warn};
use tokio::net::TcpListener;

use openairplay::{avahi, mac, server, Config};

const DEFAULT_NAME: &str = "OpenAirPlay";
const DEFAULT_PORT: u16 = 5000;
const DEFAULT_ALSA_DEVICE: &str = "default";
// Locally-administered fallback for hosts where /sys/class/net yields
// nothing usable; discovery/auth still work, the address just isn't real.
const FALLBACK_MAC: [u8; 6] = [0x02, 0x4f, 0x41, 0x50, 0x31, 0x00];

struct Args {
    name: String,
    port: u16,
    mac: Option<[u8; 6]>,
    avahi: bool,
    alsa_device: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: openairplay [--name NAME] [--port PORT] [--mac AA:BB:CC:DD:EE:FF] \
         [--alsa-device DEV] [--no-audio] [--no-avahi]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        name: DEFAULT_NAME.to_string(),
        port: DEFAULT_PORT,
        mac: None,
        avahi: true,
        alsa_device: Some(DEFAULT_ALSA_DEVICE.to_string()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => args.name = it.next().unwrap_or_else(|| usage()),
            "--port" => {
                args.port = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--mac" => {
                args.mac = Some(
                    it.next()
                        .as_deref()
                        .and_then(mac::parse)
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

    let mac = args.mac.or_else(mac::discover).unwrap_or_else(|| {
        warn!("no network interface MAC found, using a fixed fallback");
        FALLBACK_MAC
    });
    let config = Arc::new(Config {
        name: args.name,
        port: args.port,
        mac,
        alsa_device: args.alsa_device,
    });
    info!(
        "starting receiver \"{}\" (mac {}, rtsp port {}, audio {})",
        config.name,
        config.mac_hex(),
        config.port,
        config.alsa_device.as_deref().unwrap_or("disabled")
    );

    // Prefer a dual-stack socket (IPv4 clients arrive as v4-mapped
    // addresses, which the challenge code handles); fall back to IPv4-only.
    let listener = match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, config.port)))
        .await
    {
        Ok(l) => l,
        Err(_) => {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port))).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("cannot bind RTSP port {}: {e}", config.port);
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let _advertisement = if args.avahi {
        match avahi::publish(&config) {
            Ok(ad) => Some(ad),
            Err(e) => {
                warn!("avahi advertisement disabled ({e}); is avahi-utils installed?");
                None
            }
        }
    } else {
        None
    };

    tokio::select! {
        result = server::serve(listener, config) => {
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
