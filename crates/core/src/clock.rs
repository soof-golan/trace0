use std::sync::Arc;

pub use quanta::Mock;

#[derive(Clone, Debug)]
enum Source {
    SelfDescribing { nanos_num: u64, nanos_den: u64 },
    Measured(quanta::Clock),
}

#[derive(Clone, Debug)]
pub struct Clock {
    source: Source,
    start_ticks: u64,
}

impl Clock {
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

    pub fn mock() -> (Self, Arc<Mock>) {
        let (inner, mock) = quanta::Clock::mock();
        Self::mock_from(inner, mock)
    }

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

    pub fn is_direct(&self) -> bool {
        matches!(self.source, Source::SelfDescribing { .. })
    }

    #[inline]
    pub fn raw(&self) -> u64 {
        match &self.source {
            Source::SelfDescribing { .. } => read_counter(),
            Source::Measured(clock) => clock.raw(),
        }
    }

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

    pub fn ticks_from_ns(&self, ns: u64) -> u64 {
        match &self.source {
            Source::SelfDescribing {
                nanos_num,
                nanos_den,
            } => {
                let ticks = ns as u128 * *nanos_den as u128 / *nanos_num as u128;
                self.start_ticks.saturating_add(ticks as u64)
            }
            Source::Measured(_) => {
                let mut span: u64 = 1;
                while self.ns_since_start(self.start_ticks.saturating_add(span)) < ns {
                    let Some(doubled) = span.checked_mul(2) else {
                        return u64::MAX;
                    };
                    span = doubled;
                }
                let mut lo = self.start_ticks.saturating_add(span / 2);
                let mut hi = self.start_ticks.saturating_add(span);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if self.ns_since_start(mid) < ns {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                lo
            }
        }
    }
}

fn self_describing_timebase() -> Option<(u64, u64)> {
    #[cfg(all(target_arch = "aarch64", not(target_os = "ios")))]
    {
        let freq: u64;
        unsafe {
            std::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack, preserves_flags));
        }
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

#[inline(always)]
pub fn read_counter() -> u64 {
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
    #[cfg(all(target_arch = "aarch64", not(target_os = "ios")))]
    fn a_counter_that_states_its_own_rate_is_not_calibrated() {
        assert!(Clock::starting_now().is_direct());
    }

    #[test]
    fn a_direct_clock_reads_the_counter_read_counter_reads() {
        let c = Clock::starting_now();
        if !c.is_direct() {
            return;
        }
        let before = read_counter();
        let raw = c.raw();
        let after = read_counter();
        assert!(
            before <= raw && raw <= after,
            "raw() returned {raw}, outside [{before}, {after}]"
        );
    }

    #[test]
    fn a_mock_clock_only_advances_when_told() {
        let (c, m) = Clock::mock();
        let first = c.raw();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(c.raw(), first, "the mock advanced on its own");
        m.increment(5u64);
        assert_eq!(c.raw(), first + 5);
    }

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
    fn zero_nanoseconds_map_back_to_the_anchor() {
        let (c, _m) = Clock::mock_starting_at(10_000);
        assert_eq!(c.ticks_from_ns(0), 10_000);
    }

    #[test]
    fn ticks_from_ns_inverts_ns_since_start() {
        let (c, _m) = Clock::mock_starting_at(7);
        for ns in [1u64, 1_000, 250_000_000, 1 << 40] {
            let ticks = c.ticks_from_ns(ns);
            assert_eq!(c.ns_since_start(ticks), ns);
        }
    }

    #[test]
    fn a_direct_clock_inverts_within_one_tick() {
        let c = Clock::starting_now();
        if !c.is_direct() {
            return;
        }
        for ns in [1u64, 1_000, 250_000_000, 1 << 40] {
            let round = c.ns_since_start(c.ticks_from_ns(ns));
            assert!(ns.abs_diff(round) <= 1_000, "{ns}ns came back as {round}ns");
        }
    }

    #[test]
    fn the_host_clock_advances_in_step_with_wall_time() {
        let c = Clock::starting_now();
        let before = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let measured = c.ns_since_start(c.raw());
        let actual = before.elapsed().as_nanos() as u64;
        assert!(
            measured > actual / 2 && measured < actual * 2,
            "measured {measured}ns against {actual}ns of wall time"
        );
    }
}
