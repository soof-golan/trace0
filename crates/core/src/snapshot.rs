use crate::clock::Clock;
use crate::event::{Event, EventKind, pack_code_kind};
use crate::evqueue::EventBatch;

pub fn assemble<'a>(
    history: impl IntoIterator<Item = &'a EventBatch>,
    tails: &[EventBatch],
    clock: &Clock,
    horizon_ticks: u64,
    start_ticks: u64,
    end_ticks: u64,
) -> Vec<Event> {
    let history: Vec<&EventBatch> = history.into_iter().collect();
    let fresh_tails: Vec<&EventBatch> = tails
        .iter()
        .filter(|t| {
            !history
                .iter()
                .any(|h| h.tid == t.tid && h.base_ticks == t.base_ticks)
        })
        .collect();

    let mut streams: Vec<(u32, Vec<(u64, u32)>)> = Vec::new();
    for batch in history.iter().chain(fresh_tails.iter()) {
        let stream = match streams.iter_mut().find(|(tid, _)| *tid == batch.tid) {
            Some((_, stream)) => stream,
            None => {
                streams.push((batch.tid, Vec::new()));
                &mut streams.last_mut().unwrap().1
            }
        };
        stream.extend(
            batch
                .events
                .iter()
                .map(|p| (batch.base_ticks + p.delta_ticks as u64, p.code_kind)),
        );
    }

    let mut out: Vec<(u64, u32, u32)> = Vec::new();
    for (tid, stream) in &streams {
        replay(
            *tid,
            stream,
            horizon_ticks,
            start_ticks,
            end_ticks,
            &mut out,
        );
    }
    out.sort_by_key(|(ticks, _, _)| *ticks);
    out.into_iter()
        .map(|(ticks, tid, code_kind)| {
            let kind = EventKind::from_u8((code_kind >> 24) as u8);
            Event::new(
                clock.ns_since_start(ticks),
                tid,
                code_kind & crate::event::CODE_ID_MASK,
                kind,
            )
        })
        .collect()
}

