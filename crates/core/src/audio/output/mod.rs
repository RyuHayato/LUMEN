//! Output backend boundary.
//!
//! Generic application code talks to [`OutputBackend`] only. Platform
//! implementations (Phase 2A: WASAPI shared mode on Windows) live in
//! submodules and are never referenced directly outside this module.
//!
//! Stream model (event-driven, matching how WASAPI shared mode works):
//! the pipeline drives the cadence — `wait_ready` → `writable_frames` →
//! `write`. The stream never blocks the caller longer than `wait_ready`'s
//! timeout. Everything here runs on the pipeline thread only.
//!
//! Rules (audio-engineering skill):
//! - No allocations, blocking I/O, or contentious locks inside `write`.
//! - Shared-mode output is never reported as bit-identical to the source;
//!   [`StreamInfo`](crate::audio::pipeline::StreamInfo) exposes the real path.

#[cfg(windows)]
pub mod windows;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AudioError;

/// Device-change notifications, re-exported so platform-neutral code can
/// `select!` on them without knowing which backend produces them.
#[cfg(windows)]
pub use windows::device_notifications::{DeviceEvent, Notifications};

/// Non-Windows stubs: no notifications exist, so the type is still available
/// and the engine's handling stays deterministic (verify at open time).
#[cfg(not(windows))]
pub use stub_notifications::{DeviceEvent, Notifications};

#[cfg(not(windows))]
mod stub_notifications {
    use serde::{Deserialize, Serialize};

    use crate::audio::output::DeviceState;

    /// See the Windows implementation for the contract this mirrors.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "event", rename_all = "kebab-case")]
    pub enum DeviceEvent {
        DefaultChanged {
            endpoint_id: Option<String>,
        },
        Added {
            endpoint_id: String,
        },
        Removed {
            endpoint_id: String,
        },
        StateChanged {
            endpoint_id: String,
            state: DeviceState,
        },
        Unavailable {
            reason: String,
        },
    }

    impl DeviceEvent {
        pub fn endpoint_id(&self) -> Option<&str> {
            match self {
                Self::DefaultChanged { endpoint_id } => endpoint_id.as_deref(),
                Self::Added { endpoint_id }
                | Self::Removed { endpoint_id }
                | Self::StateChanged { endpoint_id, .. } => Some(endpoint_id),
                Self::Unavailable { .. } => None,
            }
        }

        pub fn makes_endpoint_unusable(&self) -> bool {
            match self {
                Self::Removed { .. } => true,
                Self::StateChanged { state, .. } => *state != DeviceState::Active,
                Self::DefaultChanged { endpoint_id } => endpoint_id.is_none(),
                Self::Added { .. } | Self::Unavailable { .. } => false,
            }
        }
    }

    /// Inert handle: no platform notifications on this build.
    pub struct Notifications;

    impl Notifications {
        pub fn is_active(&self) -> bool {
            false
        }
    }
}

/// A concrete PCM format on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleFormat {
    F32,
    S16,
    S24In32,
    S32,
}

/// WASAPI stream category. `Shared` and `Exclusive` are both real,
/// implemented paths (Phase 2B). `OutputMode` is explicit and observable;
/// a stream that fell back from exclusive to shared reports
/// `StreamInfo.requested_mode = Exclusive` with `mode() = Shared`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    #[default]
    Shared,
    Exclusive,
}

/// Device availability state as reported by MMDevice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
    Unknown,
}

impl DeviceState {
    /// Map a raw `DEVICE_STATE` bitmask value. Only the low bits of the
    /// active/disabled/notpresent/unplugged flags are expected.
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Active,
            2 => Self::Disabled,
            4 => Self::NotPresent,
            8 => Self::Unplugged,
            _ => Self::Unknown,
        }
    }

    pub fn as_label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::NotPresent => "not-present",
            Self::Unplugged => "unplugged",
            Self::Unknown => "unknown",
        }
    }
}

/// A discovered output device. `id` is an opaque backend-specific identifier
/// (on Windows: the WASAPI endpoint ID string). Treat it as opaque
/// everywhere above the backend; stale IDs must fail gracefully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub state: DeviceState,
}

