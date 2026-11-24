//! Java 26.3 `ClientClockManager.tick` arithmetic for each retained world clock.
use horizon_protocol::packets::play::ClockState;

pub(crate) fn advance_clock(clock: &mut ClockState, delta: i64) {
    let partial = f64::from(clock.partial_tick) + delta as f64 * f64::from(clock.rate);
    // Mth.floor(double) is Math.floor followed by a Java double-to-int cast.
    let full = partial.floor() as i32;
    clock.partial_tick = (partial - f64::from(full)) as f32;
    clock.total_ticks = clock.total_ticks.wrapping_add(i64::from(full));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(total_ticks: i64, partial_tick: f32, rate: f32) -> ClockState {
        ClockState {
            clock: 0,
            total_ticks,
            partial_tick,
            rate,
        }
    }

    #[test]
    fn arithmetic_matches_java_26_3_clock_manager_samples() {
        // Captured from the actual ClientClockManager.tick in the 26.3 client.
        for (total, partial, rate, delta, expected_total, expected_fraction_bits) in [
            (100, 0.75, 0.5, 1, 101, 1_048_576_000),
            (101, 0.25, -0.5, 1, 100, 1_061_158_912),
            (i64::MAX, 0.0, 1.0, 1, i64::MIN, 0),
            (i64::MIN, 0.0, 1.0, -1, i64::MAX, 0),
            (0, 0.0, 1.0, i64::MAX, 2_147_483_647, 1_593_835_520),
            (0, 0.0, 1.0, i64::MIN, -2_147_483_648, 3_741_319_168),
            (123, 0.25, 0.0, i64::MAX, 123, 1_048_576_000),
        ] {
            let mut value = clock(total, partial, rate);
            advance_clock(&mut value, delta);
            assert_eq!(
                (value.total_ticks, value.partial_tick.to_bits()),
                (expected_total, expected_fraction_bits)
            );
        }
    }

    #[test]
    fn fractions_carry_and_negative_rates_floor_as_java_does() {
        let mut value = clock(100, 0.75, 0.5);
        advance_clock(&mut value, 1);
        assert_eq!((value.total_ticks, value.partial_tick), (101, 0.25));
        value.rate = -0.5;
        advance_clock(&mut value, 1);
        assert_eq!((value.total_ticks, value.partial_tick), (100, 0.75));
    }

    #[test]
    fn long_totals_wrap_in_both_directions() {
        let mut value = clock(i64::MAX, 0.0, 1.0);
        advance_clock(&mut value, 1);
        assert_eq!((value.total_ticks, value.partial_tick), (i64::MIN, 0.0));
        advance_clock(&mut value, -1);
        assert_eq!((value.total_ticks, value.partial_tick), (i64::MAX, 0.0));
    }

    #[test]
    fn large_steps_use_the_int_floor_before_adding_to_the_long() {
        let mut value = clock(0, 0.0, 1.0);
        advance_clock(&mut value, i64::MAX);
        assert_eq!(value.total_ticks, i64::from(i32::MAX));
        assert_eq!(
            value.partial_tick,
            (i64::MAX as f64 - f64::from(i32::MAX)) as f32
        );
    }
}
