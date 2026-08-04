//! The playback thread: ALAC decode, prebuffer, and the latency-correct
//! start, feeding the host's [`AudioSink`].
//!
//! A dedicated OS thread owns the ALAC decoder; the async audio receiver
//! hands it decrypted ALAC frames (tagged with their RTP timestamp) over a
//! channel. Once the clock model is ready the first chunk is held until its
//! `play_time`, giving a latency-correct start; otherwise a fixed prebuffer
//! is used. Blocking [`AudioSink::write`] calls pace playback afterwards.
//!
//! This is the library half of the sink seam: everything here is protocol
//! semantics (the start instant comes from the NTP clock model). The device
//! itself — output, pacing against the hardware, drift, gain — lives in the
//! host's sink.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{debug, info, warn};

use crate::clock::{self, ClockModel};
use crate::decode::AlacDecoder;
use crate::sdp::AlacConfig;
use crate::sink::AudioSink;

enum Command {
    Frame {
        ts: u32,
        packet: Vec<u8>,
    },
    /// A lost packet at this RTP timestamp: emit silence to keep timing.
    Silence {
        ts: u32,
    },
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
    /// Queue a decrypted ALAC frame (with its RTP timestamp) for playback.
    pub fn frame(&self, ts: u32, packet: Vec<u8>) {
        let _ = self.tx.send(Command::Frame { ts, packet });
    }

    /// Signal a lost packet so the player conceals it with silence.
    pub fn silence(&self, ts: u32) {
        let _ = self.tx.send(Command::Silence { ts });
    }

    /// Drop buffered audio and re-arm the prebuffer (used on FLUSH).
    pub fn flush(&self) {
        let _ = self.tx.send(Command::Flush);
    }
}

/// Owns the playback thread. Dropping it stops the thread and drops the sink.
/// Held by the session for lifecycle and FLUSH.
pub struct Player {
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Player {
    /// Spawn the playback thread feeding `sink`. Never fails: a sink is
    /// always usable (the host decides what `write` does, including
    /// discarding audio when it has no device).
    pub fn spawn(
        config: &AlacConfig,
        sink: Box<dyn AudioSink>,
        clock: Arc<Mutex<ClockModel>>,
    ) -> Player {
        let config = *config;
        let prebuffer_packets = prebuffer_packets(&config);
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("audio-player".into())
            .spawn(move || run(config, prebuffer_packets, rx, thread_stop, clock, sink))
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
    prebuffer_packets: usize,
    rx: Receiver<Command>,
    stop: Arc<AtomicBool>,
    clock: Arc<Mutex<ClockModel>>,
    mut sink: Box<dyn AudioSink>,
) {
    let mut decoder = match AlacDecoder::new(&config) {
        Ok(d) => d,
        Err(e) => {
            warn!("player: cannot build ALAC decoder ({e}); audio disabled");
            drain(rx);
            return;
        }
    };

    let channels = decoder.channels();
    info!(
        "player: {} Hz {}ch, prebuffer {prebuffer_packets} packets",
        config.sample_rate, channels
    );
    let silence_frame = vec![0i16; config.frames_per_packet as usize * channels];
    let mut playout = Playout::new(prebuffer_packets, clock);
    let mut decoded_packets: u64 = 0;
    while let Ok(command) = rx.recv() {
        // Preempt a queued backlog on teardown rather than draining it in
        // real time.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match command {
            Command::Frame { ts, packet } => {
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
                playout.feed(ts, pcm, sink.as_mut());
            }
            Command::Silence { ts } => {
                playout.feed(ts, &silence_frame, sink.as_mut());
            }
            Command::Flush => {
                playout.reset();
                sink.flush();
            }
            Command::Stop => break,
        }
    }
    debug!("player: stopped, {decoded_packets} packets played");
}

fn drain(rx: Receiver<Command>) {
    while rx.recv().is_ok() {}
}

/// Drives the transition from prebuffering to steady playback: a
/// latency-correct start off the clock model (falling back to a fixed
/// prebuffer).
struct Playout {
    prebuffer: Prebuffer,
    clock: Arc<Mutex<ClockModel>>,
    started: bool,
    first_ts: Option<u32>,
}

impl Playout {
    fn new(prebuffer_packets: usize, clock: Arc<Mutex<ClockModel>>) -> Playout {
        Playout {
            prebuffer: Prebuffer::new(prebuffer_packets),
            clock,
            started: false,
            first_ts: None,
        }
    }

