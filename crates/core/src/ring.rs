use crate::evqueue::EventBatch;
use std::collections::VecDeque;

pub struct Ring {
    batches: VecDeque<Box<EventBatch>>,
    bytes: usize,
    capacity: usize,
    floor: Option<u64>,
    horizon_ticks: u64,
    evicted_events: u64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Self {
            batches: VecDeque::new(),
            bytes: 0,
            capacity,
            floor: None,
            horizon_ticks: 0,
            evicted_events: 0,
        }
    }

    pub fn push(&mut self, batch: Box<EventBatch>) {
        self.bytes += batch.bytes();
        self.batches.push_back(batch);
        self.evict_over_capacity();
    }

    pub fn pop(&mut self) -> Option<Box<EventBatch>> {
        let batch = self.batches.pop_front()?;
        self.bytes -= batch.bytes();
        Some(batch)
    }

    pub fn set_floor(&mut self, floor: Option<u64>) {
        self.floor = floor;
        self.evict_over_capacity();
    }

    fn evict_over_capacity(&mut self) {
        while self.bytes > self.capacity && self.batches.len() > 1 {
            let oldest_end = self.batches.front().unwrap().end_ticks();
            let protected = self.floor.is_some_and(|floor| oldest_end >= floor);
            if protected && self.bytes <= self.capacity.saturating_mul(2) {
                return;
            }
            let evicted = self.pop().unwrap();
            self.horizon_ticks = evicted.end_ticks();
            self.evicted_events += evicted.events.len() as u64;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &EventBatch> {
        self.batches.iter().map(|b| b.as_ref())
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.batches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn horizon_ticks(&self) -> u64 {
        self.horizon_ticks
    }

    pub fn evicted_events(&self) -> u64 {
        self.evicted_events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PackedEvent;

    fn batch(base_ticks: u64, tid: u32, events: usize) -> Box<EventBatch> {
        let mut b = Box::new(EventBatch::with_capacity(events, base_ticks, tid));
        for i in 0..events {
            b.events.push(PackedEvent {
                delta_ticks: i as u32,
                code_kind: 0,
            });
        }
        b
    }

    fn one() -> usize {
        batch(0, 1, 4).bytes()
    }

    #[test]
    fn an_empty_batch_ends_where_it_begins() {
        assert_eq!(batch(50, 1, 0).end_ticks(), 50);
        assert_eq!(batch(50, 1, 3).end_ticks(), 52);
    }

    #[test]
    fn bytes_track_pushes_and_pops() {
        let mut ring = Ring::new(usize::MAX);
        assert_eq!(ring.bytes(), 0);
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 1, 4));
        assert_eq!(ring.bytes(), 2 * one());
        assert_eq!(ring.len(), 2);
        ring.pop().unwrap();
        assert_eq!(ring.bytes(), one());
        ring.pop().unwrap();
        assert_eq!(ring.bytes(), 0);
        assert!(ring.is_empty());
    }

    #[test]
    fn iteration_walks_oldest_to_newest_without_consuming() {
        let mut ring = Ring::new(usize::MAX);
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 2, 4));
        let tids: Vec<u32> = ring.iter().map(|b| b.tid).collect();
        assert_eq!(tids, [1, 2]);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn pop_returns_batches_in_arrival_order() {
        let mut ring = Ring::new(usize::MAX);
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 2, 4));
        ring.push(batch(200, 3, 4));
        assert_eq!(ring.pop().unwrap().tid, 1);
        assert_eq!(ring.pop().unwrap().tid, 2);
        assert_eq!(ring.pop().unwrap().tid, 3);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn eviction_removes_the_oldest_batch_first() {
        let mut ring = Ring::new(2 * one());
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 2, 4));
        ring.push(batch(200, 3, 4));
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.evicted_events(), 4);
        assert_eq!(ring.pop().unwrap().tid, 2);
        assert_eq!(ring.pop().unwrap().tid, 3);
    }

    #[test]
    fn the_horizon_is_the_end_of_the_newest_evicted_batch() {
        let mut ring = Ring::new(2 * one());
        ring.push(batch(0, 1, 4));
        assert_eq!(ring.horizon_ticks(), 0);
        ring.push(batch(100, 2, 4));
        ring.push(batch(200, 3, 4));
        assert_eq!(ring.horizon_ticks(), 3);
        ring.push(batch(300, 4, 4));
        assert_eq!(ring.horizon_ticks(), 103);
    }

    #[test]
    fn popping_does_not_move_the_horizon() {
        let mut ring = Ring::new(usize::MAX);
        ring.push(batch(0, 1, 4));
        ring.pop().unwrap();
        assert_eq!(ring.horizon_ticks(), 0);
        assert_eq!(ring.evicted_events(), 0);
    }

    #[test]
    fn a_floor_protects_history_from_eviction() {
        let mut ring = Ring::new(2 * one());
        ring.set_floor(Some(0));
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 2, 4));
        ring.push(batch(200, 3, 4));
        assert_eq!(ring.len(), 3);
        assert!(ring.bytes() > 2 * one());
        assert_eq!(ring.horizon_ticks(), 0);
    }

    #[test]
    fn batches_that_end_before_the_floor_still_evict() {
        let mut ring = Ring::new(2 * one());
        ring.set_floor(Some(150));
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 2, 4));
        ring.push(batch(200, 3, 4));
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.horizon_ticks(), 3);
    }

    #[test]
    fn clearing_the_floor_evicts_the_backlog() {
        let mut ring = Ring::new(2 * one());
        ring.set_floor(Some(0));
        for i in 0..4u64 {
            ring.push(batch(i * 100, i as u32 + 1, 4));
        }
        assert_eq!(ring.len(), 4);
        ring.set_floor(None);
        assert_eq!(ring.len(), 2);
        assert!(ring.bytes() <= 2 * one());
        assert_eq!(ring.pop().unwrap().tid, 3);
    }

    #[test]
    fn the_hard_cap_overrides_the_floor() {
        let mut ring = Ring::new(2 * one());
        ring.set_floor(Some(0));
        for i in 0..5u64 {
            ring.push(batch(i * 100, i as u32 + 1, 4));
        }
        assert_eq!(ring.len(), 4);
        assert!(ring.bytes() <= 4 * one());
        assert_eq!(ring.horizon_ticks(), 3);
    }

    #[test]
    fn the_newest_batch_survives_a_capacity_smaller_than_itself() {
        let mut ring = Ring::new(1);
        ring.push(batch(0, 1, 4));
        ring.push(batch(100, 2, 4));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.pop().unwrap().tid, 2);
    }
}
