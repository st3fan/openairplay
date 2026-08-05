//! RTSP accept loop and request dispatch.
//!
//! Each TCP connection owns one session state machine that advances through
//! ANNOUNCE → SETUP → RECORD and runs the UDP audio receiver. OPTIONS and the
//! Apple-Challenge signature are handled here; the audio methods delegate to
//! the session.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use log::{debug, warn};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};

use crate::events::EventSender;
use crate::rtsp::{read_request, Request, Response};
use crate::session::{AudioObserver, Session, SessionSlot};
use crate::sink::SinkFactory;
use crate::{crypto, Config};

pub const SERVER_ID: &str = "AirTunes/105.1";
pub const PUBLIC_METHODS: &str =
    "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER";

/// Everything a connection handler needs, shared across the server.
pub struct Context {
    pub config: Config,
    /// Creates each stream's audio sink at SETUP.
    pub sink_factory: SinkFactory,
    /// Session milestones for the host.
    pub events: EventSender,
}

pub async fn serve(listener: TcpListener, context: Arc<Context>) -> io::Result<()> {
    serve_with_observer(listener, context, None).await
}

/// Like [`serve`], but every decrypted audio packet is also forwarded to
/// `observer`. Used by integration tests to inspect the crypto path; the
/// production entry point passes `None`.
pub async fn serve_with_observer(
    listener: TcpListener,
    context: Arc<Context>,
    observer: Option<AudioObserver>,
) -> io::Result<()> {
    // Shared across connections so only one client can stream at a time.
    let slot = SessionSlot::new();
    loop {
        let (stream, peer) = listener.accept().await?;
        let context = context.clone();
        let observer = observer.clone();
        let slot = slot.clone();
        tokio::spawn(async move {
            debug!("[{peer}] connected");
            if let Err(e) = handle_connection(stream, peer, context, observer, slot).await {
                warn!("[{peer}] connection error: {e}");
            }
            debug!("[{peer}] disconnected");
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    context: Arc<Context>,
    observer: Option<AudioObserver>,
    slot: SessionSlot,
) -> io::Result<()> {
    let local_addr = stream.local_addr()?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut session = Session::new(
        observer,
        context.sink_factory.clone(),
        context.events.clone(),
        peer.ip(),
        slot,
    );

    while let Some(request) = read_request(&mut reader).await? {
        log_request(&peer, &request);
        let response = dispatch(&mut session, &request, local_addr, &context.config).await;
        debug!("[{peer}] -> {}", response.status());
        response.write_to(&mut write_half).await?;
    }
    Ok(())
}

fn log_request(peer: &SocketAddr, request: &Request) {
    debug!("[{peer}] {} {}", request.method, request.uri);
    for (name, value) in request.headers.iter() {
        debug!("[{peer}]   {name}: {value}");
    }
    if request.body.is_empty() {
        return;
    }
    let content_type = request.headers.get("Content-Type").unwrap_or("");
    let printable = content_type.starts_with("text/")
        || content_type.contains("sdp")
        || content_type.contains("parameters");
    if printable {
        debug!(
            "[{peer}]   body:\n{}",
            String::from_utf8_lossy(&request.body)
        );
    } else {
        debug!(
            "[{peer}]   body: {} bytes of {content_type}",
            request.body.len()
        );
    }
}

async fn dispatch(
    session: &mut Session,
    request: &Request,
    local_addr: SocketAddr,
    config: &Config,
) -> Response {
    // Pincode / Digest auth gate (the classic AirPlay 1 password), checked
    // before any method runs, mirroring shairport-sync. A protected receiver
    // challenges with 401 + WWW-Authenticate until the client answers with a
    // valid `Authorization: Digest` header; without a configured pincode
    // every connection is authorized immediately and nothing changes. On a
    // 401 the common headers below (CSeq, Server, Apple-Response) still
    // apply, and the Apple-Challenge is still answered.
    let mut response = match session.authenticate(config.pincode.as_deref(), request) {
        Some(denied) => denied,
        None => match request.method.as_str() {
            "OPTIONS" => Response::ok().header("Public", PUBLIC_METHODS),
            "ANNOUNCE" => session.handle_announce(request),
            "SETUP" => session.handle_setup(request, local_addr.ip()).await,
            "RECORD" => session.handle_record(request),
            _ => session.handle_other(request).unwrap_or_else(|| {
                warn!("method {} not implemented", request.method);
                Response::new(501, "Not Implemented")
            }),
        },
    };

    if let Some(cseq) = request.headers.get("CSeq") {
        response = response.header("CSeq", cseq);
    }
    response = response
        .header("Server", SERVER_ID)
        .header("Audio-Jack-Status", "connected; type=analog");

    // Any request may carry a challenge; the client drops the connection if
    // the response is missing or wrong.
    if let Some(challenge) = request.headers.get("Apple-Challenge") {
        match crypto::apple_response(challenge, local_addr.ip(), &config.mac) {
            Ok(value) => response = response.header("Apple-Response", value),
            Err(e) => warn!("cannot answer Apple-Challenge: {e}"),
        }
    }

    response
}
