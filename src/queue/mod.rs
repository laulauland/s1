//! Bounded queues toward clients (spec "Toward clients").
//!
//! - [`Queue`]: one per backend and key, bounded in requests and in input
//!   tokens, with two lanes. The interactive lane always goes first, so an
//!   interactive request never waits behind queued bulk requests, only
//!   behind the ones already in flight.
//! - [`Scheduler`]: the queue plus the provider-side [`crate::limits::Gate`]
//!   and [`crate::breaker::Breaker`], as an async admission point.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

mod scheduler;

pub use scheduler::{Permit, Scheduler, Snapshot};

/// Client header that picks the lane: `interactive` (default) or `bulk`.
pub const LANE_HEADER: &str = "s1-lane";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// Single requests from a person or an agent: served first.
    #[default]
    Interactive,
    /// Streams and list calls.
    Bulk,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Interactive => "interactive",
            Lane::Bulk => "bulk",
        }
    }

    /// Lane named by the request headers.
    pub fn from_headers(h: &axum::http::HeaderMap) -> Self {
        match h.get(LANE_HEADER).and_then(|v| v.to_str().ok()) {
            Some(v) if v.trim().eq_ignore_ascii_case("bulk") => Lane::Bulk,
            _ => Lane::Interactive,
        }
    }

    fn index(self) -> usize {
        match self {
            Lane::Interactive => 0,
            Lane::Bulk => 1,
        }
    }
}

/// Queue sizes (`[queue]` in `config.toml`), per backend and key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bounds {
    /// Most waiting requests, both lanes together.
    pub max_requests: usize,
    /// Most waiting input tokens. A single larger request is still taken
    /// when the queue is empty.
    pub max_tokens: u64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_requests: 1024,
            max_tokens: 64_000_000,
        }
    }
}

/// Ticket of one waiting request.
pub type Ticket = u64;

/// The queue is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

#[derive(Debug, Clone)]
pub struct Queue {
    bounds: Bounds,
    lanes: [VecDeque<(Ticket, u64)>; 2],
    tokens: u64,
}

impl Queue {
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            lanes: [VecDeque::new(), VecDeque::new()],
            tokens: 0,
        }
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Add a request at the back of its lane, if the bounds allow.
    pub fn push(&mut self, lane: Lane, ticket: Ticket, tokens: u64) -> Result<(), Full> {
        if self.len() >= self.bounds.max_requests {
            return Err(Full);
        }
        if !self.is_empty() && self.tokens.saturating_add(tokens) > self.bounds.max_tokens {
            return Err(Full);
        }
        self.lanes[lane.index()].push_back((ticket, tokens));
        self.tokens = self.tokens.saturating_add(tokens);
        Ok(())
    }

    /// Add an admitted request (a retry) at the front of its lane. Not
    /// bounded: it was counted when it first came in.
    pub fn push_front(&mut self, lane: Lane, ticket: Ticket, tokens: u64) {
        self.lanes[lane.index()].push_front((ticket, tokens));
        self.tokens = self.tokens.saturating_add(tokens);
    }

    /// The next request to go: the interactive lane first.
    pub fn head(&self) -> Option<Ticket> {
        self.lanes.iter().find_map(|l| l.front().map(|(t, _)| *t))
    }

    pub fn pop_head(&mut self) -> Option<(Ticket, u64)> {
        let lane = self.lanes.iter_mut().find(|l| !l.is_empty())?;
        let (t, tokens) = lane.pop_front()?;
        self.tokens -= tokens;
        Some((t, tokens))
    }

    /// Remove a waiting request (its client went away).
    pub fn remove(&mut self, ticket: Ticket) -> bool {
        for lane in &mut self.lanes {
            if let Some(i) = lane.iter().position(|(t, _)| *t == ticket) {
                let (_, tokens) = lane.remove(i).expect("index is valid");
                self.tokens -= tokens;
                return true;
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        self.lanes.iter().map(VecDeque::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn depth(&self, lane: Lane) -> usize {
        self.lanes[lane.index()].len()
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Push(Lane, u64),
        Pop,
        Remove(u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (any::<bool>(), 0u64..5_000)
                .prop_map(|(b, t)| Op::Push(if b { Lane::Bulk } else { Lane::Interactive }, t)),
            Just(Op::Pop),
            (0u64..64).prop_map(Op::Remove),
        ]
    }

    #[test]
    fn lane_header_parses() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(Lane::from_headers(&h), Lane::Interactive);
        h.insert(LANE_HEADER, "Bulk".parse().unwrap());
        assert_eq!(Lane::from_headers(&h), Lane::Bulk);
    }

    #[test]
    fn one_big_request_fits_an_empty_queue() {
        let mut q = Queue::new(Bounds {
            max_requests: 10,
            max_tokens: 100,
        });
        assert_eq!(q.push(Lane::Bulk, 1, 500), Ok(()));
        assert_eq!(q.push(Lane::Bulk, 2, 1), Err(Full));
    }

    proptest! {
        /// Depth and tokens never exceed the bounds (apart from one oversized
        /// request in an empty queue); every accepted request leaves exactly
        /// once; an interactive request is never served after a bulk request
        /// that was waiting with it.
        #[test]
        fn bounds_lanes_and_accounting(
            max_requests in 1usize..20,
            max_tokens in 1u64..20_000,
            ops in proptest::collection::vec(op(), 1..300),
        ) {
            let mut q = Queue::new(Bounds { max_requests, max_tokens });
            let mut next = 0u64;
            let mut lane_of = std::collections::HashMap::new();
            let mut accepted = std::collections::HashSet::new();
            let mut left = std::collections::HashSet::new();
            for op in ops {
                match op {
                    Op::Push(lane, tokens) => {
                        let t = next;
                        next += 1;
                        let was_empty = q.is_empty();
                        if q.push(lane, t, tokens).is_ok() {
                            accepted.insert(t);
                            lane_of.insert(t, lane);
                            prop_assert!(was_empty || q.tokens() <= max_tokens);
                        }
                    }
                    Op::Pop => {
                        let interactive_waiting = q.depth(Lane::Interactive) > 0;
                        if let Some((t, _)) = q.pop_head() {
                            if interactive_waiting {
                                prop_assert_eq!(lane_of[&t], Lane::Interactive);
                            }
                            prop_assert!(left.insert(t), "left twice");
                        }
                    }
                    Op::Remove(t) => {
                        if q.remove(t) {
                            prop_assert!(left.insert(t), "left twice");
                        }
                    }
                }
                prop_assert!(q.len() <= max_requests);
                prop_assert_eq!(q.len(), accepted.len() - left.len());
            }
            while let Some((t, _)) = q.pop_head() {
                prop_assert!(left.insert(t));
            }
            prop_assert_eq!(&left, &accepted);
            prop_assert_eq!(q.tokens(), 0);
        }
    }
}
