//! Monotonic clock with an explicit tick→nanosecond conversion.
//!
//! The hot path records whatever the platform's cheapest monotonic
//! counter returns -- `cntvct_el0` on aarch64, the TSC on x86_64, and
//! `clock_gettime` where neither is usable. Those ticks are not
//! nanoseconds, and the conversion happens here, at drain time.
//!
//! Whatever counter is read, its scale must come from the same place.
//! Every timebase bug this crate has had was a counter paired with a scale
//! factor derived somewhere else.
//!
//! On aarch64 that pairing is architectural: `CNTFRQ_EL0` states the rate
//! of `CNTVCT_EL0` by definition, so the frequency is known exactly and
//! there is nothing to measure. Everywhere else -- notably x86_64, where
//! the TSC rate is often not discoverable at all -- [`quanta`] measures
//! the counter against a reference clock, which costs up to 200ms once per
//! process.

use std::sync::Arc;

pub use quanta::Mock;

/// Where ticks come from, and what they are worth in nanoseconds.
#[derive(Clone, Debug)]
enum Source {
    /// A counter that describes its own rate. Nothing to calibrate.
    SelfDescribing { nanos_num: u64, nanos_den: u64 },
    /// A counter of unknown rate, measured against a reference clock.
    Measured(quanta::Clock),
}

/// Monotonic clock anchored at a fixed start, converting raw counter
/// ticks to nanoseconds since that anchor.
#[derive(Clone, Debug)]
pub struct Clock {
    source: Source,
    start_ticks: u64,
}

impl Clock {
    /// Clock anchored at the current instant.
    ///
    /// Free where the counter states its own frequency. Otherwise the
    /// first call in the process spends up to 200ms calibrating, once,
    /// globally.
    pub fn starting_now() -> Self {
        let source = match self_describing_timebase() {
            Some((nanos_num, nanos_den)) => Source::SelfDescribing {
                nanos_num,
                nanos_den,
            },
            None => Source::Measured(quanta::Clock::new()),
        };
        let mut clock = Self {
            source,
            start_ticks: 0,
        };
        clock.start_ticks = clock.raw();
        clock
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
        let source = Source::Measured(inner);
        (
            Self {
                source,
                start_ticks,
            },
            mock,
        )
    }

    /// Raw counter reading. Ticks, not nanoseconds.
    #[inline]
    pub fn raw(&self) -> u64 {
        match &self.source {
            Source::SelfDescribing { .. } => read_counter(),
            Source::Measured(clock) => clock.raw(),
        }
    }

    /// Nanoseconds elapsed between the clock's anchor and `ticks`.
    /// Saturates at zero for ticks recorded before the anchor.
    #[inline]
    pub fn ns_since_start(&self, ticks: u64) -> u64 {
        match &self.source {
            Source::SelfDescribing {
                nanos_num,
                nanos_den,
            } => {
                let delta = ticks.saturating_sub(self.start_ticks) as u128;
                (delta * *nanos_num as u128 / *nanos_den as u128) as u64
            }
            Source::Measured(clock) => clock.delta_as_nanos(self.start_ticks, ticks),
        }
    }

    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }
}

/// `(nanoseconds_numerator, denominator)` for a counter that states its
/// own rate, or `None` when the rate has to be measured.
fn self_describing_timebase() -> Option<(u64, u64)> {
    #[cfg(all(target_arch = "aarch64", not(target_os = "ios")))]
    {
        // CNTFRQ_EL0 is defined as the rate of CNTVCT_EL0, so these two
        // reads cannot disagree with each other.
        let freq: u64;
        unsafe {
            std::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack, preserves_flags));
        }
        // A zero here means firmware never programmed the register.
        if freq == 0 {
            return None;
        }
        Some((1_000_000_000, freq))
    }
    #[cfg(not(all(target_arch = "aarch64", not(target_os = "ios"))))]
    {
        None
    }
}

#[inline]
fn read_counter() -> u64 {
    #[cfg(all(target_arch = "aarch64", not(target_os = "ios")))]
    {
        let ticks: u64;
        unsafe {
            std::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks, options(nomem, nostack, preserves_flags));
        }
        ticks
    }
    #[cfg(not(all(target_arch = "aarch64", not(target_os = "ios"))))]
    {
        unreachable!("counter is only read directly where it is self-describing")
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
