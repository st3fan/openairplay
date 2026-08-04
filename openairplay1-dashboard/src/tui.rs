//! The full-screen now-playing display.
//!
//! The receiver's message stream is the whole data model — this module keeps
//! the latest [`Message`] values in [`NowPlaying`] and draws them centered on
//! the screen.
//!
//! Everything that decides *what the screen looks like* is in
//! [`NowPlaying::lines`] and [`layout`], which take no terminal and no I/O,
//! so the tests render them through ratatui's `TestBackend` instead of a
//! real terminal.

use std::io::Write;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::client::Update;
use crate::images::{self, Placement, Protocol};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use openairplay1_dashboard_protocol::{Message, Snapshot};

/// A slow heartbeat redraw. The clock no longer needs it — positions arrive
/// from the receiver — but it keeps the screen honest if anything else drifts.
const TICK: Duration = Duration::from_secs(1);

/// The played part of the progress bar. Cyan reads clearly on both light and
/// dark terminal themes, and comes from the user's own palette rather than a
/// hard-coded RGB that might clash with it.
const PLAYED_COLOR: Color = Color::Cyan;
/// The part still to play: the dimmest colour that is still a colour, so the
/// contrast with the played part carries the meaning.
const REMAINING_COLOR: Color = Color::DarkGray;

/// The stream's format and where it came from, known from `SessionStarted`.
#[derive(Debug, Clone, PartialEq)]
struct Session {
    rate: u32,
    channels: u8,
    peer: String,
}

/// Whether we can see the receiver at all — shown in place of the track when
/// the socket is down, so a stale screen is never mistaken for live state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum Connection {
    #[default]
    Connecting,
    Connected,
    Lost,
}

/// Where playback is, as last reported.
///
/// Nothing here is extrapolated: the receiver reports the position from the
/// audio it is actually playing, about once a second, so the clock advances
/// because music is playing rather than because time is passing. A sender
/// that pauses simply stops the reports, and the last one stands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Progress {
    elapsed: Duration,
    duration: Duration,
}

/// Cover art exactly as the sender delivered it. Rendering it is the
/// terminal-graphics path; this module only holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Artwork {
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Everything the screen shows, updated from the message stream.
#[derive(Debug, Default)]
pub struct NowPlaying {
    /// Where we are connecting to, shown until a receiver answers.
    endpoint: String,
    /// What this terminal can draw. A terminal with no graphics support gets
    /// no artwork box at all — an empty gap above the text would just be a
    /// hole where a picture isn't.
    images: Protocol,
    connection: Connection,
    /// The receiver's advertised name, from its snapshot.
    name: String,
    session: Option<Session>,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    volume_db: Option<f32>,
    progress: Option<Progress>,
    artwork: Option<Artwork>,
}

impl NowPlaying {
    pub fn new(endpoint: String, images: Protocol) -> NowPlaying {
        NowPlaying {
            endpoint,
            images,
            ..NowPlaying::default()
        }
    }

    /// Is there artwork *and* a way to draw it? Both halves matter: the box
    /// is only worth reserving if a picture will land in it.
    fn shows_artwork(&self) -> bool {
        self.artwork.is_some() && self.images != Protocol::None
    }

