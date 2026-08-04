//! The dashboard must draw its first frame on its own, without any input.
//!
//! This is a regression test for a shipped bug: the terminal-capability probe
//! read the reply through `crossterm::event::poll`, which parses pending bytes
//! into crossterm's own buffer, so the follow-up read of the file descriptor
//! blocked until the *next* keystroke. On terminals that answer the query —
//! Ghostty, Kitty — the display showed nothing at all until a key was pressed.
//!
//! Nothing short of a real terminal reproduces that, so the test gives the
//! dashboard a pty and plays the part of the terminal: it answers the queries
//! the way a graphics-capable terminal does, sends **no input of its own**,
//! and checks that a screen arrives anyway.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for the dashboard to draw. Generous: it is bounded by the
/// probe's own 100 ms timeout, not by this.
const PATIENCE: Duration = Duration::from_secs(10);

/// A pty, from the terminal's side.
struct Terminal {
    master: OwnedFd,
    _slave: OwnedFd,
}

impl Terminal {
    fn open() -> Terminal {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 90,
            ws_xpixel: 900,
            ws_ypixel: 600,
        };
        // SAFETY: all four out-parameters are ours; the name argument is
        // optional and we don't want the name.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size,
            )
        };
        assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
        // SAFETY: openpty just handed us both descriptors.
        unsafe {
            Terminal {
                master: OwnedFd::from_raw_fd(master),
                _slave: OwnedFd::from_raw_fd(slave),
            }
        }
    }

    /// The dashboard's end of the pty, as three stdio handles.
    fn stdio(&self) -> [Stdio; 3] {
        std::array::from_fn(|_| {
            // SAFETY: dup'ing a descriptor we own; Stdio takes ownership of
            // the copy.
            let fd = unsafe { libc::dup(self._slave.as_raw_fd()) };
            assert!(fd >= 0, "dup failed: {}", std::io::Error::last_os_error());
            unsafe { Stdio::from_raw_fd(fd) }
        })
    }

    /// Read whatever is available within `timeout`, or nothing.
    fn read(&mut self, timeout: Duration) -> Vec<u8> {
        let mut fds = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, count matches.
        if unsafe { libc::poll(&mut fds, 1, timeout.as_millis() as libc::c_int) } <= 0 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; 64 * 1024];
        // SAFETY: reading into a buffer we own; the fd stays valid.
        let read = unsafe {
            libc::read(
                self.master.as_raw_fd(),
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        if read <= 0 {
            return Vec::new();
        }
        buffer.truncate(read as usize);
        buffer
    }

    fn write(&mut self, bytes: &[u8]) {
        // SAFETY: writing from a slice we own to a descriptor we own.
        unsafe {
            libc::write(
                self.master.as_raw_fd(),
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
            );
        }
    }
}

/// Run the dashboard against a pty and return everything it drew. `answer`
/// decides whether this terminal claims Kitty graphics support — the case
/// that used to hang is the one where it answers.
fn run_dashboard(answer: bool) -> String {
    let mut terminal = Terminal::open();
    let [stdin, stdout, stderr] = terminal.stdio();
    let mut child = Command::new(env!("CARGO_BIN_EXE_openairplay1-dashboard"))
        // A port nothing is listening on: the display should still come up
        // and say it is connecting.
        .args(["--connect", "ws://127.0.0.1:9"])
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("dashboard should start");

    let mut screen = String::new();
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        let chunk = terminal.read(Duration::from_millis(200));
        if chunk.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&chunk).into_owned();
        // Play the terminal: answer the graphics query and the device
        // attributes request — and nothing else. No keystrokes.
        if answer && text.contains("\x1b_G") && text.contains("a=q") {
            terminal.write(b"\x1b_Gi=7331;OK\x1b\\");
        }
        if text.contains("\x1b[c") {
            terminal.write(b"\x1b[?62;22c");
        }
        screen.push_str(&text);
        if screen.contains("connecting") {
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    screen
}

#[test]
fn draws_without_input_on_a_terminal_that_answers_queries() {
    let screen = run_dashboard(true);
    assert!(
        screen.contains("connecting"),
        "the display must draw before any key is pressed; got {} bytes: {screen:?}",
        screen.len()
    );
}

#[test]
fn draws_without_input_on_a_terminal_that_ignores_queries() {
    let screen = run_dashboard(false);
    assert!(
        screen.contains("connecting"),
        "a silent terminal must not stall the display; got {} bytes: {screen:?}",
        screen.len()
    );
}
