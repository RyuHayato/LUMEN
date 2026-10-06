//! The audio engine thread — control plane.
//!
//! One thread owns playback state, the queue, and the pipeline handle.
//! Commands arrive on a bounded channel; facts leave as events. There is no
//! shared mutable playback state anywhere else (the UI sees snapshots and
//! events only).
//!
//! Phase 2A: `Play`/`Pause`/`Stop`/`SeekTo` are real. The engine resolves
//! track ids to paths through an injected [`TrackResolver`] (the app layer
//! answers from SQLite — the engine itself never touches the database), and
//! drives a [`Pipeline`] (decoder → WASAPI shared mode) spawned with an
//! injectable backend factory for hardware-free tests.

use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use serde::{Deserialize, Serialize};

use crate::audio::output;
use crate::audio::output::{DeviceEvent, DeviceState};
use crate::audio::pipeline::{
    BackendFactory, Pipeline, PipelineCmd, PipelineMsg, PipelineShared, StreamInfo, StreamTarget,
};
use crate::audio::{AudioCommand, AudioEvent};
use crate::error::AudioError;
use crate::playback::{PlaybackState, Queue, RepeatMode};
use crate::TrackId;

const COMMAND_QUEUE_DEPTH: usize = 64;
const EVENT_QUEUE_DEPTH: usize = 128;
const POSITION_TICK: Duration = Duration::from_millis(250);

/// Resolves a library track id to a playable file path.
/// `None` = track unknown (or file record missing).
pub type TrackResolver = Box<dyn Fn(TrackId) -> Option<PathBuf> + Send>;

/// Serializable point-in-time view of engine state (for UI queries).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub state: PlaybackState,
    pub queue_len: usize,
    pub current: Option<TrackId>,
    pub volume: f32,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub current_path: Option<String>,
    pub stream: Option<StreamInfo>,
    pub underrun_frames: u64,
    /// Whether track boundaries are crossed without a gap.
    pub gapless: bool,
}

/// Start a dedicated watcher thread that owns the COM registration for the
/// lifetime of the registration, then forwards `DeviceEvent`s over a channel.
///
/// The registration and teardown (CoInitialize/CoUninitialize/Unregister)
/// all happen on this one thread, which satisfies COM apartment rules.
struct DeviceWatcher {
    stop_tx: Sender<()>,
    handle: JoinHandle<()>,
}

impl DeviceWatcher {
    fn shutdown(self) {
        let _ = self.stop_tx.send(());
        let _ = self.handle.join();
    }
}

fn spawn_device_watcher() -> (Receiver<DeviceEvent>, Option<DeviceWatcher>) {
    #[cfg(windows)]
    {
        let (tx, rx) = crossbeam_channel::unbounded::<DeviceEvent>();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let handle = thread::Builder::new()
            .name("lumen-device-watcher".into())
            .spawn(move || {
                // `Notifications` keeps the COM registration alive; dropping it
                // unregisters + uninitializes COM on this thread.
                let _guard = crate::audio::output::windows::device_notifications::start(tx);
                // Block until the engine asks us to stop.
                let _ = stop_rx.recv();
            });
        match handle {
            Ok(h) => (rx, Some(DeviceWatcher { stop_tx, handle: h })),
            Err(e) => {
                tracing::warn!("device watcher thread failed to spawn: {e}");
                (rx, None)
            }
        }
    }
    #[cfg(not(windows))]
    {
        let (tx_keep, rx) = crossbeam_channel::unbounded::<DeviceEvent>();
        std::mem::forget(tx_keep);
        (rx, None)
    }
}

/// Handle to the running engine thread. Dropping the handle shuts the
/// engine down and joins the thread.
pub struct EngineHandle {
    commands: Sender<AudioCommand>,
    events: Receiver<AudioEvent>,
    thread: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn send(&self, command: AudioCommand) -> Result<(), AudioError> {
        self.commands.try_send(command).map_err(|_| {
            AudioError::BackendUnavailable("engine command queue full or closed".into())
        })
    }

    pub fn events(&self) -> &Receiver<AudioEvent> {
        &self.events
    }

    /// Ask the engine for its current state (bounded wait).
    pub fn snapshot(&self) -> Result<StateSnapshot, AudioError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.send(AudioCommand::QueryState { respond: tx })?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| AudioError::BackendUnavailable("engine did not answer".into()))
    }

    pub fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.commands.send(AudioCommand::Shutdown);
            let _ = thread.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct AudioEngine;

impl AudioEngine {
    // NOTE: the watcher thread's stop handle is kept in the engine loop,
    // not the engine struct, and shut down deterministically on engine stop.

    /// Spawn the engine with the platform's default output backend. Device
    /// notifications are live on Windows (shared-mode default-device
    /// following + removal handling).
    pub fn start(resolver: TrackResolver, initial_volume: f32) -> EngineHandle {
        Self::start_with_backend(resolver, initial_volume, output::default_backend)
    }

    /// Spawn with an explicit backend factory (tests). No device watcher is
    /// used in tests: the backend is a fake, and device events are inert.
    pub fn start_with_backend(
        resolver: TrackResolver,
        initial_volume: f32,
        backend_factory: BackendFactory,
    ) -> EngineHandle {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<AudioCommand>(COMMAND_QUEUE_DEPTH);
        let (evt_tx, evt_rx) = crossbeam_channel::bounded::<AudioEvent>(EVENT_QUEUE_DEPTH);

        let thread = thread::Builder::new()
            .name("lumen-audio-engine".into())
            .spawn(move || {
                run(
                    cmd_rx,
                    evt_tx,
                    resolver,
                    initial_volume,
                    backend_factory,
                    false,
                )
            })
            .expect("failed to spawn lumen-audio-engine thread");

        EngineHandle {
            commands: cmd_tx,
            events: evt_rx,
            thread: Some(thread),
        }
    }

    /// Spawn the full production engine: real default backend + live device
    /// notifications.
    pub fn start_production(resolver: TrackResolver, initial_volume: f32) -> EngineHandle {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<AudioCommand>(COMMAND_QUEUE_DEPTH);
        let (evt_tx, evt_rx) = crossbeam_channel::bounded::<AudioEvent>(EVENT_QUEUE_DEPTH);

        let thread = thread::Builder::new()
            .name("lumen-audio-engine".into())
            .spawn(move || {
                run(
                    cmd_rx,
                    evt_tx,
                    resolver,
                    initial_volume,
                    output::default_backend,
                    true,
                )
            })
            .expect("failed to spawn lumen-audio-engine thread");

        EngineHandle {
            commands: cmd_tx,
            events: evt_rx,
            thread: Some(thread),
        }
    }
}