    /// Fold one client update into the display state.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Connected => self.connection = Connection::Connected,
            Update::Disconnected => self.connection = Connection::Lost,
            Update::Message(message) => self.apply_message(*message),
        }
    }

    fn apply_message(&mut self, message: Message) {
        match message {
            Message::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            Message::SessionStarted {
                rate,
                channels,
                peer,
            } => {
                self.session = Some(Session {
                    rate,
                    channels,
                    peer,
                });
            }
            Message::Metadata {
                title,
                artist,
                album,
            } => {
                // A complete statement about the track, not a delta.
                self.title = title;
                self.artist = artist;
                self.album = album;
            }
            Message::Artwork {
                content_type,
                data_base64,
            } => self.artwork = decode_artwork(&content_type, &data_base64),
            Message::Volume { db } => self.volume_db = Some(db),
            Message::Progress {
                elapsed_ms,
                duration_ms,
            } => {
                self.progress = Some(Progress {
                    elapsed: Duration::from_millis(elapsed_ms),
                    duration: Duration::from_millis(duration_ms),
                });
            }
            // A seek stops the position where it was until playback reports
            // again — a moment later, from the audio itself. Clearing it here
            // would blink the clock out on every seek, and on the way into
            // every pause.
            Message::Flushed => {}
            Message::SessionEnded => {
                self.session = None;
                self.title = None;
                self.artist = None;
                self.album = None;
                self.progress = None;
                self.artwork = None;
            }
            _ => {}
        }
    }

    /// Replace everything from a snapshot — what a dashboard gets on connect,
    /// and again if the receiver decides it fell behind.
    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.name = snapshot.receiver.name;
        self.session = snapshot.session.map(|s| Session {
            rate: s.rate,
            channels: s.channels,
            peer: s.peer,
        });
        let track = snapshot.track.unwrap_or_default();
        self.title = track.title;
        self.artist = track.artist;
        self.album = track.album;
        self.volume_db = snapshot.volume_db;
        self.progress = snapshot.progress.map(|p| Progress {
            elapsed: Duration::from_millis(p.elapsed_ms),
            duration: Duration::from_millis(p.duration_ms),
        });
        self.artwork = snapshot
            .artwork
            .and_then(|a| decode_artwork(&a.content_type, &a.data_base64));
    }

    fn playing(&self) -> bool {
        self.session.is_some()
    }

    /// The position to show: exactly what the receiver last reported.
    fn position(&self) -> Option<(Duration, Duration)> {
        let progress = self.progress?;
        if progress.duration.is_zero() {
            return None; // a stream with no known end: no clock to show
        }
        Some((progress.elapsed.min(progress.duration), progress.duration))
    }

    /// The centered block of text: title/artist/album (or the idle message),
    /// the progress clock, and the status line.
    fn lines(&self, width: u16) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        if self.connection != Connection::Connected {
            lines.push(Line::from(self.endpoint.as_str().bold()));
            lines.push(Line::from(""));
            lines.push(Line::from(
                match self.connection {
                    Connection::Lost => "connection lost, retrying…",
                    _ => "connecting…",
                }
                .dim(),
            ));
            return lines;
        }
        if !self.playing() {
            lines.push(Line::from(self.name.as_str().bold()));
            lines.push(Line::from(""));
            lines.push(Line::from("waiting for a sender…".dim()));
            return lines;
        }

        lines.push(Line::from(
            self.title.as_deref().unwrap_or("Unknown track").bold(),
        ));
        if let Some(artist) = &self.artist {
            lines.push(Line::from(artist.as_str()));
        }
        if let Some(album) = &self.album {
            lines.push(Line::from(album.as_str().dim()));
        }

        if let Some((elapsed, duration)) = self.position() {
            lines.push(Line::from(""));
            lines.push(progress_bar(elapsed, duration, bar_width(width)));
            lines.push(Line::from(
                format!("{} / {}", clock(elapsed), clock(duration)).dim(),
            ));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            self.status(),
            Style::default().add_modifier(Modifier::DIM),
        )));
        lines
    }

    /// `Living Room · 192.168.1.42 · 44100 Hz 2ch · -12.5 dB`
    fn status(&self) -> String {
        let mut parts = vec![self.name.clone()];
        if let Some(session) = &self.session {
            parts.push(session.peer.clone());
            parts.push(format!("{} Hz {}ch", session.rate, session.channels));
        }
        if let Some(db) = self.volume_db {
            parts.push(format!("{db:.1} dB"));
        }
        parts.join(" · ")
    }

    /// Draw the whole screen: text centered, with a box reserved above it
    /// for artwork when there is any to draw. The returned rect is where the
    /// graphics escape should put the image.
    pub fn render(&self, frame: &mut Frame, cell_aspect: f32) -> Rect {
        let area = frame.area();
        let lines = self.lines(area.width);
        let (artwork_area, text_area) =
            layout(area, lines.len() as u16, self.shows_artwork(), cell_aspect);
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            text_area,
        );
        artwork_area
    }
}

/// Decode base64 artwork from the wire. Empty data is the sender's clear;
/// undecodable data is treated the same way, since a broken image is not
/// worth failing a display over.
fn decode_artwork(content_type: &str, data_base64: &str) -> Option<Artwork> {
    if data_base64.is_empty() {
        return None;
    }
    match STANDARD.decode(data_base64) {
        Ok(data) => Some(Artwork {
            content_type: content_type.to_string(),
            data,
        }),
        Err(e) => {
            log::warn!("ignoring undecodable artwork: {e}");
            None
        }
    }
}

