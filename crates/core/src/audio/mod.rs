//! Audio engine boundary: command/event protocol and output abstraction.
//!
//! The UI never touches OS audio APIs. It sends [`AudioCommand`]s to the
//! engine thread and observes [`AudioEvent`]s.

pub mod decode;
pub mod engine;
pub mod output;
pub mod pipeline;

pub use engine::{AudioEngine, EngineHandle, StateSnapshot, TrackResolver};
pub use pipeline::StreamInfo;

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::playback::{PlaybackState, RepeatMode};
use crate::TrackId;

/// Commands accepted by the audio engine thread.
///
/// Internal to the Rust core — not serialized across the UI boundary.
/// `QueryState` carries a reply channel, which is why this type does not
/// derive serde traits.
#[derive(Debug)]
pub enum AudioCommand {
    /// Replace the queue and optionally select the starting track.
    LoadQueue {
        tracks: Vec<TrackId>,
        start_index: Option<usize>,
    },
    /// Replace the queue and immediately play `start_index`.
    PlayQueue {
        tracks: Vec<TrackId>,
        start_index: usize,
    },
    /// Context-dependent: resume when paused; start the current track when
    /// stopped/finished/error; no-op while already playing.
    Play,
    Pause,
    Stop,
    SeekTo {
        position_ms: u64,
    },
    Next,
    Previous,
    SetVolume {
        volume: f32,
    },
    SetRepeat {
        mode: RepeatMode,
    },
    SetShuffle {
        enabled: bool,
    },
    /// Explicitly select the output endpoint (`None` = follow system default).
    SetOutputDevice {
        device_id: Option<String>,
    },
    /// Switch output mode (shared/exclusive). Applied to the active stream if
    /// one is playing; otherwise takes effect on the next track open.
    SetOutputMode {
        mode: crate::audio::output::OutputMode,
    },
    /// Cross track boundaries without a gap: the next decoder is read ahead and
    /// swapped in while the current stream keeps running.
    SetGapless {
        enabled: bool,
    },
    /// Request a point-in-time snapshot of engine state.
    QueryState {
        respond: Sender<StateSnapshot>,
    },
    /// Stop the engine thread. Terminal.
    Shutdown,
}

/// Facts emitted by the audio engine. Serializable: these cross to the UI.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "event",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum AudioEvent {
    StateChanged {
        state: PlaybackState,
    },
    QueueChanged {
        len: usize,
        current: Option<TrackId>,
    },
    VolumeChanged {
        volume: f32,
    },
    /// A track started: the honest description of the real playback path
    /// (source format, decoded format, output format, conversion).
    StreamStarted {
        info: StreamInfo,
    },
    /// Periodic position while playing (~4 Hz) and after seeks.
    PositionUpdate {
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    /// Recoverable error surfaced for UI display + logs.
    Error {
        message: String,
    },
}
