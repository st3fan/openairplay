//! Cover art on terminals that can draw pixels.
//!
//! Two protocols cover the terminals people actually use: the **Kitty
//! graphics protocol** (Ghostty, Kitty, WezTerm) and **iTerm2 inline images**
//! (iTerm2, WezTerm, Konsole). Everything else gets no image and the display
//! falls back to text, which is why [`detect`] must fail *closed* — emitting
//! graphics escapes at a terminal that doesn't understand them dumps
//! base64 across the user's screen.
//!
//! The two protocols differ in what they accept: iTerm2 takes the image file
//! as-is (JPEG included), Kitty takes PNG or raw pixels, so a JPEG is decoded
//! here first. Both scale the image into a box measured in cells, so nothing
//! is resampled.
//!
//! Everything in this module is a pure function over bytes and placement,
//! except [`probe_kitty`] and [`cell_size`], which touch the terminal.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use log::debug;

/// Which terminal-graphics protocol to speak, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// Kitty graphics (`APC _G … ST`): Ghostty, Kitty, WezTerm.
    Kitty,
    /// iTerm2 inline images (`OSC 1337 File=…`): iTerm2, WezTerm, Konsole.
    ITerm2,
    /// No image support — text only.
    #[default]
    None,
}

impl Protocol {
    /// Parse the `--tui-images` argument.
    pub fn parse(value: &str) -> Option<Protocol> {
        match value {
            "kitty" => Some(Protocol::Kitty),
            "iterm2" => Some(Protocol::ITerm2),
            "none" => Some(Protocol::None),
            _ => None,
        }
    }
}

/// Decide from the environment which protocol a terminal speaks.
///
/// `probe` is the result of [`probe_kitty`] when one was run: `Some(true)`
/// settles it outright. The environment lookups are a fallback for terminals
/// that don't answer the query (and for pipes, where no probe is possible).
///
/// Pure so every terminal in the table is a test case; `env` is any lookup,
/// which is [`std::env::var`] in practice.
pub fn detect(env: impl Fn(&str) -> Option<String>, probe: Option<bool>) -> Protocol {
    if probe == Some(true) {
        return Protocol::Kitty;
    }
    let var = |name: &str| env(name).filter(|v| !v.is_empty());
    let term = var("TERM").unwrap_or_default();
    let program = var("TERM_PROGRAM").unwrap_or_default();

    // Kitty graphics. Ghostty reports itself both ways depending on how the
    // terminfo was installed, so check TERM and TERM_PROGRAM.
    if term.contains("kitty")
        || term.contains("ghostty")
        || var("KITTY_WINDOW_ID").is_some()
        || var("GHOSTTY_RESOURCES_DIR").is_some()
        || var("GHOSTTY_BIN_DIR").is_some()
        || program.eq_ignore_ascii_case("ghostty")
        || program.eq_ignore_ascii_case("kitty")
        // WezTerm speaks both; prefer Kitty, which needs no cursor dance.
        || program.eq_ignore_ascii_case("WezTerm")
    {
        return Protocol::Kitty;
    }

    if program.eq_ignore_ascii_case("iTerm.app")
        || var("LC_TERMINAL").is_some_and(|v| v.eq_ignore_ascii_case("iTerm2"))
        || var("KONSOLE_VERSION").is_some()
    {
        return Protocol::ITerm2;
    }

    Protocol::None
}

/// Ask the terminal whether it speaks the Kitty graphics protocol, by
/// transmitting a 1×1 image with `a=q` and waiting briefly for the reply.
///
/// Must run **before** raw mode is handed to the TUI event loop, and returns
/// `None` when the answer is inconclusive (no reply in time, or not a
/// terminal) so [`detect`] can fall back to the environment.
pub fn probe_kitty(timeout: Duration) -> Option<bool> {
    if !io::IsTerminal::is_terminal(&io::stdout()) {
        return None;
    }
    let _raw = RawMode::enable().ok()?;
    let query = format!(
        "\x1b_Gi={PROBE_ID},s=1,v=1,a=q,t=d,f=24;{}\x1b\\",
        STANDARD.encode([0u8, 0, 0])
    );
    let mut stdout = io::stdout();
    stdout.write_all(query.as_bytes()).ok()?;
    stdout.flush().ok()?;

    // The reply is `\x1b_Gi=<id>;OK\x1b\\`. Anything else — including a
    // terminal that echoes nothing — leaves this inconclusive.
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while Instant::now() < deadline {
        match read_with_timeout(&mut byte, Duration::from_millis(20)) {
            Ok(1) => {
                reply.push(byte[0]);
                if reply.ends_with(b"\x1b\\") {
                    break;
                }
            }
            Ok(_) => break,
            Err(_) => break,
        }
    }
    let reply = String::from_utf8_lossy(&reply).into_owned();
    debug!("kitty graphics probe reply: {reply:?}");
    reply.contains(&format!("i={PROBE_ID}")).then_some(true)
}

