//! Software volume.
//!
//! AirPlay sends volume as a dB attenuation via `SET_PARAMETER`: 0 dB is full
//! volume, the useful range is about [-30, 0], and the special value -144
//! means mute. We convert that to a linear gain and scale the PCM before it
//! reaches ALSA.

/// The `SET_PARAMETER` mute sentinel.
const MUTE_DB: f32 = -144.0;

/// Convert an AirPlay dB attenuation to a linear gain in `[0.0, 1.0]`.
pub fn db_to_gain(db: f32) -> f32 {
    if db <= MUTE_DB {
        return 0.0;
    }
    let gain = 10f32.powf(db / 20.0);
    gain.clamp(0.0, 1.0)
}

/// Scale interleaved `i16` PCM by `gain` in place, rounding and clamping.
/// `gain == 1.0` is a no-op fast path.
pub fn apply_gain(samples: &mut [i16], gain: f32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_volume_is_unity() {
        assert_eq!(db_to_gain(0.0), 1.0);
    }

    #[test]
    fn mute_is_zero() {
        assert_eq!(db_to_gain(-144.0), 0.0);
        assert_eq!(db_to_gain(-200.0), 0.0);
    }

    #[test]
    fn minus_six_db_is_about_half() {
        let g = db_to_gain(-6.0206);
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
}
