//! A full-screen now-playing display for an `openairplay1-receiver`.
//!
//! The receiver publishes what it is playing on a WebSocket
//! (`--dashboard-listen`); this connects to it and draws the current track,
//! its cover art, and the stream's details. It is a read-only view: it never
//! sends the receiver anything, and it can be started, stopped and restarted
//! independently of it.

use std::process::ExitCode;

use log::info;

mod client;
mod images;
mod tui;

use crate::images::Protocol;

/// Where a receiver serves its dashboard endpoint by default.
const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:7392";

struct Args {
    endpoint: String,
    /// Forced terminal-graphics protocol, or `None` to detect one.
    images: Option<Protocol>,
    /// Where log output goes. The display owns the screen, so logs are
    /// dropped unless this names a file.
    log_file: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: openairplay1-dashboard [--connect ws://HOST:PORT] \
         [--images auto|kitty|iterm2|none] [--log-file PATH]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        endpoint: DEFAULT_ENDPOINT.to_string(),
        images: None,
        log_file: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--connect" => args.endpoint = it.next().unwrap_or_else(|| usage()),
            "--images" => {
                let value = it.next().unwrap_or_else(|| usage());
                args.images = match value.as_str() {
                    "auto" => None,
                    other => Some(Protocol::parse(other).unwrap_or_else(|| usage())),
                };
            }
            "--log-file" => args.log_file = Some(it.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    args
}

/// Logs go to a file or nowhere: stderr would shred the display.
fn init_logging(args: &Args) -> Result<(), String> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    match &args.log_file {
        Some(path) => {
            let file = std::fs::File::create(path)
                .map_err(|e| format!("cannot write log file {path:?}: {e}"))?;
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
        None => {
            builder.target(env_logger::Target::Pipe(Box::new(std::io::sink())));
        }
    }
    builder.init();
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse_args();
    if let Err(e) = init_logging(&args) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }

    // Detection has to happen before ratatui owns the terminal: the probe
    // writes a query and reads the answer itself.
    let images = args.images.unwrap_or_else(|| {
        let probe = images::probe_kitty(std::time::Duration::from_millis(100));
        images::detect(|name| std::env::var(name).ok(), probe)
    });
    info!(
        "connecting to {}, terminal graphics: {images:?}",
        args.endpoint
    );

    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(client::run(args.endpoint.clone(), updates_tx));

    match tui::run(updates_rx, args.endpoint, images).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("display error: {e}");
            ExitCode::FAILURE
        }
    }
}