/// Image id for the probe, distinct from the one the display uses.
const PROBE_ID: u32 = 7331;
/// Image id the now-playing artwork lives under.
const ARTWORK_ID: u32 = 7332;
/// Kitty wants base64 payloads split into chunks of at most 4096 bytes.
const CHUNK: usize = 4096;

/// Where an image goes: a box in character cells, 1-based like the cursor
/// positioning escape itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

/// The escape sequence that draws `image` at `placement`, or `None` if the
/// terminal can't draw images or the payload isn't usable.
pub fn draw(
    protocol: Protocol,
    content_type: &str,
    image: &[u8],
    placement: Placement,
) -> Option<Vec<u8>> {
    if placement.cols == 0 || placement.rows == 0 || image.is_empty() {
        return None;
    }
    match protocol {
        Protocol::Kitty => kitty_draw(content_type, image, placement),
        Protocol::ITerm2 => Some(iterm2_draw(image, placement)),
        Protocol::None => None,
    }
}

/// The escape sequence that removes a previously drawn image. iTerm2 images
/// are part of the text grid and disappear when the cells are redrawn, so
/// only Kitty needs an explicit delete.
pub fn clear(protocol: Protocol) -> Option<Vec<u8>> {
    match protocol {
        Protocol::Kitty => Some(format!("\x1b_Ga=d,d=I,i={ARTWORK_ID}\x1b\\").into_bytes()),
        Protocol::ITerm2 | Protocol::None => None,
    }
}

/// Kitty: PNG goes over as-is (`f=100`); anything else is decoded to RGB
/// (`f=24`), which in practice means the JPEG every sender sends.
fn kitty_draw(content_type: &str, image: &[u8], placement: Placement) -> Option<Vec<u8>> {
    let (format_keys, payload) = if is_png(content_type, image) {
        ("f=100".to_string(), image.to_vec())
    } else {
        let rgb = decode_jpeg(image)?;
        (format!("f=24,s={},v={}", rgb.width, rgb.height), rgb.pixels)
    };

    let mut out = cursor_to(placement);
    let encoded = STANDARD.encode(&payload);
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            // a=T transmit and display, C=1 leave the cursor alone, q=2 stay
            // quiet, c/r scale the image into the box we laid out.
            out.extend_from_slice(
                format!(
                    "\x1b_Ga=T,i={ARTWORK_ID},{format_keys},c={},r={},C=1,q=2,m={more};",
                    placement.cols, placement.rows
                )
                .as_bytes(),
            );
            first = false;
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    Some(out)
}

/// iTerm2: the file bytes as-is, whatever the format, scaled into the box.
fn iterm2_draw(image: &[u8], placement: Placement) -> Vec<u8> {
    let mut out = cursor_to(placement);
    out.extend_from_slice(
        format!(
            "\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=1;\
             doNotMoveCursor=1:",
            image.len(),
            placement.cols,
            placement.rows
        )
        .as_bytes(),
    );
    out.extend_from_slice(STANDARD.encode(image).as_bytes());
    out.push(0x07); // BEL terminates OSC 1337
    out
}

/// Park the cursor at the top-left of the box before drawing.
fn cursor_to(placement: Placement) -> Vec<u8> {
    format!("\x1b[{};{}H", placement.row, placement.col).into_bytes()
}

