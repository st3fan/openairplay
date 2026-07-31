//! ALSA audio output.
//!
//! A dedicated OS thread owns the ALAC decoder and the blocking ALSA PCM
//! handle; the async audio receiver hands it decrypted ALAC frames over a
//! channel. Blocking `writei` calls pace playback to real time. This is the
//! naive milestone-3 player: prebuffer a little, then stream in arrival
//! order. No sequence reordering, retransmits, or clock sync yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use log::{debug, info, warn};

use crate::decode::AlacDecoder;
use crate::sdp::AlacConfig;

enum Command {
    Frame(Vec<u8>),
    Flush,
    Stop,
}

/// A cloneable handle for feeding ALAC frames to the playback thread. Held by
/// the audio-receiver task; sending is a no-op once the [`Player`] is dropped.
#[derive(Clone)]
pub struct PlayerSender {
    tx: Sender<Command>,
}

impl PlayerSender {
    /// Queue a decrypted ALAC frame for playback.
    pub fn frame(&self, packet: Vec<u8>) {
        let _ = self.tx.send(Command::Frame(packet));
    }
}

/// Owns the playback thread. Dropping it stops the thread and closes the
/// audio device. Held by the session for lifecycle and FLUSH.
pub struct Player {
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Player {
    /// Spawn the playback thread. `device` is an ALSA device name
    /// (e.g. `default`); `None` runs decode-only with no audio device (for
    /// `--no-audio` and for hosts without working audio). Never fails: if the
    /// device can't be opened the thread logs and discards audio so the RTSP
    /// session keeps running.
    pub fn spawn(config: &AlacConfig, device: Option<String>) -> Player {
        let config = *config;
        let prebuffer_packets = prebuffer_packets(&config);
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("alsa-player".into())
            .spawn(move || run(config, device, prebuffer_packets, rx, thread_stop))
            .expect("spawn player thread");
        Player {
            tx: Some(tx),
            handle: Some(handle),
            stop,
        }
    }

    /// A cloneable sender for the audio-receiver task to push frames.
    pub fn sender(&self) -> PlayerSender {
        PlayerSender {
            tx: self
                .tx
                .clone()
                .expect("player sender available before drop"),
        }
    }

    /// Drop buffered audio and re-arm the prebuffer (used on FLUSH).
    pub fn flush(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Flush);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Set the flag so a queued backlog is skipped, and send an explicit
        // Stop so the thread wakes even if it's idle in recv() — dropping our
        // sender alone wouldn't close the channel while the audio task still
        // holds a cloned PlayerSender.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Stop);
        }
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Target ~200 ms of prebuffer before the first write, expressed in packets.
fn prebuffer_packets(config: &AlacConfig) -> usize {
    let frames = config.frames_per_packet.max(1);
    let target = config.sample_rate / 5; // 200 ms
    (target / frames).max(1) as usize
}

fn run(
    config: AlacConfig,
    device: Option<String>,
    prebuffer_packets: usize,
    rx: Receiver<Command>,
    stop: Arc<AtomicBool>,
) {
    let mut decoder = match AlacDecoder::new(&config) {
        Ok(d) => d,
        Err(e) => {
            warn!("player: cannot build ALAC decoder ({e}); audio disabled");
            drain(rx);
            return;
        }
    };

    let mut output = match device {
        Some(name) => match AlsaOutput::open(&name, config.sample_rate, decoder.channels()) {
            Ok(out) => {
                info!(
                    "player: ALSA \"{name}\" {} Hz {}ch, prebuffer {prebuffer_packets} packets",
                    config.sample_rate,
                    decoder.channels()
                );
                Some(out)
            }
            Err(e) => {
                warn!("player: cannot open ALSA \"{name}\" ({e}); decode-only");
                None
            }
        },
        None => {
            info!("player: --no-audio, decode-only");
            None
        }
    };

    let mut prebuffer = Prebuffer::new(prebuffer_packets);
    let mut decoded_packets: u64 = 0;
    while let Ok(command) = rx.recv() {
        // Preempt a queued backlog on teardown rather than draining it in
        // real time.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match command {
            Command::Frame(packet) => {
                let pcm = match decoder.decode(&packet) {
                    Ok(pcm) => pcm,
                    Err(e) => {
                        debug!("player: dropping undecodable packet: {e}");
                        continue;
                    }
                };
                decoded_packets += 1;
                if decoded_packets <= 3 || decoded_packets.is_multiple_of(500) {
                    debug!("player: {decoded_packets} packets decoded");
                }
                if let Some(chunk) = prebuffer.push(pcm) {
                    if let Some(out) = output.as_mut() {
                        out.write(&chunk);
                    }
                }
            }
            Command::Flush => {
                prebuffer.reset();
                if let Some(out) = output.as_mut() {
                    out.reset();
                }
            }
            Command::Stop => break,
        }
    }
    debug!("player: stopped, {decoded_packets} packets played");
}

