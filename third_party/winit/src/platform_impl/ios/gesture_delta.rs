//! Convert UIKit's cumulative gesture values to deltas.
use std::cell::Cell;

use crate::event::TouchPhase;

#[derive(Default)]
pub(super) struct GestureDelta<const N: usize> {
    samples: Cell<Option<([f64; N], [f64; N])>>,
}

impl<const N: usize> GestureDelta<N> {
    pub(super) fn reset(&self) {
        self.samples.set(None);
    }

    pub(super) fn update(&self, phase: TouchPhase, value: [f64; N]) -> Option<[f64; N]> {
        match phase {
            TouchPhase::Started => {
                self.samples.set(Some((value, value)));
                Some([0.0; N])
            }
            TouchPhase::Moved => {
                let (start, previous) = self.samples.get()?;
                self.samples.set(Some((start, value)));
                Some(std::array::from_fn(|i| value[i] - previous[i]))
            }
            TouchPhase::Ended => {
                let (_, previous) = self.samples.take()?;
                Some(std::array::from_fn(|i| value[i] - previous[i]))
            }
            TouchPhase::Cancelled => {
                let (start, previous) = self.samples.take()?;
                // Undo only the movement already delivered to the app.
                Some(std::array::from_fn(|i| start[i] - previous[i]))
            }
        }
    }
}

/// Pan samples come from UIKit in view points. Winit delivers physical pixels.
#[derive(Default)]
pub(super) struct PanDelta {
    motion: GestureDelta<2>,
    scale: Cell<f64>,
}

impl PanDelta {
    pub(super) fn reset(&self) {
        self.motion.reset();
    }

    pub(super) fn update(
        &self,
        phase: TouchPhase,
        translation: [f64; 2],
        display_scale: f64,
    ) -> Option<[f64; 2]> {
        if phase == TouchPhase::Started {
            // Keep the conversion stable until the gesture ends. A scale change
            // must not turn an unchanged translation into motion or change the
            // number of pixels undone on cancellation.
            self.scale.set(display_scale);
        }
        self.motion
            .update(phase, translation)
            .map(|delta| delta.map(|axis| axis * self.scale.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_reports_pixels_and_keeps_scale_through_cancellation() {
        let pan = PanDelta::default();
        assert_eq!(
            pan.update(TouchPhase::Started, [4.0, 8.0], 2.0),
            Some([0.0, 0.0])
        );
        assert_eq!(
            pan.update(TouchPhase::Moved, [5.5, 7.0], 2.0),
            Some([3.0, -2.0])
        );
        assert_eq!(
            pan.update(TouchPhase::Moved, [5.5, 7.0], 3.0),
            Some([0.0, 0.0])
        );
        assert_eq!(
            pan.update(TouchPhase::Cancelled, [7.0, 9.0], 3.0),
            Some([-3.0, 2.0])
        );
        pan.update(TouchPhase::Started, [0.0, 0.0], 3.0);
        assert_eq!(
            pan.update(TouchPhase::Ended, [1.0, 0.5], 3.0),
            Some([3.0, 1.5])
        );
    }

    #[test]
    fn cancelled_pinch_reverses_only_the_delivered_scale() {
        let pinch = GestureDelta::default();
        assert_eq!(pinch.update(TouchPhase::Started, [1.25]), Some([0.0]));
        assert_eq!(pinch.update(TouchPhase::Moved, [1.5]), Some([0.25]));
        assert_eq!(pinch.update(TouchPhase::Cancelled, [1.75]), Some([-0.25]));
        assert_eq!(pinch.update(TouchPhase::Cancelled, [1.75]), None);
    }

    #[test]
    fn ending_delivers_the_last_increment_and_clears_the_gesture() {
        let pan = GestureDelta::default();
        assert_eq!(
            pan.update(TouchPhase::Started, [4.0, 8.0]),
            Some([0.0, 0.0])
        );
        assert_eq!(pan.update(TouchPhase::Moved, [5.0, 6.0]), Some([1.0, -2.0]));
        assert_eq!(pan.update(TouchPhase::Ended, [7.0, 5.0]), Some([2.0, -1.0]));
        assert_eq!(pan.update(TouchPhase::Moved, [8.0, 6.0]), None);
    }

    #[test]
    fn cancelling_pan_does_not_include_motion_before_recognition() {
        let pan = GestureDelta::default();
        assert_eq!(pan.update(TouchPhase::Cancelled, [2.0, 4.0]), None);
        pan.update(TouchPhase::Started, [4.0, 8.0]);
        pan.update(TouchPhase::Moved, [6.0, 5.0]);
        assert_eq!(
            pan.update(TouchPhase::Cancelled, [7.0, 4.0]),
            Some([-2.0, 3.0])
        );
        pan.update(TouchPhase::Started, [1.0, 1.0]);
        pan.reset();
        assert_eq!(pan.update(TouchPhase::Ended, [4.0, 4.0]), None);
    }
}
