//! The receiver binary's [`AudioSink`]: ALSA output with drift correction
//! and live volume.
//!
//! This is the host side of the sink seam — the library hands it PCM that
//! should play (prebuffered and start-timed), and it owns the device, the
//! pacing (blocking `writei`), drift correction against the device queue,
//! and the gain. The AirPlay volume arrives as an `openairplay::Event::Volume`
//! in dB; the binary maps it to a linear gain shared with the sink.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use log::{debug, info, warn};

use crate::sink::AudioSink;

/// The `SET_PARAMETER` mute sentinel.
const MUTE_DB: f32 = -144.0;

/// Convert an AirPlay dB attenuation to a linear gain in `[0.0, 1.0]`.
pub fn volume_to_gain(db: f32) -> f32 {
    if db <= MUTE_DB {
        return 0.0;
    }
    let gain = 10f32.powf(db / 20.0);
    gain.clamp(0.0, 1.0)
}

/// Scale interleaved `i16` PCM by `gain` in place, rounding and clamping.
/// `gain == 1.0` is a no-op fast path.
fn apply_gain(samples: &mut [i16], gain: f32) {
    if gain >= 1.0 {
        return;
    }
    if gain <= 0.0 {
        samples.fill(0);
        return;
    }
    for s in samples.iter_mut() {
        let scaled = (*s as f32 * gain).round();
        *s = scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

/// The playback gain, shared between the event consumer (which sets it from
/// volume events) and the sink (which applies it live). Outlives any single
/// stream, so a volume set before `SETUP` isn't lost.
#[derive(Clone)]
pub struct SharedGain(Arc<AtomicU32>);

impl SharedGain {
    /// Starts at full volume.
    pub fn new() -> SharedGain {
        SharedGain(Arc::new(AtomicU32::new(1.0f32.to_bits())))
    }

    /// Set the linear gain (`1.0` = full, `0.0` = mute).
    pub fn set(&self, gain: f32) {
        self.0
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Default for SharedGain {
    fn default() -> SharedGain {
        SharedGain::new()
    }
}

/// Discards all audio — used for `--no-audio` (decode-only). Never blocks,
/// so the pipeline runs unpaced, same as before the sink seam existed.
pub struct NullSink;

impl AudioSink for NullSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

/// ALSA playback sink: blocking `writei` pacing, frame-stuffing drift
/// correction against the device queue, and live gain. A device that won't
/// open is logged and audio is discarded so the session keeps running.
pub struct AlsaSink {
    output: Option<AlsaOutput>,
    gain: SharedGain,
    /// Gain-scaled, drift-adjusted copy of the incoming chunk.
    scratch: Vec<i16>,
    channels: usize,
    /// Steer the device queue toward the prebuffered depth (~200 ms).
    target_depth: i64,
    /// Only correct once the queue strays by more than ~10 ms.
    drift_threshold: i64,
    since_drift_check: u32,
    /// False until the first write after open/flush (that chunk is the
    /// library's released prebuffer; drift correction starts after it).
    started: bool,
}

impl AlsaSink {
    pub fn open(device: &str, rate: u32, channels: u8, gain: SharedGain) -> AlsaSink {
        let output = match AlsaOutput::open(device, rate, channels as usize) {
            Ok(out) => {
                info!("player: ALSA \"{device}\" {rate} Hz {channels}ch");
                Some(out)
            }
            Err(e) => {
                warn!("player: cannot open ALSA \"{device}\" ({e}); decode-only");
                None
            }
        };
        AlsaSink {
            output,
            gain,
            scratch: Vec::new(),
            channels: channels as usize,
            target_depth: (rate / 5) as i64, // 200 ms of frames
            drift_threshold: (rate / 100) as i64,
            since_drift_check: 0,
            started: false,
        }
    }
}

impl AudioSink for AlsaSink {
    fn write(&mut self, pcm: &[i16]) {
        if self.output.is_none() {
            return; // device failed to open → discard
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(pcm);
        // Apply the current volume (live) just before playback.
        apply_gain(&mut self.scratch, self.gain.get());
        let starting = !self.started;
        self.started = true;
        if !starting {
            self.drift_correct();
        }
        if let Some(out) = self.output.as_mut() {
            out.write(&self.scratch);
        }
    }

    fn flush(&mut self) {
        self.started = false;
        self.since_drift_check = 0;
        if let Some(out) = self.output.as_mut() {
            out.reset(); // discard queued frames → immediate silence
        }
    }
}

impl AlsaSink {
    /// Nudge the chunk by ±1 frame to steer the ALSA queue depth back toward
    /// the target, countering source/DAC clock drift. Checked periodically.
    fn drift_correct(&mut self) {
        self.since_drift_check += 1;
        if self.since_drift_check < 50 {
            return;
        }
        self.since_drift_check = 0;
        let Some(depth) = self.output.as_ref().and_then(|out| out.delay()) else {
            return;
        };
        match drift_action(depth, self.target_depth, self.drift_threshold) {
            1 if self.scratch.len() >= self.channels => {
                // Queue too shallow: duplicate the last frame to add depth.
                let tail = self.scratch[self.scratch.len() - self.channels..].to_vec();
                self.scratch.extend_from_slice(&tail);
                debug!("player: drift: inserted a frame");
            }
            -1 if self.scratch.len() >= self.channels => {
                // Queue too deep: drop one frame.
                self.scratch.truncate(self.scratch.len() - self.channels);
                debug!("player: drift: dropped a frame");
            }
            _ => {}
        }
    }
}

/// Drift correction decision from the current ALSA queue depth: `+1` insert a
/// frame (too shallow, risking underrun), `-1` drop a frame (too deep), `0`
/// hold.
fn drift_action(depth: i64, target: i64, threshold: i64) -> i32 {
    if depth < target - threshold {
        1
    } else if depth > target + threshold {
        -1
    } else {
        0
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

    /// Frames currently queued in the device (how far ahead of the DAC we
    /// are), for drift correction. `None` if ALSA can't report it.
    fn delay(&self) -> Option<i64> {
        self.pcm.delay().ok()
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
    fn full_volume_is_unity() {
        assert_eq!(volume_to_gain(0.0), 1.0);
    }

    #[test]
    fn mute_is_zero() {
        assert_eq!(volume_to_gain(-144.0), 0.0);
        assert_eq!(volume_to_gain(-200.0), 0.0);
    }

    #[test]
    fn minus_six_db_is_about_half() {
        let g = volume_to_gain(-6.0206);
        assert!((g - 0.5).abs() < 0.001, "got {g}");
    }

    #[test]
    fn unity_gain_leaves_samples_untouched() {
        let mut s = [100i16, -200, 30000];
        apply_gain(&mut s, 1.0);
        assert_eq!(s, [100, -200, 30000]);
    }

    #[test]
    fn zero_gain_silences() {
        let mut s = [100i16, -200, 30000];
        apply_gain(&mut s, 0.0);
        assert_eq!(s, [0, 0, 0]);
    }

    #[test]
    fn half_gain_scales_and_rounds() {
        let mut s = [100i16, -100, 32767];
        apply_gain(&mut s, 0.5);
        assert_eq!(s, [50, -50, 16384]); // 16383.5 rounds to 16384
    }

    #[test]
    fn scaling_the_extremes_stays_in_range() {
        // Full-scale samples scaled by a near-unity gain: no overflow, and the
        // values are attenuated as expected.
        let mut s = [i16::MIN, i16::MAX];
        apply_gain(&mut s, 0.999);
        assert_eq!(s, [-32735, 32734]);
    }

    #[test]
    fn shared_gain_clamps_and_round_trips() {
        let gain = SharedGain::new();
        assert_eq!(gain.get(), 1.0);
        gain.set(volume_to_gain(-20.0));
        assert!((gain.get() - 0.1).abs() < 1e-4);
        gain.set(2.0);
        assert_eq!(gain.get(), 1.0);
        gain.set(-1.0);
        assert_eq!(gain.get(), 0.0);
    }

    #[test]
    fn drift_action_steers_toward_target() {
        let target = 8820;
        let thr = 441; // 10 ms at 44100
        assert_eq!(drift_action(target, target, thr), 0, "on target: hold");
        assert_eq!(
            drift_action(target + 100, target, thr),
            0,
            "within band: hold"
        );
        assert_eq!(
            drift_action(target - 100, target, thr),
            0,
            "within band: hold"
        );
        assert_eq!(
            drift_action(target - 500, target, thr),
            1,
            "too shallow: insert"
        );
        assert_eq!(
            drift_action(target + 500, target, thr),
            -1,
            "too deep: drop"
        );
    }
}
