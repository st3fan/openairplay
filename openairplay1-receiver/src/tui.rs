//! `--tui`: a full-screen now-playing display.
//!
//! The receiver's event stream is the whole data model — this module keeps
//! the latest [`Event`] values in [`NowPlaying`] and draws them centered on
//! the screen. It is a pure consumer of the library's public API, like the
//! rest of the binary.
//!
//! Everything that decides *what the screen looks like* is in
//! [`NowPlaying::lines`] and [`layout`], which take no terminal and no I/O,
//! so the tests render them through ratatui's `TestBackend` instead of a
//! real terminal.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc::UnboundedReceiver;

use openairplay1::Event;

/// How often the screen redraws while a track plays, so the elapsed clock
/// advances between the sender's (infrequent) progress updates.
const TICK: Duration = Duration::from_millis(500);

/// The stream's format and where it came from, known from `SessionStarted`.
#[derive(Debug, Clone, PartialEq)]
struct Session {
    rate: u32,
    channels: u8,
    peer: IpAddr,
}

/// The last position the sender reported, and when we heard it — the clock
/// on screen is this extrapolated with the local clock.
#[derive(Debug, Clone)]
struct Progress {
    elapsed: Duration,
    duration: Duration,
    at: Instant,
}

/// Cover art exactly as the sender delivered it. Rendering it is the
/// terminal-graphics path; this module only holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Artwork {
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Everything the screen shows, updated from the event stream.
#[derive(Debug, Default)]
pub struct NowPlaying {
    /// The receiver's own advertised name, shown when idle and in the
    /// status line.
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
    pub fn new(name: String) -> NowPlaying {
        NowPlaying {
            name,
            ..NowPlaying::default()
        }
    }

    /// Fold one event into the display state.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::SessionStarted {
                rate,
                channels,
                peer,
                ..
            } => {
                self.session = Some(Session {
                    rate,
                    channels,
                    peer,
                });
            }
            Event::Metadata {
                title,
                artist,
                album,
            } => {
                // A complete statement about the track, not a delta.
                self.title = title;
                self.artist = artist;
                self.album = album;
            }
            Event::Artwork { content_type, data } => {
                self.artwork = (!data.is_empty()).then_some(Artwork { content_type, data });
            }
            Event::Volume { db } => self.volume_db = Some(db),
            Event::Progress { elapsed, duration } => {
                self.progress = Some(Progress {
                    elapsed,
                    duration,
                    at: Instant::now(),
                });
            }
            // A seek invalidates the position until the sender sends a new
            // one, which it does immediately after.
            Event::Flushed => self.progress = None,
            Event::SessionEnded => {
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

    fn playing(&self) -> bool {
        self.session.is_some()
    }

    /// Position within the track *now*: the sender's last report advanced by
    /// the time since we got it, never past the end of the track.
    fn position(&self) -> Option<(Duration, Duration)> {
        let progress = self.progress.as_ref()?;
        if progress.duration.is_zero() {
            return None; // a stream with no known end: no clock to show
        }
        let elapsed = (progress.elapsed + progress.at.elapsed()).min(progress.duration);
        Some((elapsed, progress.duration))
    }

    /// The centered block of text: title/artist/album (or the idle message),
    /// the progress clock, and the status line.
    fn lines(&self, width: u16) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
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
            lines.push(Line::from(progress_bar(
                elapsed,
                duration,
                bar_width(width),
            )));
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
            parts.push(session.peer.to_string());
            parts.push(format!("{} Hz {}ch", session.rate, session.channels));
        }
        if let Some(db) = self.volume_db {
            parts.push(format!("{db:.1} dB"));
        }
        parts.join(" · ")
    }

    /// Draw the whole screen: artwork box on top (filled in by the graphics
    /// path), text block centered under it.
    pub fn render(&self, frame: &mut Frame) -> Rect {
        let area = frame.area();
        let lines = self.lines(area.width);
        let (artwork_area, text_area) = layout(area, lines.len() as u16, self.artwork.is_some());
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            text_area,
        );
        artwork_area
    }
}