fn is_png(content_type: &str, image: &[u8]) -> bool {
    content_type.eq_ignore_ascii_case("image/png") || image.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Decoded pixels, always three bytes per pixel — what Kitty's `f=24` wants.
struct Rgb {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Decode a JPEG to RGB. Grayscale is expanded; anything unsupported (or
/// corrupt) returns `None` and the display simply shows no artwork.
fn decode_jpeg(image: &[u8]) -> Option<Rgb> {
    use jpeg_decoder::PixelFormat;

    let mut decoder = jpeg_decoder::Decoder::new(image);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let pixels = match info.pixel_format {
        PixelFormat::RGB24 => pixels,
        PixelFormat::L8 => pixels.iter().flat_map(|&v| [v, v, v]).collect(),
        other => {
            debug!("artwork: unsupported JPEG pixel format {other:?}");
            return None;
        }
    };
    Some(Rgb {
        width: info.width as u32,
        height: info.height as u32,
        pixels,
    })
}

/// The terminal's cell size in pixels, as `(width, height)`, when the
/// terminal reports one. Used to keep square cover art square.
pub fn cell_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: ws is a valid winsize for the kernel to fill in.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

/// How many times taller than wide a cell is; the fallback matches the usual
/// terminal font.
pub const DEFAULT_CELL_ASPECT: f32 = 2.0;

/// Cell aspect (height ÷ width) from the terminal, or the default.
pub fn cell_aspect() -> f32 {
    match cell_size() {
        Some((w, h)) if w > 0 => h as f32 / w as f32,
        _ => DEFAULT_CELL_ASPECT,
    }
}

/// Raw mode just for the probe: the reply must not be line-buffered or
/// echoed. Restores the previous settings on drop.
struct RawMode;

impl RawMode {
    fn enable() -> io::Result<RawMode> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Read one byte from stdin, giving up after `timeout`.
fn read_with_timeout(buf: &mut [u8; 1], timeout: Duration) -> io::Result<usize> {
    if crossterm::event::poll(timeout)? {
        io::stdin().read(buf)
    } else {
        Err(io::Error::new(io::ErrorKind::TimedOut, "no reply"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detects_kitty_family_terminals() {
        for env in [
            vec![("TERM", "xterm-kitty")],
            vec![("TERM", "xterm-ghostty")],
            vec![("TERM", "xterm-256color"), ("TERM_PROGRAM", "ghostty")],
            vec![("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")],
            vec![
                ("TERM", "xterm-256color"),
                ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty"),
            ],
            vec![("TERM", "xterm-256color"), ("TERM_PROGRAM", "WezTerm")],
        ] {
            assert_eq!(
                detect(env_of(&env), None),
                Protocol::Kitty,
                "should be Kitty: {env:?}"
            );
        }
    }

    #[test]
    fn detects_iterm2_family_terminals() {
        for env in [
            vec![("TERM_PROGRAM", "iTerm.app")],
            vec![("LC_TERMINAL", "iTerm2")],
            vec![("KONSOLE_VERSION", "220400")],
        ] {
            assert_eq!(
                detect(env_of(&env), None),
                Protocol::ITerm2,
                "should be iTerm2: {env:?}"
            );
        }
    }

    #[test]
    fn unknown_terminals_get_no_images() {
        // Failing closed matters: escapes at a terminal that can't read them
        // spray base64 over the screen.
        for env in [
            vec![("TERM", "xterm-256color")],
            vec![("TERM", "screen")],
            vec![("TERM", "dumb")],
            vec![],
        ] {
            assert_eq!(
                detect(env_of(&env), None),
                Protocol::None,
                "should be None: {env:?}"
            );
        }
    }

    #[test]
    fn a_positive_probe_settles_it() {
        // Even a terminal we'd otherwise write off: it answered the query.
        assert_eq!(
            detect(env_of(&[("TERM", "xterm-256color")]), Some(true)),
            Protocol::Kitty
        );
        // A negative probe is not proof — some terminals stay silent — so
        // the environment still decides.
        assert_eq!(
            detect(env_of(&[("TERM", "xterm-kitty")]), Some(false)),
            Protocol::Kitty
        );
    }

    fn placement() -> Placement {
        Placement {
            col: 5,
            row: 3,
            cols: 20,
            rows: 10,
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    #[test]
    fn kitty_transmits_png_untouched() {
        let out = draw(Protocol::Kitty, "image/png", PNG, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("\x1b[3;5H"), "cursor first: {text:?}");
        assert!(text.contains("\x1b_Ga=T,i=7332,f=100,c=20,r=10,C=1,q=2,m=0;"));
        assert!(text.contains(&STANDARD.encode(PNG)), "payload as-is");
        assert!(text.ends_with("\x1b\\"));
    }

    #[test]
    fn kitty_chunks_large_payloads() {
        // Two chunks: the first says m=1 (more coming), the last m=0.
        let big = {
            let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
            v.extend(std::iter::repeat_n(0xab, 8 * 1024));
            v
        };
        let out = draw(Protocol::Kitty, "image/png", &big, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(",m=1;"), "first chunk continues");
        assert!(text.contains("\x1b_Gm=0;"), "last chunk ends the transfer");
        let payload: String = text
            .split("\x1b\\")
            // Only the graphics escapes carry payload; the cursor move that
            // precedes the first one has a ';' of its own.
            .filter_map(|part| part.rsplit_once("\x1b_G").map(|(_, cmd)| cmd))
            .filter_map(|cmd| cmd.split_once(';').map(|(_, data)| data))
            .collect();
        assert_eq!(payload, STANDARD.encode(&big), "chunks must reassemble");
    }

    /// An 8×8 JPEG and the same image as PNG, so the decode path is exercised
    /// with something a decoder actually accepts.
    const JPEG: &[u8] = include_bytes!("../tests/data/tiny.jpg");
    const REAL_PNG: &[u8] = include_bytes!("../tests/data/tiny.png");

    #[test]
    fn kitty_decodes_jpeg_to_raw_pixels() {
        // Kitty takes PNG or pixels, never JPEG — senders send JPEG.
        let out = draw(Protocol::Kitty, "image/jpeg", JPEG, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("f=24,s=8,v=8,"),
            "must declare RGB and the decoded size: {}",
            &text[..80.min(text.len())]
        );
        let payload: Vec<u8> = STANDARD
            .decode(
                text.split("\x1b\\")
                    .filter_map(|part| part.rsplit_once("\x1b_G").map(|(_, cmd)| cmd))
                    .filter_map(|cmd| cmd.split_once(';').map(|(_, data)| data))
                    .collect::<String>(),
            )
            .unwrap();
        assert_eq!(payload.len(), 8 * 8 * 3, "three bytes per pixel");
    }

    #[test]
    fn kitty_sends_a_real_png_without_decoding_it() {
        let out = draw(Protocol::Kitty, "image/png", REAL_PNG, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("f=100,"), "PNG goes over as a file");
        assert!(text.contains(&STANDARD.encode(REAL_PNG)));
    }

    #[test]
    fn png_is_recognized_by_its_magic_even_if_mislabelled() {
        // Some senders label everything image/jpeg; the bytes decide.
        let out = draw(Protocol::Kitty, "image/jpeg", REAL_PNG, placement()).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("f=100,"));
    }

    #[test]
    fn kitty_drops_artwork_it_cannot_decode() {
        // Not a PNG and not a decodable JPEG: no escape at all, rather than
        // a half-written image.
        assert!(draw(Protocol::Kitty, "image/jpeg", b"not a jpeg", placement()).is_none());
    }

    #[test]
    fn iterm2_sends_the_file_as_is() {
        let jpeg = b"\xff\xd8\xff\xe0 pretend jpeg";
        let out = draw(Protocol::ITerm2, "image/jpeg", jpeg, placement()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("\x1b[3;5H"));
        assert!(text.contains(&format!(
            "\x1b]1337;File=inline=1;size={};width=20;height=10;preserveAspectRatio=1;\
             doNotMoveCursor=1:",
            jpeg.len()
        )));
        assert!(text.contains(&STANDARD.encode(jpeg)));
        assert!(text.ends_with('\u{7}'), "OSC ends with BEL");
    }

    #[test]
    fn nothing_is_drawn_without_a_protocol_or_a_box() {
        assert!(draw(Protocol::None, "image/png", PNG, placement()).is_none());
        let empty = Placement {
            cols: 0,
            ..placement()
        };
        assert!(draw(Protocol::Kitty, "image/png", PNG, empty).is_none());
        assert!(draw(Protocol::Kitty, "image/png", b"", placement()).is_none());
    }

    #[test]
    fn only_kitty_needs_an_explicit_delete() {
        assert_eq!(
            clear(Protocol::Kitty).map(String::from_utf8),
            Some(Ok("\x1b_Ga=d,d=I,i=7332\x1b\\".to_string()))
        );
        assert!(clear(Protocol::ITerm2).is_none());
        assert!(clear(Protocol::None).is_none());
    }

    #[test]
    fn parses_the_cli_choice() {
        assert_eq!(Protocol::parse("kitty"), Some(Protocol::Kitty));
        assert_eq!(Protocol::parse("iterm2"), Some(Protocol::ITerm2));
        assert_eq!(Protocol::parse("none"), Some(Protocol::None));
        assert_eq!(Protocol::parse("auto"), None); // handled by the caller
        assert_eq!(Protocol::parse("sixel"), None);
    }
}
