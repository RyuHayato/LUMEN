//! Playback queue with deterministic ordering.
//!
//! `tracks` holds the queue in user-visible order. `order` holds the *play*
//! order as indices into `tracks` — identity mapping when shuffle is off, a
//! seeded permutation when shuffle is on. `position` points into `order`.
//!
//! Shuffle uses a small internal PRNG with a stored seed so behavior is
//! deterministic and testable without external dependencies.

use serde::{Deserialize, Serialize};

use crate::playback::state::RepeatMode;
use crate::TrackId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queue {
    tracks: Vec<TrackId>,
    order: Vec<usize>,
    position: Option<usize>,
    repeat: RepeatMode,
    shuffle: bool,
    seed: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    pub fn new() -> Self {
        Self::with_seed(0x5EED_5EED_5EED_5EED)
    }

    pub fn with_seed(seed: u64) -> Self {
        Self {
            tracks: Vec::new(),
            order: Vec::new(),
            position: None,
            repeat: RepeatMode::Off,
            shuffle: false,
            seed,
        }
    }

    /// Replace the queue contents. `start` selects the initial track by its
    /// index in `tracks` (user-visible order).
    pub fn set_tracks(&mut self, tracks: Vec<TrackId>, start: Option<usize>) {
        self.tracks = tracks;
        self.rebuild_order();
        self.position = match start {
            Some(i) if i < self.tracks.len() => {
                let track_id = self.tracks[i];
                self.order
                    .iter()
                    .position(|&idx| self.tracks[idx] == track_id)
            }
            _ => None,
        };
    }

    pub fn tracks(&self) -> &[TrackId] {
        &self.tracks
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn current(&self) -> Option<TrackId> {
        self.position.map(|p| self.tracks[self.order[p]])
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    /// Enable/disable shuffle. The current track keeps playing and becomes
    /// the head of the new play order.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        if self.shuffle == shuffle {
            return;
        }
        self.shuffle = shuffle;
        let current = self.current();
        self.rebuild_order();
        self.position = match current {
            Some(id) => self.order.iter().position(|&idx| self.tracks[idx] == id),
            None => None,
        };
    }

    /// What `next_track` would return, without moving the position.
    ///
    /// The pipeline needs this to open the upcoming track's decoder while the
    /// current one is still playing (gapless). Peeking rather than advancing keeps
    /// the engine the single owner of queue position: the pipeline is told what is
    /// coming, and the position only moves when the advance is actually reported.
    pub fn peek_next(&self) -> Option<TrackId> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One && self.position.is_some() {
            return self.current();
        }
        match self.position {
            None => self.order.first().map(|&i| self.tracks[i]),
            Some(p) if p + 1 < self.order.len() => Some(self.tracks[self.order[p + 1]]),
            Some(_) if self.repeat == RepeatMode::All => {
                self.order.first().map(|&i| self.tracks[i])
            }
            Some(_) => None,
        }
    }

    /// Advance to the next track according to repeat/shuffle.
    /// Returns the newly current track, or `None` at the end of the queue.
    pub fn next_track(&mut self) -> Option<TrackId> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One && self.position.is_some() {
            return self.current();
        }
        let next_pos = match self.position {
            None => Some(0),
            Some(p) if p + 1 < self.order.len() => Some(p + 1),
            Some(_) if self.repeat == RepeatMode::All => Some(0),
            Some(_) => return None,
        };
        self.position = next_pos;
        self.current()
    }

    /// Step back. With `RepeatMode::All`, stepping back from the first track
    /// wraps to the end of the play order.
    pub fn previous_track(&mut self) -> Option<TrackId> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One && self.position.is_some() {
            return self.current();
        }
        let prev_pos = match self.position {
            None => Some(0),
            Some(p) if p > 0 => Some(p - 1),
            Some(_) if self.repeat == RepeatMode::All => Some(self.order.len() - 1),
            Some(_) => return None,
        };
        self.position = prev_pos;
        self.current()
    }

    fn rebuild_order(&mut self) {
        self.order = (0..self.tracks.len()).collect();
        if self.shuffle {
            self.fisher_yates();
        }
    }

    fn fisher_yates(&mut self) {
        for i in (1..self.order.len()).rev() {
            let j = self.next_random(i as u64 + 1) as usize;
            self.order.swap(i, j);
        }
    }

    /// xorshift64* — deterministic PRNG, uniform index in `0..bound`.
    fn next_random(&mut self, bound: u64) -> u64 {
        let mut x = self.seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) % bound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(n: usize) -> Queue {
        let mut q = Queue::with_seed(42);
        q.set_tracks((0..n as i64).collect(), None);
        q
    }

    #[test]
    fn sequential_next_then_end() {
        let mut q = queue(3);
        assert_eq!(q.next_track(), Some(0));
        assert_eq!(q.next_track(), Some(1));
        assert_eq!(q.next_track(), Some(2));
        assert_eq!(q.next_track(), None);
    }

    #[test]
    fn repeat_all_wraps() {
        let mut q = queue(2);
        q.set_repeat(RepeatMode::All);
        assert_eq!(q.next_track(), Some(0));
        assert_eq!(q.next_track(), Some(1));
        assert_eq!(q.next_track(), Some(0));
    }

    #[test]
    fn repeat_one_stays() {
        let mut q = queue(3);
        q.set_repeat(RepeatMode::One);
        assert_eq!(q.next_track(), Some(0));
        assert_eq!(q.next_track(), Some(0));
    }

    #[test]
    fn previous_at_start_without_repeat() {
        let mut q = queue(2);
        q.next_track();
        assert_eq!(q.previous_track(), None);
        assert_eq!(q.current(), Some(0));
    }

    #[test]
    fn previous_wraps_with_repeat_all() {
        let mut q = queue(3);
        q.set_repeat(RepeatMode::All);
        q.next_track();
        assert_eq!(q.previous_track(), Some(2));
    }

    #[test]
    fn shuffle_visits_every_track_once() {
        let mut q = queue(10);
        q.set_shuffle(true);
        let mut seen = Vec::new();
        while let Some(id) = q.next_track() {
            seen.push(id);
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn shuffle_is_deterministic_for_same_seed() {
        let mut a = queue(8);
        let mut b = queue(8);
        a.set_shuffle(true);
        b.set_shuffle(true);
        let seq_a: Vec<_> = std::iter::from_fn(|| a.next_track()).collect();
        let seq_b: Vec<_> = std::iter::from_fn(|| b.next_track()).collect();
        assert_eq!(seq_a, seq_b);
    }

    #[test]
    fn shuffle_keeps_current_track() {
        let mut q = queue(5);
        q.next_track(); // current = 0
        q.next_track(); // current = 1
        q.set_shuffle(true);
        assert_eq!(q.current(), Some(1));
        q.set_shuffle(false);
        assert_eq!(q.current(), Some(1));
    }

    #[test]
    fn set_tracks_with_start_index() {
        let mut q = queue(4);
        q.set_tracks(vec![10, 11, 12, 13], Some(2));
        assert_eq!(q.current(), Some(12));
    }

    #[test]
    fn empty_queue_operations_are_safe() {
        let mut q = Queue::new();
        assert_eq!(q.next_track(), None);
        assert_eq!(q.previous_track(), None);
        assert_eq!(q.current(), None);
    }

    #[test]
    fn peek_next_agrees_with_next_track_and_leaves_position_alone() {
        let mut q = queue(3);
        assert_eq!(q.peek_next(), Some(0), "empty position peeks the head");
        assert_eq!(q.current(), None, "peeking does not move the position");

        q.next_track();
        assert_eq!(q.peek_next(), Some(1));
        assert_eq!(q.current(), Some(0), "still where it was");

        q.next_track();
        q.next_track();
        assert_eq!(q.peek_next(), None, "end of queue without repeat");
        assert_eq!(q.current(), Some(2), "and the position never moved");
    }

    #[test]
    fn peek_next_honours_repeat_and_shuffle() {
        let mut q = queue(3);
        q.set_repeat(RepeatMode::All);
        q.next_track();
        q.next_track();
        q.next_track();
        assert_eq!(q.peek_next(), Some(0), "repeat-all wraps around");

        q.set_repeat(RepeatMode::One);
        q.next_track();
        assert_eq!(q.peek_next(), q.current(), "repeat-one peeks itself");

        let mut s = queue(8);
        s.set_shuffle(true);
        s.next_track();
        let peeked = s.peek_next();
        // Peeking must not disturb the shuffle cursor: the value a subsequent
        // advance yields has to be the value we advertised.
        assert_eq!(peeked, s.next_track());
    }
}