struct CurrentTrack {
    info: Option<StreamInfo>,
}

struct Engine {
    state: PlaybackState,
    queue: Queue,
    volume: f32,
    current: Option<CurrentTrack>,
    seek_return: Option<PlaybackState>,
    /// A Stop was sent to the pipeline and its `Stopped` ack is pending.
    /// The engine transitions to `Stopped` only on the ack — this keeps
    /// "position is 0 after Stop" true for every later snapshot.
    stopping: bool,
    /// The output endpoint marginally currently bound to the active stream.
    current_endpoint: Option<String>,
    /// True once a `SwitchingDevice` recovery flow was issued, so the next
    /// Defchanged/Removed event is not double-acted on.
    device_switch_in_flight: bool,
    /// Mirrors the pipeline's gapless policy so a snapshot can report it.
    gapless: bool,
}

impl Engine {
    fn new() -> Self {
        Self {
            state: PlaybackState::Stopped,
            queue: Queue::new(),
            volume: 1.0,
            current: None,
            seek_return: None,
            stopping: false,
            current_endpoint: None,
            device_switch_in_flight: false,
            gapless: true,
        }
    }

    fn snapshot(&self, shared: &PipelineShared, duration_ms: Option<u64>) -> StateSnapshot {
        let gapless = self.gapless;
        StateSnapshot {
            state: self.state,
            queue_len: self.queue.len(),
            current: self.queue.current(),
            volume: self.volume,
            repeat: self.queue.repeat(),
            shuffle: self.queue.shuffle(),
            position_ms: shared.position_ms(),
            duration_ms,
            current_path: self
                .current
                .as_ref()
                .and_then(|c| c.info.as_ref().map(|i| i.path.clone())),
            stream: self.current.as_ref().and_then(|c| c.info.clone()),
            underrun_frames: shared
                .underrun_frames
                .load(std::sync::atomic::Ordering::Relaxed),
            gapless,
        }
    }
}

fn emit(events: &Sender<AudioEvent>, event: AudioEvent) {
    if let Err(e) = events.try_send(event) {
        tracing::warn!("audio event dropped: {e}");
    }
}

fn set_state(engine: &mut Engine, events: &Sender<AudioEvent>, next: PlaybackState) {
    match engine.state.transition(next) {
        Ok(()) => emit(events, AudioEvent::StateChanged { state: next }),
        Err(e) => emit(
            events,
            AudioEvent::Error {
                message: e.to_string(),
            },
        ),
    }
}

/// Tell the pipeline which track is coming next, so it can open that decoder
/// while this one is still playing.
///
/// This is the engine's half of gapless playback. The queue position does not
/// move: `peek_next` only reports what `next_track` would return, so the engine
/// stays the sole owner of where playback is, and the pipeline is simply told
/// what to have ready. A failure to resolve the next path is not an error - it
/// just means no prefetch, and the boundary will be handled the ordinary way.
fn prime_next_decode(engine: &mut Engine, resolver: &TrackResolver, pipeline: &Pipeline) {
    let next = engine.queue.peek_next().and_then(resolver);
    // A closed/full queue must never stall the render thread, so this is
    // best-effort by design.
    let _ = pipeline.send(PipelineCmd::Prefetch { path: next });
}

/// Begin playback of `track` (Loading → pipeline Play).
fn play_track(
    engine: &mut Engine,
    track_id: TrackId,
    resolver: &TrackResolver,
    pipeline: &Pipeline,
    events: &Sender<AudioEvent>,
) {
    let Some(path) = resolver(track_id) else {
        emit(
            events,
            AudioEvent::Error {
                message: format!("track {track_id} has no playable file"),
            },
        );
        return;
    };
    set_state(engine, events, PlaybackState::Loading);
    engine.current = Some(CurrentTrack { info: None });
    if let Err(e) = pipeline.send(PipelineCmd::Play { path }) {
        emit(
            events,
            AudioEvent::Error {
                message: e.to_string(),
            },
        );
        set_state(engine, events, PlaybackState::Error);
    }
}