/// Split the screen into an (optional) artwork box and the text block,
/// together centered vertically. The artwork box is square in *pixels*, so
/// its height in cells is half its width; it is capped by both the screen
/// height left over after the text and a fraction of the width.
fn layout(area: Rect, text_lines: u16, with_artwork: bool, cell_aspect: f32) -> (Rect, Rect) {
    let gap = if with_artwork { 1 } else { 0 };
    let art_height = if with_artwork {
        let by_width = (area.width / 2).min(20);
        let spare = area.height.saturating_sub(text_lines + gap + 2);
        by_width.min(spare)
    } else {
        0
    };
    let block = art_height + gap + text_lines;
    let [_, top, text, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(art_height + gap),
        Constraint::Length(text_lines),
        Constraint::Fill(1),
    ])
    .areas(center_block(area, block));

    // Cover art is square, so its width in cells is its height scaled by how
    // much taller than wide a cell is. The gap row sits below it.
    let art_width = (art_height as f32 * cell_aspect).round() as u16;
    let artwork = Rect {
        x: top.x + top.width.saturating_sub(art_width) / 2,
        y: top.y,
        width: art_width.min(top.width),
        height: art_height,
    };
    (artwork, text)
}

/// Vertically center a block of `height` rows within `area` (or use all of
/// it when the block doesn't fit).
fn center_block(area: Rect, height: u16) -> Rect {
    if height >= area.height {
        return area;
    }
    Rect {
        y: area.y + (area.height - height) / 2,
        height,
        ..area
    }
}

/// Progress bar width: a fraction of the screen, within sane bounds.
fn bar_width(screen: u16) -> u16 {
    (screen / 3).clamp(10, 40)
}

