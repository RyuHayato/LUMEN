//! Playback domain logic: the playback state machine and the queue.
//!
//! Pure logic, no I/O, no OS audio APIs — fully unit-testable.

pub mod queue;
pub mod state;

pub use queue::Queue;
pub use state::{PlaybackState, RepeatMode};