/// What the engine asks for when opening a stream.
/// `format: None` means "device default" (shared mode: the mix format;
/// exclusive mode requires an explicit source format and rejects `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRequest {
    pub mode: OutputMode,
    pub format: Option<OutputFormat>,
}

pub trait OutputBackend {
    fn name(&self) -> &'static str;
    fn enumerate_devices(&self) -> Result<Vec<OutputDevice>, AudioError>;
    fn default_device(&self) -> Result<Option<OutputDevice>, AudioError>;

    /// Open a stream on `device_id` (`None` = system default device).
    fn open_stream(
        &mut self,
        device_id: Option<&str>,
        request: &StreamRequest,
    ) -> Result<Box<dyn OutputStream>, AudioError>;
}

/// An open output stream. Driven by the pipeline thread.
///
/// Deliberately not `Send`: WASAPI objects (event handles, COM apartments)
/// are bound to the thread that created them. The stream is created, used,
/// and dropped on the pipeline thread.
pub trait OutputStream {
    /// The format actually negotiated with the device. This is the only
    /// format description the rest of the app may trust for the output path.
    fn negotiated_format(&self) -> OutputFormat;
    fn mode(&self) -> OutputMode;
    fn device_name(&self) -> &str;

    /// The device's own mix format (diagnostics; shared mode).
    fn mix_format(&self) -> Option<OutputFormat> {
        None
    }

    /// Whether the OS converts rate/channels between our negotiated format
    /// and the mix format (diagnostics honesty, never hidden).
    fn conversion_description(&self) -> String;

    fn start(&mut self) -> Result<(), AudioError>;
    fn stop(&mut self) -> Result<(), AudioError>;

    /// Wait until the device can accept more frames, up to `timeout`.
    /// `Ok(false)` = timeout.
    fn wait_ready(&mut self, timeout: Duration) -> Result<bool, AudioError>;

    /// Frames of free space in the device buffer right now.
    fn writable_frames(&self) -> Result<usize, AudioError>;

    /// The endpoint ID of the device this stream is bound to.
    /// Opaque to callers; matches the ID reported in device events.
    fn endpoint_id(&self) -> &str;

    /// Write interleaved f32 frames. `frames.len()` must not exceed
    /// `writable_frames() * channels`. Returns frames accepted.
    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError>;

    /// Write `frames` of silence without touching the input path
    /// (underrun handling).
    fn write_silence(&mut self, frames: usize) -> Result<(), AudioError>;
}

/// The platform's primary backend. On Windows this is WASAPI, supporting both
/// shared and exclusive mode (Phase 2A/2B).
pub fn default_backend() -> Box<dyn OutputBackend> {
    #[cfg(windows)]
    {
        Box::new(windows::WasapiBackend::new())
    }
    #[cfg(not(windows))]
    {
        Box::new(NullBackend)
    }
}

/// Register device-change notifications for the default backend.
///
/// Returns the registration handle plus the receiver the playback owner
/// selects on. Keep the handle alive for the lifetime of playback.
pub fn watch_devices() -> (Notifications, crossbeam_channel::Receiver<DeviceEvent>) {
    #[cfg(windows)]
    {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = windows::device_notifications::start(tx);
        (handle, rx)
    }
    #[cfg(not(windows))]
    {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = stub_notifications::Notifications;
        let _ = tx;
        (handle, rx)
    }
}

/// Placeholder backend for platforms without an implementation, so the core
/// compiles and fails gracefully everywhere.
#[cfg(not(windows))]
struct NullBackend;

#[cfg(not(windows))]
impl OutputBackend for NullBackend {
    fn name(&self) -> &'static str {
        "null"
    }

    fn enumerate_devices(&self) -> Result<Vec<OutputDevice>, AudioError> {
        Ok(Vec::new())
    }

    fn default_device(&self) -> Result<Option<OutputDevice>, AudioError> {
        Ok(None)
    }

    fn open_stream(
        &mut self,
        _device_id: Option<&str>,
        _request: &StreamRequest,
    ) -> Result<Box<dyn OutputStream>, AudioError> {
        Err(AudioError::BackendUnavailable(
            "no output backend implemented for this platform".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backend_reports_its_name() {
        let backend = default_backend();
        #[cfg(windows)]
        assert_eq!(backend.name(), "wasapi");
        #[cfg(not(windows))]
        assert_eq!(backend.name(), "null");
    }
}