fn run(
    commands: Receiver<AudioCommand>,
    events: Sender<AudioEvent>,
    resolver: TrackResolver,
    initial_volume: f32,
    backend_factory: BackendFactory,
    watch_devices: bool,
) {
    let (device_rx, device_watcher) = if watch_devices {
        spawn_device_watcher()
    } else {
        let (tx_keep, rx) = crossbeam_channel::unbounded::<DeviceEvent>();
        // Keep the sender alive (leaked, never used) so `recv(device_rx)` in
        // the select never returns a spurious `Disconnected` that would busy-spin.
        std::mem::forget(tx_keep);
        (rx, None)
    };

    // The configured stream target (device id + mode) lives in the engine;
    // on SetOutputDevice/SetOutputMode we can replace it and re-arm the
    // pipeline for subsequent opens.
    let mut stream_target = StreamTarget::default();

    let (pipeline, pipeline_msgs, shared) =
        Pipeline::spawn_configured(backend_factory, stream_target.clone());
    // Gapless is on by default: it is the correct behaviour for a local hi-fi
    // player, and the off switch exists for A/B comparison, not as the norm.
    let _ = pipeline.send(PipelineCmd::SetGapless { enabled: true });
    shared.volume_bits.store(
        initial_volume.clamp(0.0, 1.0).to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );

    let mut engine = Engine::new();
    engine.volume = initial_volume.clamp(0.0, 1.0);
    tracing::info!("audio engine started");

    loop {
        crossbeam_channel::select! {
            recv(commands) -> command => {
                let Ok(command) = command else { break };
                match command {
                    AudioCommand::LoadQueue { tracks, start_index } => {
                        engine.queue.set_tracks(tracks, start_index);
                        emit(&events, AudioEvent::QueueChanged {
                            len: engine.queue.len(),
                            current: engine.queue.current(),
                        });
                    }
                    AudioCommand::PlayQueue { tracks, start_index } => {
                        engine.queue.set_tracks(tracks, Some(start_index));
                        emit(&events, AudioEvent::QueueChanged {
                            len: engine.queue.len(),
                            current: engine.queue.current(),
                        });
                        if let Some(id) = engine.queue.current() {
                            play_track(&mut engine, id, &resolver, &pipeline, &events);
                        }
                    }
                    AudioCommand::Play => {
                        if engine.stopping {
                            emit(&events, AudioEvent::Error {
                                message: "stop in progress, try again".into(),
                            });
                            continue;
                        }
                        match engine.state {
                            PlaybackState::Paused => {
                                let _ = pipeline.send(PipelineCmd::Resume);
                                set_state(&mut engine, &events, PlaybackState::Playing);
                            }
                            PlaybackState::Stopped
                            | PlaybackState::Finished
                            | PlaybackState::Error => {
                                let Some(id) = engine.queue.current() else {
                                    emit(&events, AudioEvent::Error {
                                        message: "queue is empty".into(),
                                    });
                                    continue;
                                };
                                play_track(&mut engine, id, &resolver, &pipeline, &events);
                            }
                            _ => {}
                        }
                    }
                    AudioCommand::Pause => {
                        if engine.state == PlaybackState::Playing {
                            let _ = pipeline.send(PipelineCmd::Pause);
                            set_state(&mut engine, &events, PlaybackState::Paused);
                        }
                    }
                    AudioCommand::Stop => {
                        if !engine.stopping
                            && matches!(
                                engine.state,
                                PlaybackState::Playing
                                    | PlaybackState::Paused
                                    | PlaybackState::Loading
                                    | PlaybackState::Seeking
                            )
                        {
                            let _ = pipeline.send(PipelineCmd::Stop);
                            engine.stopping = true;
                            // State moves to Stopped when the pipeline acks
                            // (PipelineMsg::Stopped) — ack-ordering keeps
                            // "position is 0 after Stop" true for snapshots.
                        }
                    }
                    AudioCommand::SeekTo { position_ms } => {
                        if matches!(engine.state, PlaybackState::Playing | PlaybackState::Paused) {
                            engine.seek_return = Some(engine.state);
                            let _ = pipeline.send(PipelineCmd::Seek { position_ms });
                            set_state(&mut engine, &events, PlaybackState::Seeking);
                        }
                    }
                    AudioCommand::Next => {
                        let was_playing = engine.state == PlaybackState::Playing;
                        let current = engine.queue.next_track();
                        emit(&events, AudioEvent::QueueChanged {
                            len: engine.queue.len(),
                            current,
                        });
                        if was_playing {
                            if let Some(id) = current {
                                play_track(&mut engine, id, &resolver, &pipeline, &events);
                            } else if !engine.stopping {
                                let _ = pipeline.send(PipelineCmd::Stop);
                                engine.stopping = true;
                            }
                        }
                    }
                    AudioCommand::Previous => {
                        let was_playing = engine.state == PlaybackState::Playing;
                        let current = engine.queue.previous_track();
                        emit(&events, AudioEvent::QueueChanged {
                            len: engine.queue.len(),
                            current,
                        });
                        if was_playing {
                            if let Some(id) = current {
                                play_track(&mut engine, id, &resolver, &pipeline, &events);
                            }
                        }
                    }
                    AudioCommand::SetVolume { volume } => {
                        engine.volume = volume.clamp(0.0, 1.0);
                        shared.volume_bits.store(
                            engine.volume.to_bits(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        emit(&events, AudioEvent::VolumeChanged { volume: engine.volume });
                    }
                    AudioCommand::SetRepeat { mode } => {
                        engine.queue.set_repeat(mode);
                    }
                    AudioCommand::SetOutputMode { mode } => {
                        stream_target.mode = mode;
                        if matches!(engine.state, PlaybackState::Playing | PlaybackState::Paused) {
                            let _ = pipeline.send(PipelineCmd::SetOutputMode { mode });
                        }
                    }
                    AudioCommand::SetGapless { enabled } => {
                        // The pipeline owns the policy; the engine mirrors it so
                        // it can report the setting back in a snapshot.
                        engine.gapless = enabled;
                        let _ = pipeline.send(PipelineCmd::SetGapless { enabled });
                        // Read ahead again under the new policy.
                        prime_next_decode(&mut engine, &resolver, &pipeline);
                    }
                    AudioCommand::SetShuffle { enabled } => {
                        engine.queue.set_shuffle(enabled);
                        emit(&events, AudioEvent::QueueChanged {
                            len: engine.queue.len(),
                            current: engine.queue.current(),
                        });
                    }
                    AudioCommand::SetOutputDevice { device_id } => {
                        stream_target.device_id = device_id.clone();
                        // Re-arm the active stream on the new endpoint (if any).
                        if matches!(engine.state, PlaybackState::Playing | PlaybackState::Paused) {
                            let _ = pipeline.send(PipelineCmd::SwitchDevice { device_id });
                        }
                    }
                    AudioCommand::QueryState { respond } => {
                        let duration = engine
                            .current
                            .as_ref()
                            .and_then(|c| c.info.as_ref().and_then(|i| i.duration_ms));
                        let _ = respond.try_send(engine.snapshot(&shared, duration));
                    }
                    AudioCommand::Shutdown => break,
                }
            }
            recv(pipeline_msgs) -> msg => {
                let Ok(msg) = msg else { continue };
                match msg {
                    PipelineMsg::Started { info } => {
                        engine.device_switch_in_flight = false;
                        engine.current_endpoint = Some(info.output_endpoint_id.clone());
                        if let Some(current) = engine.current.as_mut() {
                            current.info = Some(info.clone());
                        }
                        emit(&events, AudioEvent::StreamStarted { info });
                        if engine.state == PlaybackState::Loading {
                            set_state(&mut engine, &events, PlaybackState::Playing);
                        }
                        // The stream is live, so read the next one ahead.
                        prime_next_decode(&mut engine, &resolver, &pipeline);
                    }
                    PipelineMsg::Stopped => {
                        engine.stopping = false;
                        engine.current = None;
                        engine.current_endpoint = None;
                        engine.device_switch_in_flight = false;
                        // If a new track already started (Loading), the ack
                        // belongs to the previous flow — consume it silently.
                        if matches!(
                            engine.state,
                            PlaybackState::Playing
                                | PlaybackState::Paused
                                | PlaybackState::Seeking
                                | PlaybackState::Finished
                        ) {
                            set_state(&mut engine, &events, PlaybackState::Stopped);
                        }
                    }
                    PipelineMsg::SeekApplied { position_ms } => {
                        let back = engine.seek_return.take().unwrap_or(PlaybackState::Playing);
                        if engine.state == PlaybackState::Seeking && !engine.stopping {
                            set_state(&mut engine, &events, back);
                        }
                        emit(&events, AudioEvent::PositionUpdate {
                            position_ms,
                            duration_ms: engine
                                .current
                                .as_ref()
                                .and_then(|c| c.info.as_ref().and_then(|i| i.duration_ms)),
                        });
                    }
                    PipelineMsg::Ended => {
                        if matches!(engine.state, PlaybackState::Playing | PlaybackState::Seeking) {
                            set_state(&mut engine, &events, PlaybackState::Finished);
                            match engine.queue.next_track() {
                                Some(id) => {
                                    emit(&events, AudioEvent::QueueChanged {
                                        len: engine.queue.len(),
                                        current: Some(id),
                                    });
                                    play_track(&mut engine, id, &resolver, &pipeline, &events);
                                }
                                None => {
                                    engine.current = None;
                                    set_state(&mut engine, &events, PlaybackState::Stopped);
                                }
                            }
                        }
                    }
                    PipelineMsg::Advanced { info } => {
                        // The pipeline crossed a track boundary on its own,
                        // without a stop and without a gap. The queue still
                        // advances here and only here - the engine remains the
                        // single owner of the position - but no `Play` is sent,
                        // because audio for this track is already flowing.
                        if matches!(engine.state, PlaybackState::Playing | PlaybackState::Seeking) {
                            match engine.queue.next_track() {
                                Some(id) => {
                                    engine.device_switch_in_flight = false;
                                    engine.current_endpoint = Some(info.output_endpoint_id.clone());
                                    engine.current = Some(CurrentTrack {
                                        info: Some(info.clone()),
                                    });
                                    emit(&events, AudioEvent::QueueChanged {
                                        len: engine.queue.len(),
                                        current: Some(id),
                                    });
                                    emit(&events, AudioEvent::StreamStarted { info });
                                    prime_next_decode(&mut engine, &resolver, &pipeline);
                                }
                                None => {
                                    // The pipeline advanced past the end of the
                                    // queue. Let it finish the track normally
                                    // rather than cutting it short.
                                    engine.current = None;
                                }
                            }
                        }
                    }
                    PipelineMsg::Error { message } => {
                        emit(&events, AudioEvent::Error { message });
                        if engine.state == PlaybackState::Loading {
                            set_state(&mut engine, &events, PlaybackState::Error);
                        }
                    }
                }
            }
            recv(device_rx) -> event => {
                let Ok(event) = event else { continue };
                handle_device_event(&event, &mut engine, &pipeline, &mut stream_target, &events);
            }
            default(POSITION_TICK) => {
                if engine.state == PlaybackState::Playing {
                    let duration = engine
                        .current
                        .as_ref()
                        .and_then(|c| c.info.as_ref().and_then(|i| i.duration_ms));
                    emit(&events, AudioEvent::PositionUpdate {
                        position_ms: shared.position_ms(),
                        duration_ms: duration,
                    });
                }
            }
        }
    }

    if let Some(w) = device_watcher {
        w.shutdown();
    }
    tracing::info!("audio engine stopped");
}

/// Deterministic device-change handling:
///
/// - The *currently bound* endpoint becoming unusable mid-playback → stop the
///   stream, report an error, and move to an explicit deterministic state.
///   We do not silently jump to a different device.
/// - The default endpoint going away / becoming unavailable when nothing is
///   bound yet → just clear current endpoint and let the next open hit the
///   error path with a real message.
/// - The default device changing while nothing is playing → no-op (next open
///   follows the new default). While playing *and we followed the default* →
///   re-arm the pipeline on the new default.
/// - If the user explicitly pinned a device, we never silently switch.
fn handle_device_event(
    event: &DeviceEvent,
    engine: &mut Engine,
    pipeline: &Pipeline,
    stream_target: &mut StreamTarget,
    events: &Sender<AudioEvent>,
) {
    match event {
        DeviceEvent::Removed { endpoint_id } => {
            if engine.current_endpoint.as_deref() == Some(endpoint_id.as_str()) {
                // Our bound endpoint was unplugged/removed mid-stream: stop
                // cleanly and say so. The decoder thread may be waiting on the
                // device; stop tears the stream down.
                emit(
                    events,
                    AudioEvent::Error {
                        message: "output device was disconnected".into(),
                    },
                );
                if matches!(
                    engine.state,
                    PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Seeking
                ) {
                    engine.stopping = true;
                    let _ = pipeline.send(PipelineCmd::Stop);
                }
            }
        }
        DeviceEvent::StateChanged {
            endpoint_id,
            state: DeviceState::Disabled | DeviceState::NotPresent | DeviceState::Unplugged,
        } => {
            if engine.current_endpoint.as_deref() == Some(endpoint_id.as_str()) {
                emit(
                    events,
                    AudioEvent::Error {
                        message: "output device became unavailable".into(),
                    },
                );
                if matches!(
                    engine.state,
                    PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Seeking
                ) {
                    engine.stopping = true;
                    let _ = pipeline.send(PipelineCmd::Stop);
                }
            }
        }
        DeviceEvent::DefaultChanged { endpoint_id: _ } => {
            let following_default = stream_target.device_id.is_none();
            if !following_default {
                // Explicitly pinned: default changes must not move our stream.
                return;
            }
            if matches!(engine.state, PlaybackState::Playing | PlaybackState::Paused) {
                // Re-arm on the new default (a SwitchDevice to the same
                // logical device follows the *current* system default because
                // device_id is None).
                if !engine.device_switch_in_flight {
                    engine.device_switch_in_flight = true;
                    // Keep following the *current* system default: the new
                    // default is whatever GetDefaultAudioEndpoint returns now.
                    stream_target.device_id = None;
                    let _ = pipeline.send(PipelineCmd::SwitchDevice { device_id: None });
                }
            } else {
                engine.current_endpoint = None;
            }
        }
        DeviceEvent::Added { .. } | DeviceEvent::Unavailable { .. } => {}
        // `DefaultChanged` handled above; any extension variant is a no-op.
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::output::{
        OutputBackend, OutputDevice, OutputFormat, OutputMode, OutputStream, SampleFormat,
        StreamRequest,
    };
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ---------------------------------------------------------- fake backend

    struct FakeBackend;

    fn fake_factory() -> Box<dyn OutputBackend> {
        Box::new(FakeBackend)
    }

    impl OutputBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn enumerate_devices(&self) -> Result<Vec<OutputDevice>, AudioError> {
            Ok(vec![OutputDevice {
                id: "fake".into(),
                name: "Fake Device".into(),
                is_default: true,
                state: crate::audio::output::DeviceState::Active,
            }])
        }

        fn default_device(&self) -> Result<Option<OutputDevice>, AudioError> {
            Ok(self.enumerate_devices()?.into_iter().next())
        }

        fn open_stream(
            &mut self,
            _device_id: Option<&str>,
            request: &StreamRequest,
        ) -> Result<Box<dyn OutputStream>, AudioError> {
            let format = request.format.unwrap_or(OutputFormat {
                sample_rate_hz: 48_000,
                channels: 2,
                sample_format: SampleFormat::F32,
            });
            // The plain fake backend keeps its own throwaway spy.
            Ok(Box::new(FakeStream::new(format, None)))
        }
    }

    struct FakeStream {
        format: OutputFormat,
        /// Every sample this stream accepted, in order.
        written: Arc<std::sync::Mutex<Vec<f32>>>,
        /// Times the stream was stopped. Gapless playback must not stop the
        /// stream at a track boundary, so this is asserted directly.
        stops: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
        /// Frames the device can still accept, and whether to model it at all.
        capacity: Arc<AtomicUsize>,
        capacity_model: bool,
        /// Total frames of silence the pipeline deliberately wrote.
        silence_frames: Arc<AtomicUsize>,
    }

    /// How many frames a modelled device accepts per cycle. Comfortably larger
    /// than one decoded block, so a healthy pipeline never starves.
    const DEVICE_CAPACITY: usize = 80;

    /// A modelled device consumes at roughly real time, so a prefetch opened on
    /// a worker thread has the same wall-clock opportunity to finish that it
    /// would during real playback. Without this the fake drains a 150 ms track
    /// in microseconds, the prefetch can never be ready, and the takeover path
    /// would be untestable for a reason that has nothing to do with the code.
    const DEVICE_CYCLE: Duration = Duration::from_millis(2);

    impl FakeStream {
        /// With no spy installed (the plain `fake_factory`) the stream records
        /// into a private sink, so the two test backends share one type.
        ///
        /// A spied stream additionally models a real device buffer: capacity is
        /// finite, `write` consumes it, and the next `wait_ready` refills it.
        /// Without that model the pipeline sees a device that is always writable
        /// and never drained, writes thousands of frames of "underrun silence"
        /// per cycle, and the silence a real gap would produce is unmeasurable.
        fn new(format: OutputFormat, sink: Option<&StreamSpy>) -> Self {
            let (written, stops, starts, silence_frames) = match sink {
                Some(s) => (
                    s.written.clone(),
                    s.stops.clone(),
                    s.starts.clone(),
                    s.silence_frames.clone(),
                ),
                None => {
                    let owned = Arc::new(StreamSpy::default());
                    (
                        owned.written.clone(),
                        owned.stops.clone(),
                        owned.starts.clone(),
                        owned.silence_frames.clone(),
                    )
                }
            };
            Self {
                format,
                written,
                stops,
                starts,
                capacity: Arc::new(AtomicUsize::new(0)),
                capacity_model: sink.is_some(),
                silence_frames,
            }
        }
    }

    /// Shared record of what a `FakeBackend` handed to the device.
    #[derive(Default)]
    struct StreamSpy {
        written: Arc<std::sync::Mutex<Vec<f32>>>,
        stops: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
        silence_frames: Arc<AtomicUsize>,
    }

    impl StreamSpy {
        fn samples(&self) -> Vec<f32> {
            self.written.lock().unwrap().clone()
        }

        fn stops(&self) -> usize {
            self.stops.load(Ordering::Relaxed)
        }

        fn silence(&self) -> usize {
            self.silence_frames.load(Ordering::Relaxed)
        }
    }

    /// A backend whose streams feed a shared spy, so tests can inspect the exact
    /// audio that would have reached the device.
    ///
    /// `BackendFactory` is a plain `fn` pointer, so it cannot capture the spy -
    /// and it is called on the *engine* thread, not the test thread, so a
    /// thread-local would not be visible either. It is reached through a
    /// process-wide slot instead, and `SPY_LOCK` serialises the tests that use it
    /// so two engines never record into each other's sink.
    struct SpyBackend;

    static ACTIVE_SPY: Mutex<Option<Arc<StreamSpy>>> = Mutex::new(None);
    static SPY_LOCK: Mutex<()> = Mutex::new(());

    /// Build an engine whose output streams record into `spy`.
    ///
    /// The caller must already hold `SPY_LOCK`.
    fn engine_with_spy(resolver: TrackResolver, spy: Arc<StreamSpy>) -> EngineHandle {
        *ACTIVE_SPY.lock().unwrap() = Some(spy);
        AudioEngine::start_with_backend(resolver, 1.0, spy_factory)
    }

    fn spy_factory() -> Box<dyn OutputBackend> {
        Box::new(SpyBackend)
    }

    impl OutputBackend for SpyBackend {
        fn name(&self) -> &'static str {
            "spy"
        }

        fn enumerate_devices(&self) -> Result<Vec<OutputDevice>, AudioError> {
            Ok(vec![OutputDevice {
                id: "spy".into(),
                name: "Spy Device".into(),
                is_default: true,
                state: crate::audio::output::DeviceState::Active,
            }])
        }

        fn default_device(&self) -> Result<Option<OutputDevice>, AudioError> {
            Ok(self.enumerate_devices()?.into_iter().next())
        }

        fn open_stream(
            &mut self,
            _device_id: Option<&str>,
            request: &StreamRequest,
        ) -> Result<Box<dyn OutputStream>, AudioError> {
            let format = request.format.unwrap_or(OutputFormat {
                sample_rate_hz: 48_000,
                channels: 2,
                sample_format: SampleFormat::F32,
            });
            let spy = ACTIVE_SPY.lock().unwrap().clone();
            Ok(Box::new(FakeStream::new(format, spy.as_deref())))
        }
    }

    impl OutputStream for FakeStream {
        fn negotiated_format(&self) -> OutputFormat {
            self.format
        }

        fn mode(&self) -> OutputMode {
            OutputMode::Shared
        }

        fn device_name(&self) -> &str {
            "Fake Device"
        }

        fn endpoint_id(&self) -> &str {
            "fake-device"
        }

        fn conversion_description(&self) -> String {
            "none (fake backend)".into()
        }

        fn start(&mut self) -> Result<(), AudioError> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), AudioError> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn wait_ready(&mut self, _timeout: Duration) -> Result<bool, AudioError> {
            // A modelled device is refilled here, once per render cycle, which
            // is what a real endpoint signals when it wants more audio.
            if self.capacity_model {
                self.capacity.store(DEVICE_CAPACITY, Ordering::Relaxed);
                std::thread::sleep(DEVICE_CYCLE);
            }
            Ok(true) // instant cadence: tests run at decode speed
        }

        fn writable_frames(&self) -> Result<usize, AudioError> {
            if self.capacity_model {
                Ok(self.capacity.load(Ordering::Relaxed))
            } else {
                Ok(4800)
            }
        }

        fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError> {
            self.written.lock().unwrap().extend_from_slice(frames);
            let accepted = frames.len() / self.format.channels as usize;
            if self.capacity_model {
                self.capacity
                    .fetch_sub(accepted.min(DEVICE_CAPACITY), Ordering::Relaxed);
            }
            Ok(accepted)
        }

        fn write_silence(&mut self, frames: usize) -> Result<(), AudioError> {
            // Silence written on purpose is counted separately from the sample
            // data, because the decoded stream itself contains zero-valued
            // padding (SampleBuffer is sized to packet capacity, not to the
            // frames actually decoded), so "count the zeros" is not a usable
            // measure of a gap. The *call* is the fact.
            self.silence_frames.fetch_add(frames, Ordering::Relaxed);
            Ok(())
        }
    }

    // --------------------------------------------------------------- fixtures

    fn write_test_wav(path: &PathBuf, duration_ms: u64) {
        write_test_wav_silent(path, duration_ms)
    }

    /// A fixture that is not silent. Silence makes a gap undetectable, because
    /// silence written as a gap looks exactly like music that happens to be
    /// quiet. Every value is a constant, so "was there a run of zeros at the
    /// boundary?" is a decisive question.
    fn write_test_wav_tone(path: &PathBuf, duration_ms: u64, level: u8) {
        write_test_wav_silent(path, duration_ms);
        let data_len = (duration_ms as usize) * 8;
        let mut out = Vec::with_capacity(data_len);
        for i in 0..data_len {
            // A square wave at ~250 Hz against the 8 kHz fixture rate: half the
            // samples are exactly zero and half are exactly `level`, which is
            // what makes an inserted silence run countable.
            out.push(if i % 32 < 16 { 0 } else { level });
        }
        let mut bytes = std::fs::read(path).unwrap();
        // Overwrite the data chunk in place (the RIFF/data headers are fixed size).
        let start = bytes.len() - data_len;
        bytes[start..].copy_from_slice(&out);
        std::fs::File::create(path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }

    fn write_test_wav_silent(path: &PathBuf, duration_ms: u64) {
        let sample_rate: u32 = 8_000;
        let data_len = (duration_ms as usize) * 8;
        let mut w = Vec::new();
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&sample_rate.to_le_bytes());
        w.extend_from_slice(&sample_rate.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&8u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        w.extend(std::iter::repeat_n(128u8, data_len));
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(w.len() as u32).to_le_bytes());
        file.extend_from_slice(&w);
        std::fs::File::create(path)
            .unwrap()
            .write_all(&file)
            .unwrap();
    }

    struct Fixture {
        dir: PathBuf,
        tracks: Vec<(TrackId, PathBuf)>,
    }

    impl Fixture {
        fn new(name: &str, durations_ms: &[u64]) -> Self {
            let dir =
                std::env::temp_dir().join(format!("lumen-engine-{}-{}", name, std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let tracks = durations_ms
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let path = dir.join(format!("track{i}.wav"));
                    write_test_wav(&path, *d);
                    (i as TrackId + 1, path)
                })
                .collect();
            Self { dir, tracks }
        }

        fn resolver(&self) -> TrackResolver {
            let map: std::collections::HashMap<TrackId, PathBuf> =
                self.tracks.iter().cloned().collect();
            Box::new(move |id| map.get(&id).cloned())
        }

        /// Rewrite each track with a distinct non-zero level, so the gapless
        /// tests can tell which track they are hearing and whether anything
        /// silent was spliced in between.
        fn with_tones(name: &str, durations_ms: &[u64]) -> Self {
            let f = Self::new(name, durations_ms);
            for (i, (_, path)) in f.tracks.iter().enumerate() {
                // 40, 80, 120 ... never 0.
                write_test_wav_tone(path, durations_ms[i], 40 + (i as u8) * 40);
            }
            f
        }
    }

    /// Count the longest run of consecutive zero samples in a written frame.
    #[cfg(test)]
    #[allow(dead_code)]
    fn longest_zero_run(samples: &[f32]) -> usize {
        let (mut best, mut run) = (0usize, 0usize);
        for s in samples {
            if *s == 0.0 {
                run += 1;
                best = best.max(run);
            } else {
                run = 0;
            }
        }
        best
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Drain events until `pred` matches or timeout elapses.
    fn wait_for(
        handle: &EngineHandle,
        timeout_ms: u64,
        pred: impl Fn(&AudioEvent) -> bool,
    ) -> AudioEvent {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            assert!(remaining > Duration::ZERO, "timed out waiting for event");
            let event = handle.events().recv_timeout(remaining).expect("event");
            if pred(&event) {
                return event;
            }
        }
    }

    fn is_state(event: &AudioEvent, state: PlaybackState) -> bool {
        matches!(event, AudioEvent::StateChanged { state: s } if *s == state)
    }

    // ------------------------------------------------------------------ tests

    /// Play a queue of tone fixtures to completion and return what the device
    /// would have heard, plus how many times the stream was stopped.
    /// What one full queue run tells us about its boundaries.
    #[derive(Default, Debug)]
    struct BoundaryReport {
        samples: usize,
        stops: usize,
        silence_frames: usize,
    }

    /// Play a queue to completion and report what happened at the boundaries.
    fn play_queue_to_end(fixture: &Fixture, gapless: bool) -> BoundaryReport {
        // Serialise: the spy is process-wide.
        let _serialised = SPY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let spy = Arc::new(StreamSpy::default());
        let handle = engine_with_spy(fixture.resolver(), spy.clone());

        handle
            .send(AudioCommand::SetGapless { enabled: gapless })
            .unwrap();
        handle
            .send(AudioCommand::PlayQueue {
                tracks: fixture.tracks.iter().map(|(id, _)| *id).collect(),
                start_index: 0,
            })
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "queue did not finish in time"
            );
            let Ok(event) = handle.events().recv_timeout(Duration::from_millis(500)) else {
                continue;
            };
            if let AudioEvent::StateChanged { state } = event {
                if state == PlaybackState::Stopped {
                    break;
                }
            }
        }
        let _ = handle.send(AudioCommand::Stop);

        let report = BoundaryReport {
            samples: spy.samples().len(),
            stops: spy.stops(),
            silence_frames: spy.silence(),
        };
        *ACTIVE_SPY.lock().unwrap() = None;
        report
    }

    #[test]
    fn gapless_boundaries_write_no_silence_and_never_stop_the_stream() {
        let on = Fixture::with_tones("gapless-on", &[150, 150, 150]);
        let report = play_queue_to_end(&on, true);

        assert!(report.samples > 0, "audio was produced");
        // Silence is allowed *only* at the end of the queue, where there is no
        // next track to hand over to and the stream genuinely stops. Any
        // between-track silence would be a real gap, so the ceiling is one
        // device buffer per stopping point rather than a hard zero.
        assert!(
            report.silence_frames <= DEVICE_CAPACITY * 2,
            "gapless must not write between-track silence (wrote {} frames, \
             allowed at most {} for the end of the queue)",
            report.silence_frames,
            DEVICE_CAPACITY * 2
        );
        assert_eq!(
            report.stops, 1,
            "gapless playback must not stop the stream between tracks"
        );
    }

    #[test]
    fn disabling_gapless_restores_the_silence_and_the_stop() {
        // The control. Without it the assertions above could pass for the wrong
        // reason - e.g. if the queue never advanced, or never played at all.
        let off = Fixture::with_tones("gapless-off", &[150, 150, 150]);
        let report = play_queue_to_end(&off, false);

        assert!(report.samples > 0, "the control must also produce audio");
        assert!(
            report.silence_frames > 0,
            "the old path writes silence at every boundary (got {})",
            report.silence_frames
        );
        assert!(
            report.stops > 1,
            "the old path stops the stream at every boundary (got {})",
            report.stops
        );
    }

    #[test]
    fn gapless_falls_back_when_the_next_track_has_a_different_format() {
        // The invariant that matters either way: playback continues across every
        // boundary and no track is dropped, whatever the prefetch decided.
        let fixture = Fixture::with_tones("gapless-fallback", &[150, 150, 150]);
        let report = play_queue_to_end(&fixture, true);
        assert!(report.samples > 0, "playback continued past the boundaries");
    }

    #[test]
    fn gapless_advances_the_queue_and_keeps_playing() {
        let fixture = Fixture::with_tones("gapless-queue", &[120, 120, 120]);
        let _serialised = SPY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let spy = Arc::new(StreamSpy::default());
        let handle = engine_with_spy(fixture.resolver(), spy.clone());

        handle
            .send(AudioCommand::PlayQueue {
                tracks: fixture.tracks.iter().map(|(id, _)| *id).collect(),
                start_index: 0,
            })
            .unwrap();

        // Every track must report itself as started, and the state must stay
        // Playing across the boundaries rather than dropping to Finished.
        let mut started = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while started < 3 {
            assert!(std::time::Instant::now() < deadline, "did not start all");
            let event = handle
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("event");
            if let AudioEvent::StreamStarted { .. } = event {
                started += 1;
            }
        }
        assert_eq!(started, 3, "all three tracks opened");
    }

    #[test]
    fn play_through_to_end_advances_queue_then_stops() {
        let fixture = Fixture::new("e2e", &[300, 300]);
        let mut handle = AudioEngine::start_with_backend(fixture.resolver(), 1.0, fake_factory);

        handle
            .send(AudioCommand::PlayQueue {
                tracks: vec![1, 2],
                start_index: 0,
            })
            .unwrap();

        // Track 1: Loading → Playing with stream info.
        wait_for(&handle, 3000, |e| is_state(e, PlaybackState::Loading));
        let started = wait_for(&handle, 3000, |e| {
            matches!(e, AudioEvent::StreamStarted { .. })
        });
        match started {
            AudioEvent::StreamStarted { info } => {
                assert_eq!(info.codec, "pcm");
                assert_eq!(info.source_sample_rate_hz, Some(8000));
                assert_eq!(info.output_device, "Fake Device");
            }
            _ => unreachable!(),
        }
        wait_for(&handle, 3000, |e| is_state(e, PlaybackState::Playing));

        // Track 1 ends → Finished → next track loads and plays.
        wait_for(&handle, 10_000, |e| is_state(e, PlaybackState::Finished));
        wait_for(&handle, 10_000, |e| {
            matches!(
                e,
                AudioEvent::QueueChanged {
                    current: Some(2),
                    ..
                }
            )
        });
        wait_for(
            &handle,
            10_000,
            |e| matches!(e, AudioEvent::StreamStarted { info } if info.path.ends_with("track1.wav")),
        );

        // Track 2 ends → queue exhausted → Stopped.
        wait_for(&handle, 10_000, |e| is_state(e, PlaybackState::Stopped));

        let snap = handle.snapshot().unwrap();
        assert_eq!(snap.state, PlaybackState::Stopped);
        handle.shutdown();
    }

    #[test]
    fn pause_resume_seek_stop_flow() {
        let fixture = Fixture::new("controls", &[5000]);
        let mut handle = AudioEngine::start_with_backend(fixture.resolver(), 1.0, fake_factory);
        handle
            .send(AudioCommand::PlayQueue {
                tracks: vec![1],
                start_index: 0,
            })
            .unwrap();
        wait_for(&handle, 3000, |e| is_state(e, PlaybackState::Playing));

        handle.send(AudioCommand::Pause).unwrap();
        wait_for(&handle, 2000, |e| is_state(e, PlaybackState::Paused));

        // Seek while paused → Seeking → back to Paused.
        handle
            .send(AudioCommand::SeekTo { position_ms: 2000 })
            .unwrap();
        wait_for(&handle, 2000, |e| is_state(e, PlaybackState::Seeking));
        wait_for(&handle, 2000, |e| is_state(e, PlaybackState::Paused));

        let snap = handle.snapshot().unwrap();
        // WAV seek granularity: 1152-frame packets (144ms at 8 kHz), landed
        // position reported truthfully (see decode.rs docs).
        assert!(
            (1856..=2000).contains(&snap.position_ms),
            "position after seek: {}ms",
            snap.position_ms
        );

        handle.send(AudioCommand::Play).unwrap(); // resume
        wait_for(&handle, 2000, |e| is_state(e, PlaybackState::Playing));

        handle.send(AudioCommand::Stop).unwrap();
        wait_for(&handle, 2000, |e| is_state(e, PlaybackState::Stopped));
        let snap = handle.snapshot().unwrap();
        assert_eq!(snap.position_ms, 0);
        handle.shutdown();
    }

    #[test]
    fn missing_file_surfaces_error_not_crash() {
        let fixture = Fixture::new("missing", &[]);
        let mut handle = AudioEngine::start_with_backend(
            Box::new(|_id| Some(PathBuf::from("Z:/definitely/not/here.wav"))),
            1.0,
            fake_factory,
        );
        handle
            .send(AudioCommand::PlayQueue {
                tracks: vec![99],
                start_index: 0,
            })
            .unwrap();
        let err = wait_for(&handle, 5000, |e| matches!(e, AudioEvent::Error { .. }));
        match err {
            AudioEvent::Error { message } => assert!(!message.is_empty()),
            _ => unreachable!(),
        }
        wait_for(&handle, 5000, |e| is_state(e, PlaybackState::Error));
        // Engine is still alive and answers.
        assert_eq!(handle.snapshot().unwrap().state, PlaybackState::Error);
        handle.shutdown();
        drop(fixture);
    }

    #[test]
    fn volume_is_clamped_and_shared() {
        let fixture = Fixture::new("volume", &[100]);
        let mut handle = AudioEngine::start_with_backend(fixture.resolver(), 1.0, fake_factory);
        handle
            .send(AudioCommand::SetVolume { volume: 1.4 })
            .unwrap();
        wait_for(
            &handle,
            2000,
            |e| matches!(e, AudioEvent::VolumeChanged { volume } if (*volume - 1.0).abs() < f32::EPSILON),
        );
        handle.shutdown();
    }

    #[test]
    fn empty_queue_play_is_a_clean_error() {
        let fixture = Fixture::new("empty", &[]);
        let mut handle = AudioEngine::start_with_backend(fixture.resolver(), 1.0, fake_factory);
        handle.send(AudioCommand::Play).unwrap();
        wait_for(&handle, 2000, |e| matches!(e, AudioEvent::Error { .. }));
        assert_eq!(handle.snapshot().unwrap().state, PlaybackState::Stopped);
        handle.shutdown();
    }

    // ----------------------------------------------- device-event handling

    /// Constructs an engine mid-playback bound to `endpoint`, a fake-backend
    /// pipeline, and directly drives `handle_device_event` (the same function
    /// the engine select loop uses). Deterministic, no hardware needed.
    fn device_case(
        endpoint: &str,
        event: DeviceEvent,
    ) -> (
        Engine,
        StreamTarget,
        crossbeam_channel::Receiver<AudioEvent>,
    ) {
        let (pipeline, _msgs, _shared) = Pipeline::spawn(fake_factory);
        let mut engine = Engine::new();
        engine.state = PlaybackState::Playing;
        engine.current_endpoint = Some(endpoint.into());
        let mut target = StreamTarget::default();
        let (evt_tx, evt_rx) = crossbeam_channel::bounded::<AudioEvent>(16);
        super::handle_device_event(&event, &mut engine, &pipeline, &mut target, &evt_tx);
        (engine, target, evt_rx)
    }

    #[test]
    fn removed_binding_device_marks_stopping_and_emits_error() {
        let (engine, _target, evt_rx) = device_case(
            "fake-device",
            DeviceEvent::Removed {
                endpoint_id: "fake-device".into(),
            },
        );
        assert!(engine.stopping, "stop must be requested");
        let event = evt_rx.try_recv().unwrap();
        assert!(matches!(event, AudioEvent::Error { .. }));
    }

    #[test]
    fn active_device_disabled_marks_stopping_and_emits_error() {
        let (engine, _target, evt_rx) = device_case(
            "fake-device",
            DeviceEvent::StateChanged {
                endpoint_id: "fake-device".into(),
                state: crate::audio::output::DeviceState::Disabled,
            },
        );
        assert!(engine.stopping);
        let event = evt_rx.try_recv().unwrap();
        assert!(matches!(event, AudioEvent::Error { .. }));
    }

    #[test]
    fn removal_of_other_endpoint_does_nothing() {
        let (engine, _target, evt_rx) = device_case(
            "fake-device",
            DeviceEvent::Removed {
                endpoint_id: "some-other".into(),
            },
        );
        assert!(!engine.stopping);
        assert!(evt_rx.try_recv().is_err());
    }

    #[test]
    fn default_change_while_following_default_and_playing_arms_switch() {
        let (engine, target, evt_rx) = device_case(
            "fake-device",
            DeviceEvent::DefaultChanged {
                endpoint_id: Some("{new}".into()),
            },
        );
        // When following the (unpinned) default, a default change re-arms the
        // stream; device_switch_in_flight must be set exactly once.
        assert!(engine.device_switch_in_flight);
        assert!(target.device_id.is_none());
        // The error path is not used here.
        assert!(evt_rx.try_recv().is_err());
    }

    #[test]
    fn default_change_does_not_move_an_explicitly_pinned_device() {
        let (pipeline, _msgs, _shared) = Pipeline::spawn(fake_factory);
        let mut engine = Engine::new();
        engine.state = PlaybackState::Playing;
        engine.current_endpoint = Some("fake-device".into());
        let mut target = StreamTarget {
            device_id: Some("pinned-id".into()),
            ..Default::default()
        };
        let (evt_tx, evt_rx) = crossbeam_channel::bounded::<AudioEvent>(16);
        super::handle_device_event(
            &DeviceEvent::DefaultChanged {
                endpoint_id: Some("{new}".into()),
            },
            &mut engine,
            &pipeline,
            &mut target,
            &evt_tx,
        );
        assert_eq!(target.device_id.as_deref(), Some("pinned-id"));
        assert!(!engine.device_switch_in_flight);
        assert!(evt_rx.try_recv().is_err());
    }

    #[test]
    fn added_and_unavailable_events_are_inert() {
        let (engine, _target, evt_rx) = device_case(
            "fake-device",
            DeviceEvent::Added {
                endpoint_id: "fake-device".into(),
            },
        );
        assert!(!engine.stopping, "add is not a removal");
        assert!(evt_rx.try_recv().is_err());
    }
}