fn drain(rx: Receiver<Command>) {
    while rx.recv().is_ok() {}
}

/// Accumulates decoded PCM until a packet threshold is reached, then releases
/// it and passes subsequent packets straight through. `reset` re-arms it.
struct Prebuffer {
    threshold_packets: usize,
    packets: usize,
    buffer: Vec<i16>,
    released: bool,
}

impl Prebuffer {
    fn new(threshold_packets: usize) -> Prebuffer {
        Prebuffer {
            threshold_packets,
            packets: 0,
            buffer: Vec::new(),
            released: false,
        }
    }

    /// Feed decoded PCM. Returns the samples to write now, or `None` while
    /// still prebuffering.
    fn push(&mut self, pcm: &[i16]) -> Option<Vec<i16>> {
        if self.released {
            return Some(pcm.to_vec());
        }
        self.buffer.extend_from_slice(pcm);
        self.packets += 1;
        if self.packets >= self.threshold_packets {
            self.released = true;
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.packets = 0;
        self.buffer.clear();
        self.released = false;
    }
}

struct AlsaOutput {
    pcm: PCM,
    channels: usize,
}

impl AlsaOutput {
    fn open(device: &str, rate: u32, channels: usize) -> Result<AlsaOutput, alsa::Error> {
        let pcm = PCM::new(device, Direction::Playback, false)?;
        {
            let hwp = HwParams::any(&pcm)?;
            hwp.set_channels(channels as u32)?;
            hwp.set_rate(rate, ValueOr::Nearest)?;
            hwp.set_format(Format::s16())?;
            hwp.set_access(Access::RWInterleaved)?;
            // ~500 ms device buffer to absorb scheduling jitter; best-effort.
            let _ = hwp.set_buffer_time_near(500_000, ValueOr::Nearest);
            pcm.hw_params(&hwp)?;
        }
        pcm.prepare()?;
        Ok(AlsaOutput { pcm, channels })
    }

    /// Write all interleaved samples, blocking to pace playback and
    /// recovering from underruns.
    fn write(&mut self, samples: &[i16]) {
        let io = match self.pcm.io_i16() {
            Ok(io) => io,
            Err(e) => {
                warn!("player: ALSA io handle lost: {e}");
                return;
            }
        };
        let mut frames = samples;
        while !frames.is_empty() {
            match io.writei(frames) {
                Ok(0) => break,
                Ok(written) => frames = &frames[written * self.channels..],
                Err(e) => {
                    if self.pcm.try_recover(e, true).is_err() {
                        warn!("player: unrecoverable ALSA write error, dropping chunk");
                        return;
                    }
                }
            }
        }
    }

    /// Reset the device after a flush so the next write starts cleanly.
    fn reset(&mut self) {
        if self.pcm.state() == State::Running {
            let _ = self.pcm.drop();
        }
        let _ = self.pcm.prepare();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuffer_holds_until_threshold_then_releases() {
        let mut pb = Prebuffer::new(3);
        assert_eq!(pb.push(&[1, 2]), None); // packet 1
        assert_eq!(pb.push(&[3, 4]), None); // packet 2
                                            // Packet 3 hits the threshold and releases the whole accumulation.
        assert_eq!(pb.push(&[5, 6]), Some(vec![1, 2, 3, 4, 5, 6]));
        // Subsequent packets pass straight through.
        assert_eq!(pb.push(&[7, 8]), Some(vec![7, 8]));
    }

    #[test]
    fn reset_rearms_prebuffer() {
        let mut pb = Prebuffer::new(2);
        assert_eq!(pb.push(&[1]), None);
        assert_eq!(pb.push(&[2]), Some(vec![1, 2]));
        pb.reset();
        assert_eq!(pb.push(&[9]), None, "after reset it prebuffers again");
        assert_eq!(pb.push(&[10]), Some(vec![9, 10]));
    }

    #[test]
    fn threshold_of_one_releases_immediately() {
        let mut pb = Prebuffer::new(1);
        assert_eq!(pb.push(&[42, 43]), Some(vec![42, 43]));
    }

    #[test]
    fn prebuffer_packet_count_from_config() {
        let config = AlacConfig::parse("96 352 0 16 40 10 14 2 255 0 0 44100").unwrap();
        // 200 ms = 8820 frames / 352 per packet = 25 packets.
        assert_eq!(prebuffer_packets(&config), 25);
    }
}
