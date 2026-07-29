//! Monotonic clock with an explicit tick→nanosecond conversion.
//!
//! The hot path records whatever the platform's cheapest monotonic
//! counter returns -- `cntvct_el0` on aarch64, the TSC on x86_64, and
//! `clock_gettime` where neither is usable. Those ticks are not
//! nanoseconds, and the conversion happens here, at drain time.
//!
//! Counter and conversion both come from [`quanta`], deliberately. Every
//! timebase bug this crate has had came from pairing a counter with a
//! scale factor derived somewhere else; asking one component for both
//! makes that mismatch unrepresentable. quanta also calibrates against a
//! reference clock rather than trusting a declared frequency, and scales
//! with a multiply and a shift rather than a division.

use std::sync::Arc;

pub use quanta::Mock;

/// Monotonic clock anchored at a fixed start, converting raw counter
/// ticks to nanoseconds since that anchor.
#[derive(Clone, Debug)]
pub struct Clock {
    inner: quanta::Clock,
    start_ticks: u64,
}

impl Clock {
    /// Clock anchored at the current instant.
    ///
    /// The first call in the process calibrates the counter against a
    /// reference clock, which quanta bounds at 200ms. Calibration is
    /// global, so later clocks reuse it.
    pub fn starting_now() -> Self {
        let inner = quanta::Clock::new();
        let start_ticks = inner.raw();
        Self { inner, start_ticks }
    }

    /// Synthetic clock whose ticks are nanoseconds and only advance when
    /// the returned handle says so. Tests need timestamps that do not
    /// depend on the host's counter or on how long a test took to run.
    pub fn mock() -> (Self, Arc<Mock>) {
        let (inner, mock) = quanta::Clock::mock();
        Self::mock_from(inner, mock)
    }

    /// A mock clock anchored after `start_ticks` nanoseconds have passed,
    /// for testing that events before the anchor saturate rather than
    /// wrap.
    pub fn mock_starting_at(start_ticks: u64) -> (Self, Arc<Mock>) {
        let (inner, mock) = quanta::Clock::mock();
        mock.increment(start_ticks);
        Self::mock_from(inner, mock)
    }

    fn mock_from(inner: quanta::Clock, mock: Arc<Mock>) -> (Self, Arc<Mock>) {
        let start_ticks = inner.raw();
        (Self { inner, start_ticks }, mock)
    }

    /// Raw counter reading. Ticks, not nanoseconds.
    #[inline]
    pub fn raw(&self) -> u64 {
        self.inner.raw()
    }

    /// Nanoseconds elapsed between the clock's anchor and `ticks`.
    /// Saturates at zero for ticks recorded before the anchor.
    #[inline]
    pub fn ns_since_start(&self, ticks: u64) -> u64 {
        self.inner.delta_as_nanos(self.start_ticks, ticks)
    }

    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_reads_as_zero() {
        let (c, _m) = Clock::mock();
        assert_eq!(c.ns_since_start(c.start_ticks()), 0);
    }

    #[test]
    fn elapsed_ticks_convert_to_nanoseconds() {
        let (c, m) = Clock::mock();
        m.increment(1_500u64);
        assert_eq!(c.ns_since_start(c.raw()), 1_500);
    }

    #[test]
    fn ticks_before_the_anchor_saturate_instead_of_wrapping() {
        let (c, _m) = Clock::mock_starting_at(10_000);
        assert_eq!(c.start_ticks(), 10_000);
        assert_eq!(c.ns_since_start(9_000), 0);
        assert_eq!(c.ns_since_start(0), 0);
    }

    #[test]
    fn conversion_is_monotonic_across_a_wide_range() {
        let (c, _m) = Clock::mock();
        let mut last = 0;
        for ticks in [1u64, 1_000, 1 << 20, 1 << 32, 4_000_000_000_000_000] {
            let ns = c.ns_since_start(ticks);
            assert!(ns >= last, "went backwards at {ticks}");
            last = ns;
        }
    }

    #[test]
    fn the_host_clock_advances_in_step_with_wall_time() {
        let c = Clock::starting_now();
        let before = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let measured = c.ns_since_start(c.raw());
        let actual = before.elapsed().as_nanos() as u64;
        // Loose bounds: this asserts the timebase is applied at all, which
        // is what a 42x scaling error would violate.
        assert!(
            measured > actual / 2 && measured < actual * 2,
            "measured {measured}ns against {actual}ns of wall time"
        );
    }
}