fn replay(
    tid: u32,
    stream: &[(u64, u32)],
    horizon_ticks: u64,
    start_ticks: u64,
    end_ticks: u64,
    out: &mut Vec<(u64, u32, u32)>,
) {
    let mut stack: Vec<(u64, u32)> = Vec::new();
    let mut opens_emitted = false;
    for (ticks, code_kind) in stream.iter().copied() {
        let opens = EventKind::from_u8((code_kind >> 24) as u8).opens_slice();
        if ticks < start_ticks {
            if opens {
                stack.push((ticks, code_kind));
            } else {
                stack.pop();
            }
            continue;
        }
        if !opens_emitted {
            opens_emitted = true;
            out.extend(stack.iter().map(|(ts, ck)| (*ts, tid, *ck)));
        }
        if ticks > end_ticks {
            return;
        }
        if opens {
            stack.push((ticks, code_kind));
        } else if stack.pop().is_none() {
            let code_id = code_kind & crate::event::CODE_ID_MASK;
            let synthesized = pack_code_kind(code_id, EventKind::Begin);
            out.push((horizon_ticks, tid, synthesized));
        }
        out.push((ticks, tid, code_kind));
    }
    if !opens_emitted {
        out.extend(stack.iter().map(|(ts, ck)| (*ts, tid, *ck)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PackedEvent;

    fn batch(tid: u32, base_ticks: u64, events: &[(u32, u32, EventKind)]) -> EventBatch {
        EventBatch {
            base_ticks,
            tid,
            events: events
                .iter()
                .map(|(delta, code, kind)| PackedEvent {
                    delta_ticks: *delta,
                    code_kind: pack_code_kind(*code, *kind),
                })
                .collect(),
        }
    }

    fn shape(events: &[Event]) -> Vec<(u64, u32, u32, EventKind)> {
        events
            .iter()
            .map(|e| (e.ts_ns, e.tid, e.code_id(), e.kind()))
            .collect()
    }

    fn clock() -> Clock {
        Clock::mock().0
    }

    #[test]
    fn a_middle_slice_carries_only_its_own_events() {
        let history = [batch(
            1,
            0,
            &[
                (0, 10, EventKind::Begin),
                (5, 10, EventKind::End),
                (100, 20, EventKind::Begin),
                (150, 20, EventKind::End),
                (300, 30, EventKind::Begin),
                (310, 30, EventKind::End),
            ],
        )];
        let out = assemble(&history, &[], &clock(), 0, 90, 200);
        assert_eq!(
            shape(&out),
            vec![(100, 1, 20, EventKind::Begin), (150, 1, 20, EventKind::End),]
        );
    }

    #[test]
    fn a_function_open_before_the_slice_appears_as_an_open_span() {
        let history = [batch(
            1,
            0,
            &[
                (10, 10, EventKind::Begin),
                (120, 20, EventKind::Begin),
                (130, 20, EventKind::End),
            ],
        )];
        let out = assemble(&history, &[], &clock(), 0, 100, 200);
        assert_eq!(
            shape(&out),
            vec![
                (10, 1, 10, EventKind::Begin),
                (120, 1, 20, EventKind::Begin),
                (130, 1, 20, EventKind::End),
            ]
        );
    }

    #[test]
    fn an_idle_thread_still_shows_its_open_frames() {
        let history = [batch(1, 0, &[(10, 10, EventKind::Begin)])];
        let out = assemble(&history, &[], &clock(), 0, 100, 200);
        assert_eq!(shape(&out), vec![(10, 1, 10, EventKind::Begin)]);
    }

    #[test]
    fn a_close_without_an_open_synthesizes_the_open_at_the_horizon() {
        let history = [batch(
            1,
            0,
            &[(50, 10, EventKind::End), (60, 20, EventKind::Begin)],
        )];
        let out = assemble(&history, &[], &clock(), 30, 40, 100);
        assert_eq!(
            shape(&out),
            vec![
                (30, 1, 10, EventKind::Begin),
                (50, 1, 10, EventKind::End),
                (60, 1, 20, EventKind::Begin),
            ]
        );
    }

    #[test]
    fn a_batch_present_in_both_ring_and_tails_is_counted_once() {
        let history = [batch(
            1,
            100,
            &[
                (0, 7, EventKind::Begin),
                (1, 7, EventKind::End),
                (2, 7, EventKind::Begin),
                (3, 7, EventKind::End),
            ],
        )];
        let tails = [
            batch(1, 100, &[(0, 7, EventKind::Begin), (1, 7, EventKind::End)]),
            batch(2, 100, &[(5, 9, EventKind::Begin)]),
        ];
        let out = assemble(&history, &tails, &clock(), 0, 0, 1000);
        assert_eq!(
            shape(&out),
            vec![
                (100, 1, 7, EventKind::Begin),
                (101, 1, 7, EventKind::End),
                (102, 1, 7, EventKind::Begin),
                (103, 1, 7, EventKind::End),
                (105, 2, 9, EventKind::Begin),
            ]
        );
    }

    #[test]
    fn threads_do_not_leak_frames_into_each_other() {
        let history = [
            batch(1, 0, &[(10, 1, EventKind::Begin)]),
            batch(2, 0, &[(50, 2, EventKind::End)]),
        ];
        let out = assemble(&history, &[], &clock(), 5, 40, 100);
        assert_eq!(
            shape(&out),
            vec![
                (5, 2, 2, EventKind::Begin),
                (10, 1, 1, EventKind::Begin),
                (50, 2, 2, EventKind::End),
            ]
        );
    }

    #[test]
    fn a_yield_closes_the_frame_and_a_resume_reopens_it() {
        let history = [batch(
            1,
            0,
            &[
                (10, 10, EventKind::Begin),
                (20, 10, EventKind::Yield),
                (120, 10, EventKind::Resume),
                (130, 10, EventKind::End),
            ],
        )];
        let out = assemble(&history, &[], &clock(), 0, 100, 200);
        assert_eq!(
            shape(&out),
            vec![
                (120, 1, 10, EventKind::Resume),
                (130, 1, 10, EventKind::End),
            ]
        );
    }
}