/// Split the screen into an (optional) artwork box and the text block,
/// together centered vertically. The artwork box is square in *pixels*, so
/// its height in cells is half its width; it is capped by both the screen
/// height left over after the text and a fraction of the width.
fn layout(area: Rect, text_lines: u16, with_artwork: bool) -> (Rect, Rect) {
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

    // The artwork is centered horizontally over its own two-cells-per-row
    // aspect, and the gap row sits below it.
    let art_width = art_height * 2;
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
fn progress_bar(elapsed: Duration, duration: Duration, width: u16) -> String {
    let ratio = if duration.is_zero() {
        0.0
    } else {
        (elapsed.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
    };
    let filled = (ratio * width as f64).round() as usize;
    let mut bar = "━".repeat(filled);
    bar.push_str(&"─".repeat(width as usize - filled));
    bar
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
    /// The receiver dropped the event channel.
    Disconnected,
}

/// Restores the terminal however this future ends — returned, errored, or
/// **dropped**. Dropping matters: the caller runs this in a `select!` with
/// the receiver, so the whole loop can be cancelled at an await point, and a
/// missed restore leaves the user in raw mode with no cursor.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Run the full-screen display until the user quits or the receiver stops.
///
/// Owns the terminal for its lifetime: `ratatui::try_init` enables raw mode
/// and the alternate screen and installs a panic hook that restores them;
/// [`TerminalGuard`] covers every other way out.
pub async fn run(mut events: UnboundedReceiver<Event>, name: String) -> std::io::Result<Exit> {
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    event_loop(&mut terminal, &mut events, name).await
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    events: &mut UnboundedReceiver<Event>,
    name: String,
) -> std::io::Result<Exit> {
    let mut state = NowPlaying::new(name);
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // Raw mode means the terminal sends no SIGINT, but `kill` and systemd
    // still send SIGTERM — catch it so the screen is handed back.
    let mut terminate = signal(SignalKind::terminate())?;

    loop {
        terminal.draw(|frame| {
            state.render(frame);
        })?;

        tokio::select! {
            // The receiver's events: the display state.
            event = events.recv() => match event {
                Some(event) => state.apply(event),
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
    use std::net::Ipv4Addr;

    fn draw(state: &NowPlaying, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                state.render(frame);
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

    fn playing() -> NowPlaying {
        let mut state = NowPlaying::new("Living Room".into());
        state.apply(Event::SessionStarted {
            rate: 44100,
            channels: 2,
            peer: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
        });
        state.apply(Event::Metadata {
            title: Some("Sonata No. 1".into()),
            artist: Some("Some Artist".into()),
            album: Some("Some Album".into()),
        });
        state.apply(Event::Volume { db: -12.5 });
        state
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
        let state = NowPlaying::new("Living Room".into());
        let screen = draw(&state, 40, 10);
        assert!(screen.contains("Living Room"), "{screen}");
        assert!(screen.contains("waiting for a sender"), "{screen}");
        assert_centered(&screen, 40);
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
        state.apply(Event::Progress {
            elapsed: Duration::from_secs(83),
            duration: Duration::from_secs(247),
        });
        let screen = draw(&state, 60, 20);
        assert!(screen.contains("1:23 / 4:07"), "{screen}");
        assert!(screen.contains('━') && screen.contains('─'), "{screen}");
    }

    #[test]
    fn a_stream_with_no_known_end_shows_no_clock() {
        let mut state = playing();
        state.apply(Event::Progress {
            elapsed: Duration::from_secs(10),
            duration: Duration::ZERO,
        });
        let screen = draw(&state, 60, 20);
        assert!(!screen.contains('/'), "{screen}");
    }

    #[test]
    fn missing_metadata_fields_are_skipped_not_blanked() {
        let mut state = playing();
        state.apply(Event::Metadata {
            title: Some("Just A Title".into()),
            artist: None,
            album: None,
        });
        let screen = draw(&state, 40, 12);
        assert!(screen.contains("Just A Title"), "{screen}");
        assert!(!screen.contains("Some Artist"), "{screen}");
    }

    #[test]
    fn session_end_returns_to_idle() {
        let mut state = playing();
        state.apply(Event::SessionEnded);
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

    #[test]
    fn artwork_gets_a_box_above_the_text() {
        let area = Rect::new(0, 0, 60, 24);
        let (art, text) = layout(area, 6, true);
        assert!(art.height > 0 && art.width == art.height * 2);
        assert!(art.y + art.height <= text.y, "artwork must sit above text");
        let (none, _) = layout(area, 6, false);
        assert_eq!(none.height, 0, "no artwork, no box");
    }

    #[test]
    fn artwork_box_yields_to_a_short_screen() {
        // Eight rows of text on a ten-row screen leaves no room for art.
        let (art, _) = layout(Rect::new(0, 0, 60, 10), 8, true);
        assert_eq!(art.height, 0);
    }

    #[test]
    fn empty_artwork_clears_it() {
        let mut state = playing();
        state.apply(Event::Artwork {
            content_type: "image/jpeg".into(),
            data: vec![1, 2, 3],
        });
        assert!(state.artwork.is_some());
        state.apply(Event::Artwork {
            content_type: "image/none".into(),
            data: Vec::new(),
        });
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
        assert_eq!(
            progress_bar(Duration::ZERO, Duration::from_secs(10), 10),
            "──────────"
        );
        assert_eq!(
            progress_bar(Duration::from_secs(5), Duration::from_secs(10), 10),
            "━━━━━─────"
        );
        assert_eq!(
            progress_bar(Duration::from_secs(10), Duration::from_secs(10), 10),
            "━━━━━━━━━━"
        );
        // A position past the end (a seek report we haven't caught up with)
        // must not overflow the bar.
        assert_eq!(
            progress_bar(Duration::from_secs(99), Duration::from_secs(10), 10),
            "━━━━━━━━━━"
        );
    }
}