/// `━━━━━━━──────────` — filled proportionally to `elapsed / duration`.
///
/// The two halves are told apart by **colour** first: heavy-vs-light line
/// glyphs alone are nearly invisible at a glance, which is what this looked
/// like before. The glyphs still differ so the bar keeps working on a
/// terminal with no colour at all.
fn progress_bar(elapsed: Duration, duration: Duration, width: u16) -> Line<'static> {
    let ratio = if duration.is_zero() {
        0.0
    } else {
        (elapsed.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
    };
    let filled = (ratio * width as f64).round() as usize;
    Line::from(vec![
        Span::styled(
            "━".repeat(filled),
            Style::default()
                .fg(PLAYED_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "─".repeat(width as usize - filled),
            Style::default().fg(REMAINING_COLOR),
        ),
    ])
}

/// `1:23`, or `1:02:03` once past an hour.
fn clock(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Why the TUI loop ended.
pub enum Exit {
    /// The user asked to quit (`q`, `Esc`, or `Ctrl-C`).
    Quit,
    /// The client task stopped, so there is nothing left to display.
    Disconnected,
}

/// Restores the terminal however this future ends — returned, errored, or
/// **dropped**. Dropping matters: the caller runs this in a `select!` with
/// the receiver, so the whole loop can be cancelled at an await point, and a
/// missed restore leaves the user in raw mode with no cursor.
struct TerminalGuard {
    images: Protocol,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Kitty images outlive the alternate screen, so drop ours explicitly.
        if let Some(escape) = images::clear(self.images) {
            let _ = std::io::stdout().write_all(&escape);
        }
        ratatui::restore();
    }
}

/// What is currently on screen, so a redraw only re-transmits the image when
/// it has to. Terminal images are not part of ratatui's cell buffer: Kitty
/// placements survive a redraw, but an iTerm2 image is erased whenever its
/// cells are rewritten, so the text layout shifting counts as a change too.
#[derive(PartialEq)]
struct DrawnArtwork {
    artwork: Artwork,
    area: Rect,
}

/// Send (or remove) the artwork escape when what should be on screen and
/// what is on screen have diverged.
fn draw_artwork(
    images: Protocol,
    state: &NowPlaying,
    area: Rect,
    drawn: &mut Option<DrawnArtwork>,
) -> std::io::Result<()> {
    if images == Protocol::None {
        return Ok(());
    }
    let wanted = state
        .artwork
        .as_ref()
        .filter(|_| area.width > 0 && area.height > 0)
        .map(|artwork| DrawnArtwork {
            artwork: artwork.clone(),
            area,
        });
    if wanted == *drawn {
        return Ok(());
    }

    let mut out = std::io::stdout();
    if drawn.is_some() {
        if let Some(escape) = images::clear(images) {
            out.write_all(&escape)?;
        }
    }
    if let Some(wanted) = &wanted {
        let placement = Placement {
            // Escape sequences count from 1; ratatui rects from 0.
            col: wanted.area.x + 1,
            row: wanted.area.y + 1,
            cols: wanted.area.width,
            rows: wanted.area.height,
        };
        match images::draw(
            images,
            &wanted.artwork.content_type,
            &wanted.artwork.data,
            placement,
        ) {
            Some(escape) => out.write_all(&escape)?,
            // Undecodable artwork: leave the screen text-only rather than
            // retrying it on every redraw.
            None => {
                *drawn = None;
                return Ok(());
            }
        }
    }
    out.flush()?;
    *drawn = wanted;
    Ok(())
}

/// Run the full-screen display until the user quits or the receiver stops.
///
/// Owns the terminal for its lifetime: `ratatui::try_init` enables raw mode
/// and the alternate screen and installs a panic hook that restores them;
/// [`TerminalGuard`] covers every other way out.
pub async fn run(
    mut updates: UnboundedReceiver<Update>,
    endpoint: String,
    images: Protocol,
) -> std::io::Result<Exit> {
    // Cell size has to be read before ratatui takes the terminal over; it
    // doesn't change unless the font does.
    let cell_aspect = images::cell_aspect();
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard { images };
    event_loop(&mut terminal, &mut updates, endpoint, images, cell_aspect).await
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    updates: &mut UnboundedReceiver<Update>,
    endpoint: String,
    images: Protocol,
    cell_aspect: f32,
) -> std::io::Result<Exit> {
    let mut state = NowPlaying::new(endpoint, images);
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // Raw mode means the terminal sends no SIGINT, but `kill` and systemd
    // still send SIGTERM — catch it so the screen is handed back.
    let mut terminate = signal(SignalKind::terminate())?;
    let mut drawn: Option<DrawnArtwork> = None;

    loop {
        let mut box_area = Rect::ZERO;
        terminal.draw(|frame| {
            box_area = state.render(frame, cell_aspect);
        })?;
        draw_artwork(images, &state, box_area, &mut drawn)?;

        tokio::select! {
            // The receiver's messages: the display state.
            update = updates.recv() => match update {
                Some(update) => state.apply(update),
                None => return Ok(Exit::Disconnected),
            },
            // Keys and resizes. In raw mode Ctrl-C is a key, not a signal.
            term = input.next() => match term {
                Some(Ok(TermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        return Ok(Exit::Quit);
                    }
                }
                Some(Ok(_)) => {}          // resize and the rest: redraw
                Some(Err(e)) => return Err(e),
                None => return Ok(Exit::Quit),
            },
            _ = terminate.recv() => return Ok(Exit::Quit),
            // Advance the elapsed clock between progress updates.
            _ = tick.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(state: &NowPlaying, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                state.render(frame, images::DEFAULT_CELL_ASPECT);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A connected dashboard with a track playing, built the way a real one
    /// is: a snapshot on connect, then changes.
    fn playing() -> NowPlaying {
        let mut state = connected();
        state.apply(msg(Message::SessionStarted {
            rate: 44100,
            channels: 2,
            peer: "192.168.1.42".into(),
        }));
        state.apply(msg(Message::Metadata {
            title: Some("Sonata No. 1".into()),
            artist: Some("Some Artist".into()),
            album: Some("Some Album".into()),
        }));
        state.apply(msg(Message::Volume { db: -12.5 }));
        state
    }

    /// Connected, with the receiver's snapshot but nothing playing.
    fn connected() -> NowPlaying {
        let mut state = NowPlaying::new("ws://127.0.0.1:7392".into(), Protocol::Kitty);
        state.apply(Update::Connected);
        state.apply(msg(Message::Snapshot(
            openairplay1_dashboard_protocol::Snapshot {
                receiver: openairplay1_dashboard_protocol::ReceiverInfo {
                    name: "Living Room".into(),
                    version: "0.2.0".into(),
                },
                ..Default::default()
            },
        )));
        state
    }

    fn msg(message: Message) -> Update {
        Update::Message(Box::new(message))
    }

    /// Every rendered line is centered within the screen width.
    fn assert_centered(screen: &str, width: u16) {
        for line in screen.lines().filter(|l| !l.trim().is_empty()) {
            let left = line.len() - line.trim_start().len();
            let right = width as usize - line.chars().count();
            assert!(
                left.abs_diff(right) <= 1,
                "line not centered (left {left}, right {right}): {line:?}"
            );
        }
    }

    #[test]
    fn idle_screen_names_the_receiver() {
        let state = connected();
        let screen = draw(&state, 40, 10);
        assert!(screen.contains("Living Room"), "{screen}");
        assert!(screen.contains("waiting for a sender"), "{screen}");
        assert_centered(&screen, 40);
    }

    #[test]
    fn before_a_receiver_answers_the_screen_says_so() {
        // A stale screen must never be mistaken for live state, so the
        // connection state replaces the track entirely.
        let state = NowPlaying::new("ws://127.0.0.1:7392".into(), Protocol::Kitty);
        let screen = draw(&state, 44, 10);
        assert!(screen.contains("ws://127.0.0.1:7392"), "{screen}");
        assert!(screen.contains("connecting"), "{screen}");
        assert_centered(&screen, 44);
    }

    #[test]
    fn losing_the_receiver_is_shown_and_stops_the_clock() {
        let mut state = playing();
        state.apply(msg(Message::Progress {
            elapsed_ms: 1_000,
            duration_ms: 2_000,
        }));
        state.apply(Update::Disconnected);
        let screen = draw(&state, 44, 12);
        assert!(screen.contains("connection lost, retrying"), "{screen}");
        assert!(
            !screen.contains("0:01 / 0:02"),
            "the clock must stop: {screen}"
        );
    }

    #[test]
    fn a_snapshot_fills_the_screen_in_one_go() {
        // What a dashboard started mid-track gets: everything at once.
        use openairplay1_dashboard_protocol as proto;
        let mut state = NowPlaying::new("ws://host:7392".into(), Protocol::Kitty);
        state.apply(Update::Connected);
        state.apply(msg(Message::Snapshot(proto::Snapshot {
            receiver: proto::ReceiverInfo {
                name: "Living Room".into(),
                version: "0.2.0".into(),
            },
            session: Some(proto::SessionInfo {
                rate: 44100,
                channels: 2,
                peer: "192.168.1.42".into(),
            }),
            track: Some(proto::Track {
                title: Some("Sonata No. 1".into()),
                artist: Some("Some Artist".into()),
                album: None,
            }),
            volume_db: Some(-12.5),
            progress: Some(proto::Progress {
                elapsed_ms: 83_000,
                duration_ms: 247_000,
            }),
            artwork: Some(proto::Artwork {
                content_type: "image/jpeg".into(),
                data_base64: STANDARD.encode([1, 2, 3]),
            }),
        })));

        let screen = draw(&state, 60, 20);
        assert!(screen.contains("Sonata No. 1"), "{screen}");
        assert!(screen.contains("Some Artist"), "{screen}");
        assert!(screen.contains("1:23 / 4:07"), "{screen}");
        assert!(
            screen.contains("Living Room · 192.168.1.42 · 44100 Hz 2ch · -12.5 dB"),
            "{screen}"
        );
        assert_eq!(
            state.artwork.as_ref().map(|a| a.data.clone()),
            Some(vec![1, 2, 3]),
            "artwork is decoded from base64"
        );
    }

    #[test]
    fn undecodable_artwork_is_dropped_not_fatal() {
        let mut state = playing();
        state.apply(msg(Message::Artwork {
            content_type: "image/jpeg".into(),
            data_base64: "not base64!!".into(),
        }));
        assert!(state.artwork.is_none());
        assert!(
            draw(&state, 40, 12).contains("Sonata"),
            "the screen survives"
        );
    }

    #[test]
    fn playing_screen_shows_track_and_status() {
        let screen = draw(&playing(), 60, 16);
        assert!(screen.contains("Sonata No. 1"), "{screen}");
        assert!(screen.contains("Some Artist"), "{screen}");
        assert!(screen.contains("Some Album"), "{screen}");
        assert!(
            screen.contains("Living Room · 192.168.1.42 · 44100 Hz 2ch · -12.5 dB"),
            "{screen}"
        );
        assert_centered(&screen, 60);
    }

    #[test]
    fn progress_is_shown_as_a_bar_and_a_clock() {
        let mut state = playing();
        state.apply(msg(Message::Progress {
            elapsed_ms: 83_000,
            duration_ms: 247_000,
        }));
        let screen = draw(&state, 60, 20);
        assert!(screen.contains("1:23 / 4:07"), "{screen}");
        assert!(screen.contains('━') && screen.contains('─'), "{screen}");
    }

    #[test]
    fn the_clock_only_moves_when_the_receiver_says_so() {
        // The pause case, and the reason nothing here extrapolates: with no
        // new report the position stands still, however long we wait.
        let mut state = playing();
        state.apply(msg(Message::Progress {
            elapsed_ms: 83_000,
            duration_ms: 247_000,
        }));
        let before = draw(&state, 60, 20);
        assert!(before.contains("1:23 / 4:07"), "{before}");

        std::thread::sleep(Duration::from_millis(1_100));
        let after = draw(&state, 60, 20);
        assert!(
            after.contains("1:23 / 4:07"),
            "a paused position must not advance on its own: {after}"
        );

        // Playback resuming moves it, because the receiver says it moved.
        state.apply(msg(Message::Progress {
            elapsed_ms: 84_000,
            duration_ms: 247_000,
        }));
        assert!(draw(&state, 60, 20).contains("1:24 / 4:07"));
    }

    #[test]
    fn a_flush_keeps_the_position_on_screen() {
        // A pause usually arrives as FLUSH first; clearing the position here
        // would blank the clock on the way into every pause and blink it on
        // every seek.
        let mut state = playing();
        state.apply(msg(Message::Progress {
            elapsed_ms: 83_000,
            duration_ms: 247_000,
        }));
        state.apply(msg(Message::Flushed));
        assert!(
            draw(&state, 60, 20).contains("1:23 / 4:07"),
            "the last known position should stay put"
        );
    }

    #[test]
    fn a_stream_with_no_known_end_shows_no_clock() {
        let mut state = playing();
        state.apply(msg(Message::Progress {
            elapsed_ms: 10_000,
            duration_ms: 0,
        }));
        let screen = draw(&state, 60, 20);
        assert!(!screen.contains('/'), "{screen}");
    }

    #[test]
    fn missing_metadata_fields_are_skipped_not_blanked() {
        let mut state = playing();
        state.apply(msg(Message::Metadata {
            title: Some("Just A Title".into()),
            artist: None,
            album: None,
        }));
        let screen = draw(&state, 40, 12);
        assert!(screen.contains("Just A Title"), "{screen}");
        assert!(!screen.contains("Some Artist"), "{screen}");
    }

    #[test]
    fn session_end_returns_to_idle() {
        let mut state = playing();
        state.apply(msg(Message::SessionEnded));
        let screen = draw(&state, 40, 10);
        assert!(screen.contains("waiting for a sender"), "{screen}");
        assert!(!screen.contains("Sonata"), "{screen}");
    }

    #[test]
    fn a_tiny_terminal_still_renders() {
        // Narrower and shorter than the content: no panic, no overflow.
        let screen = draw(&playing(), 12, 4);
        assert!(!screen.is_empty());
    }

    /// The vertical middle of the text block on screen, so "centered" can be
    /// asserted rather than eyeballed.
    fn text_rows(screen: &str) -> Vec<usize> {
        screen
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(row, _)| row)
            .collect()
    }

    #[test]
    fn without_terminal_graphics_the_text_is_centered_on_the_screen() {
        // A terminal that can't draw images must not get a hole above the
        // text where a picture isn't.
        let mut state = playing();
        state.images = Protocol::None;
        state.apply(msg(Message::Artwork {
            content_type: "image/jpeg".into(),
            data_base64: STANDARD.encode([1, 2, 3]),
        }));
        assert!(state.artwork.is_some(), "the artwork is still held");

        let height = 21;
        let screen = draw(&state, 60, height);
        let rows = text_rows(&screen);
        let (first, last) = (rows[0], rows[rows.len() - 1]);
        let above = first;
        let below = height as usize - 1 - last;
        assert!(
            above.abs_diff(below) <= 1,
            "text should sit in the middle: {above} rows above, {below} below\n{screen}"
        );
    }

    #[test]
    fn with_terminal_graphics_the_text_makes_room_for_the_artwork() {
        // The same state on a terminal that can draw: the picture's box comes
        // out of the space above, so the text sits lower.
        let mut state = playing();
        state.images = Protocol::Kitty;
        state.apply(msg(Message::Artwork {
            content_type: "image/jpeg".into(),
            data_base64: STANDARD.encode([1, 2, 3]),
        }));
        let with_art = text_rows(&draw(&state, 60, 21))[0];
        state.images = Protocol::None;
        let without_art = text_rows(&draw(&state, 60, 21))[0];
        assert!(
            with_art > without_art,
            "artwork should push the text down ({with_art} vs {without_art})"
        );
    }

    #[test]
    fn artwork_gets_a_box_above_the_text() {
        let area = Rect::new(0, 0, 60, 24);
        let (art, text) = layout(area, 6, true, images::DEFAULT_CELL_ASPECT);
        assert!(art.height > 0 && art.width == art.height * 2);
        assert!(art.y + art.height <= text.y, "artwork must sit above text");
        let (none, _) = layout(area, 6, false, images::DEFAULT_CELL_ASPECT);
        assert_eq!(none.height, 0, "no artwork, no box");
    }

    #[test]
    fn artwork_box_yields_to_a_short_screen() {
        // Eight rows of text on a ten-row screen leaves no room for art.
        let (art, _) = layout(
            Rect::new(0, 0, 60, 10),
            8,
            true,
            images::DEFAULT_CELL_ASPECT,
        );
        assert_eq!(art.height, 0);
    }

    #[test]
    fn empty_artwork_clears_it() {
        let mut state = playing();
        state.apply(msg(Message::Artwork {
            content_type: "image/jpeg".into(),
            data_base64: STANDARD.encode([1, 2, 3]),
        }));
        assert!(state.artwork.is_some());
        state.apply(msg(Message::Artwork {
            content_type: "image/none".into(),
            data_base64: String::new(),
        }));
        assert!(state.artwork.is_none(), "image/none must clear the art");
    }

    #[test]
    fn the_clock_formats_hours_only_when_needed() {
        assert_eq!(clock(Duration::from_secs(0)), "0:00");
        assert_eq!(clock(Duration::from_secs(83)), "1:23");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn the_progress_bar_fills_proportionally() {
        /// The bar as (text, colour) pairs, which is what the screen shows.
        fn parts(elapsed: u64, total: u64, width: u16) -> Vec<(String, Option<Color>)> {
            progress_bar(
                Duration::from_secs(elapsed),
                Duration::from_secs(total),
                width,
            )
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style.fg))
            .collect()
        }

        assert_eq!(
            parts(0, 10, 10),
            vec![
                (String::new(), Some(PLAYED_COLOR)),
                ("──────────".into(), Some(REMAINING_COLOR)),
            ]
        );
        assert_eq!(
            parts(5, 10, 10),
            vec![
                ("━━━━━".into(), Some(PLAYED_COLOR)),
                ("─────".into(), Some(REMAINING_COLOR)),
            ]
        );
        assert_eq!(
            parts(10, 10, 10),
            vec![
                ("━━━━━━━━━━".into(), Some(PLAYED_COLOR)),
                (String::new(), Some(REMAINING_COLOR)),
            ]
        );
        // A position past the end (a seek report we haven't caught up with)
        // must not overflow the bar.
        assert_eq!(parts(99, 10, 10)[0].0.chars().count(), 10);
    }

    #[test]
    fn the_two_halves_of_the_bar_are_different_colours() {
        // The whole point: heavy-vs-light glyphs alone were near-invisible,
        // so the colours must actually differ where the bar is drawn.
        let mut state = playing();
        state.apply(msg(Message::Progress {
            elapsed_ms: 50_000,
            duration_ms: 100_000,
        }));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| {
                state.render(frame, images::DEFAULT_CELL_ASPECT);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();

        let colours: Vec<_> = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| matches!(buffer[(x, y)].symbol(), "━" | "─"))
            .map(|(x, y)| (buffer[(x, y)].symbol().to_string(), buffer[(x, y)].fg))
            .collect();
        assert!(!colours.is_empty(), "the bar should be on screen");
        assert!(
            colours
                .iter()
                .any(|(s, fg)| s == "━" && *fg == PLAYED_COLOR),
            "played part must use the played colour: {colours:?}"
        );
        assert!(
            colours
                .iter()
                .any(|(s, fg)| s == "─" && *fg == REMAINING_COLOR),
            "remaining part must use the remaining colour: {colours:?}"
        );
        assert_ne!(PLAYED_COLOR, REMAINING_COLOR);
    }
}