    fn feed(&mut self, ts: u32, pcm: &[i16], sink: &mut dyn AudioSink) {
        if !self.started {
            self.first_ts.get_or_insert(ts);
        }
        let Some(chunk) = self.prebuffer.push(pcm) else {
            return;
        };
        let starting = !self.started;
        if starting {
            self.started = true;
            self.await_start_instant();
        }
        sink.write(&chunk);
    }

    /// Hold the first chunk until its frame's `play_time`, so audio starts at
    /// the instant the client intends. No-op until the clock model is ready.
    fn await_start_instant(&self) {
        let Some(first_ts) = self.first_ts else {
            return;
        };
        let target = self.clock.lock().unwrap().play_time(first_ts);
        let Some(target) = target else {
            info!("player: start via prebuffer (no clock sync yet)");
            return;
        };
        let now = clock::now_ns();
        if target > now {
            let wait = (target - now).min(2_000_000_000); // cap the wait at 2 s
            info!(
                "player: latency-correct start, waiting {} ms",
                wait / 1_000_000
            );
            std::thread::sleep(Duration::from_nanos(wait));
        } else {
            info!("player: latency-correct start (already due, no wait)");
        }
    }

    fn reset(&mut self) {
        self.prebuffer.reset();
        self.started = false;
        self.first_ts = None;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Records every delivered chunk and every flush.
    #[derive(Clone, Default)]
    struct Recorder {
        writes: Arc<Mutex<Vec<Vec<i16>>>>,
        flushes: Arc<AtomicUsize>,
    }

    impl AudioSink for Recorder {
        fn write(&mut self, pcm: &[i16]) {
            self.writes.lock().unwrap().push(pcm.to_vec());
        }
        fn flush(&mut self) {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn golden_config() -> AlacConfig {
        let fmtp = include_str!("../tests/data/golden_fmtp.txt").trim();
        AlacConfig::parse(&format!("96 {fmtp}")).unwrap()
    }

    fn golden_packet() -> Vec<u8> {
        include_bytes!("../tests/data/golden_packet.bin").to_vec()
    }

    fn golden_pcm() -> Vec<i16> {
        include_bytes!("../tests/data/golden_pcm_i16le.bin")
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// Wait (bounded) until `predicate` holds.
    fn settle(mut predicate: impl FnMut() -> bool) {
        for _ in 0..400 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("player did not settle");
    }

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

    #[test]
    fn delivers_decoded_golden_pcm_to_the_sink() {
        let config = golden_config();
        let prebuffer = prebuffer_packets(&config);
        let packet = golden_packet();
        let expected = golden_pcm();
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let player = Player::spawn(&config, Box::new(recorder), clock);
        let sender = player.sender();

        for i in 0..prebuffer {
            sender.frame(i as u32 * config.frames_per_packet, packet.clone());
        }
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "the prebuffer releases as one chunk");
        assert_eq!(writes[0].len(), prebuffer * expected.len());
        assert_eq!(writes[0][..expected.len()], expected[..]);
    }

    #[test]
    fn flush_drops_prebuffered_audio_and_flushes_the_sink() {
        let config = golden_config();
        let prebuffer = prebuffer_packets(&config);
        let packet = golden_packet();
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let flushes = recorder.flushes.clone();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let player = Player::spawn(&config, Box::new(recorder), clock);
        let sender = player.sender();

        // Below the prebuffer threshold: nothing can have played yet.
        for i in 0..prebuffer - 1 {
            sender.frame(i as u32 * config.frames_per_packet, packet.clone());
        }
        sender.flush();
        settle(|| flushes.load(Ordering::Relaxed) == 1);

        // After the flush the prebuffer is re-armed: a full threshold of new
        // audio plays, and none of the pre-flush audio.
        for i in 0..prebuffer {
            sender.frame((1000 + i as u32) * config.frames_per_packet, packet.clone());
        }
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].len(),
            prebuffer * golden_pcm().len(),
            "only post-flush audio reaches the sink"
        );
    }

    #[test]
    fn lost_packets_are_concealed_with_silence() {
        let config = golden_config();
        let prebuffer = prebuffer_packets(&config);
        let packet = golden_packet();
        let pcm_len = golden_pcm().len(); // frames_per_packet * channels
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let player = Player::spawn(&config, Box::new(recorder), clock);
        let sender = player.sender();

        for i in 0..prebuffer - 1 {
            sender.frame(i as u32 * config.frames_per_packet, packet.clone());
        }
        sender.silence((prebuffer - 1) as u32 * config.frames_per_packet);
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        let chunk = &writes[0];
        assert_eq!(chunk.len(), prebuffer * pcm_len);
        assert!(
            chunk[chunk.len() - pcm_len..].iter().all(|&s| s == 0),
            "the lost packet arrives as a silent frame"
        );
    }
}
