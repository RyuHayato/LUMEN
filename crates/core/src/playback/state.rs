//! Explicit playback state machine.
//!
//! There is exactly one authoritative owner of this state: the audio engine
//! thread. UI components keep no duplicate playback state.

use serde::{Deserialize, Serialize};

use crate::error::PlaybackError;

/// Repeat behavior for the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// Coarse playback states. `Seeking` and `Buffering` are transitional states
/// entered from a stable state (`Playing`/`Paused`) and left back to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
    Seeking,
    Buffering,
    Finished,
    Error,
}

impl PlaybackState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Loading => "loading",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Seeking => "seeking",
            Self::Buffering => "buffering",
            Self::Finished => "finished",
            Self::Error => "error",
        }
    }

    /// Whether a transition from `self` to `next` is allowed.
    pub fn can_transition(self, next: Self) -> bool {
        use PlaybackState::*;
        match self {
            Stopped => matches!(next, Loading),
            Loading => matches!(next, Playing | Buffering | Error | Stopped),
            Playing => matches!(
                next,
                Paused | Seeking | Buffering | Finished | Error | Stopped
            ),
            Paused => matches!(next, Playing | Seeking | Stopped | Error),
            // Seeking returns to the state it came from; both are accepted here.
            Seeking => matches!(next, Playing | Paused | Error | Stopped),
            Buffering => matches!(next, Playing | Error | Stopped),
            Finished => matches!(next, Loading | Stopped),
            Error => matches!(next, Stopped | Loading),
        }
    }

    /// Apply a transition, returning an error when it is not allowed.
    pub fn transition(&mut self, next: Self) -> Result<(), PlaybackError> {
        if self.can_transition(next) {
            *self = next;
            Ok(())
        } else {
            Err(PlaybackError::InvalidTransition {
                from: self.name().to_string(),
                to: next.name().to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_play_pause_finish() {
        let mut s = PlaybackState::Stopped;
        s.transition(PlaybackState::Loading).unwrap();
        s.transition(PlaybackState::Playing).unwrap();
        s.transition(PlaybackState::Paused).unwrap();
        s.transition(PlaybackState::Playing).unwrap();
        s.transition(PlaybackState::Finished).unwrap();
        s.transition(PlaybackState::Loading).unwrap();
    }

    #[test]
    fn stopped_cannot_jump_to_playing() {
        let mut s = PlaybackState::Stopped;
        let err = s.transition(PlaybackState::Playing).unwrap_err();
        assert_eq!(
            err,
            PlaybackError::InvalidTransition {
                from: "stopped".into(),
                to: "playing".into()
            }
        );
        assert_eq!(s, PlaybackState::Stopped);
    }

    #[test]
    fn seek_roundtrip_from_playing_and_paused() {
        let mut s = PlaybackState::Stopped;
        s.transition(PlaybackState::Loading).unwrap();
        s.transition(PlaybackState::Playing).unwrap();
        s.transition(PlaybackState::Seeking).unwrap();
        s.transition(PlaybackState::Playing).unwrap();

        s.transition(PlaybackState::Paused).unwrap();
        s.transition(PlaybackState::Seeking).unwrap();
        s.transition(PlaybackState::Paused).unwrap();
    }

    #[test]
    fn error_recovers_through_stopped() {
        let mut s = PlaybackState::Stopped;
        s.transition(PlaybackState::Loading).unwrap();
        s.transition(PlaybackState::Error).unwrap();
        s.transition(PlaybackState::Stopped).unwrap();
        s.transition(PlaybackState::Loading).unwrap();
    }

    #[test]
    fn finished_cannot_play_directly() {
        let mut s = PlaybackState::Stopped;
        s.transition(PlaybackState::Loading).unwrap();
        s.transition(PlaybackState::Playing).unwrap();
        s.transition(PlaybackState::Finished).unwrap();
        assert!(s.transition(PlaybackState::Playing).is_err());
    }
}
