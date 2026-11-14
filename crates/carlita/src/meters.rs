//! Meter bars as they are drawn: they jump up to a new peak at once and fall
//! back smoothly, one step per display frame. The engine reports levels a
//! little less often than the display refreshes, and without the fall the
//! bars moved in visible steps.

use std::time::Instant;

/// How fast a bar falls once the level drops, in dB per second.
const FALL_DB_PER_SEC: f32 = 30.0;
/// Below this (about -80 dBFS) a bar is drawn empty.
const FLOOR: f32 = 1e-4;

pub struct MeterBars<const N: usize> {
    target: [f32; N],
    shown: [f32; N],
    drawn: [f32; N],
    last: Instant,
}

impl<const N: usize> MeterBars<N> {
    pub fn new() -> Self {
        Self {
            target: [0.0; N],
            shown: [0.0; N],
            drawn: [0.0; N],
            last: Instant::now(),
        }
    }

    /// The latest levels from the engine (linear, 1.0 = full scale).
    pub fn set(&mut self, levels: [f32; N]) {
        for ((target, shown), level) in self.target.iter_mut().zip(&mut self.shown).zip(levels) {
            *target = level;
            if level > *shown {
                *shown = level;
            }
        }
    }

    /// Lets the bars fall for the time since the last step. Returns the
    /// levels to draw, or None when no bar moved (so nothing is redrawn).
    pub fn step(&mut self, now: Instant) -> Option<[f32; N]> {
        let dt = now
            .saturating_duration_since(self.last)
            .as_secs_f32()
            .min(0.25);
        self.last = now;
        let fall = 10f32.powf(-FALL_DB_PER_SEC * dt / 20.0);
        for i in 0..N {
            let mut v = (self.shown[i] * fall).max(self.target[i]);
            if v < FLOOR {
                v = 0.0;
            }
            self.shown[i] = v;
        }
        if self.shown == self.drawn {
            return None;
        }
        self.drawn = self.shown;
        Some(self.shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rises_at_once_and_falls_smoothly() {
        let t0 = Instant::now();
        let mut m = MeterBars::<1>::new();
        m.last = t0;
        m.set([0.5]);
        assert_eq!(m.step(t0), Some([0.5]));
        m.set([0.0]);
        let a = m.step(t0 + Duration::from_millis(100)).unwrap()[0];
        // 30 dB/s for 0.1 s is 3 dB down.
        assert!((a - 0.5 * 10f32.powf(-0.15)).abs() < 1e-4, "{a}");
        let b = m.step(t0 + Duration::from_millis(200)).unwrap()[0];
        assert!(b < a);
        // A new peak shows at once.
        m.set([0.9]);
        assert_eq!(m.step(t0 + Duration::from_millis(210)), Some([0.9]));
    }

    #[test]
    fn nothing_to_draw_when_silent() {
        let t0 = Instant::now();
        let mut m = MeterBars::<2>::new();
        m.last = t0;
        m.set([0.0, 0.0]);
        assert_eq!(m.step(t0 + Duration::from_millis(8)), None);
        m.set([0.2, 0.0]);
        assert!(m.step(t0 + Duration::from_millis(16)).is_some());
        m.set([0.0, 0.0]);
        // Falls to nothing within a few seconds, then stops redrawing.
        let mut t = t0 + Duration::from_millis(16);
        let mut last = Some([1.0, 1.0]);
        for _ in 0..1000 {
            t += Duration::from_millis(8);
            last = m.step(t);
            if last.is_none() {
                break;
            }
        }
        assert_eq!(last, None);
        assert_eq!(m.shown, [0.0, 0.0]);
    }
}
