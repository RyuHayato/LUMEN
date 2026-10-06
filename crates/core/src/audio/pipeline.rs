//! The audio pipeline thread: decoder → FIFO → output stream.
//!
//! Design (ADR-014): ONE thread owns the decoder, the PCM FIFO, and the
//! output stream. There are no locks and no cross-thread buffer handoffs in
//! the data plane — control arrives on a channel, facts leave as atomics +
//! messages. Because nothing is shared, seek is trivially race-free: the
//! seek command is applied between render cycles and the FIFO is cleared
//! before any post-seek frame is decoded.
//!
//! Timing model: the WASAPI event drives the cadence. After each wake the
//! thread tops the FIFO up to ~2× the device buffer, then writes what the
//! device can accept. The FIFO absorbs decode-time stalls (page faults,
//! AV scanning); if it ever runs dry while the decoder is alive, the
//! shortfall is written as silence and counted in `underrun_frames`.
//!
//! Volume: linear application gain applied at write time (see docs — LUMEN
//! controls application gain only, never system/DAC volume).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serde::Serialize;

use crate::audio::decode::TrackDecoder;
use crate::audio::output::{
    OutputBackend, OutputFormat, OutputMode, OutputStream, SampleFormat, StreamRequest,
};
use crate::error::AudioError;

/// Commands from the engine (control plane).
#[derive(Debug)]
pub enum PipelineCmd {
    Play {
        path: PathBuf,
    },
    Pause,
    Resume,
    Seek {
        position_ms: u64,
    },
    Stop,
    /// Turn gapless transitions on or off. Off restores the previous behaviour
    /// exactly: a stop, a silence fill, and a fresh open at each boundary.
    SetGapless {
        enabled: bool,
    },
    /// Open the upcoming track's decoder ahead of time, on a worker thread.
    ///
    /// This is what makes gapless possible. At end of stream the device is
    /// still holding the tail of the current track, so the pipeline swaps in an
    /// already-open decoder and keeps writing to the *same* running stream. The
    /// expensive part - opening the file and probing the format - has already
    /// happened by then, so the swap costs microseconds and nothing is missed.
    ///
    /// `None` cancels any pending prefetch, which is what every user command
    /// that invalidates the future does.
    Prefetch {
        path: Option<PathBuf>,
    },
    /// Re-open the current track's stream onto a different endpoint.
    SwitchDevice {
        device_id: Option<String>,
    },
    /// Change shared/exclusive mode, re-opening the active stream if any.
    SetOutputMode {
        mode: OutputMode,
    },
    Shutdown,
}

/// Diagnostics for one opened stream — the honest description of the real
/// playback path. Shown in the UI; never marketing.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    pub path: String,
    pub codec: String,
    pub container: String,
    pub source_sample_rate_hz: Option<u32>,
    pub source_channels: Option<u16>,
    pub source_bit_depth: Option<u32>,
    pub duration_ms: Option<u64>,
    pub decoded_format: String,
    pub output_device: String,
    pub output_endpoint_id: String,
    /// The mode the engine asked for. Differs from `output_mode` when
    /// exclusive was requested and negotiation fell back to shared —
    /// this pairing is how the UI tells the truth about what happened.
    pub requested_mode: OutputMode,
    /// The mode the stream actually opened in.
    pub output_mode: OutputMode,
    pub output_sample_rate_hz: u32,
    pub output_channels: u16,
    pub output_sample_format: SampleFormat,
    pub mix_sample_rate_hz: Option<u32>,
    pub mix_channels: Option<u16>,
    pub conversion: String,
}

/// Facts the pipeline reports back to the engine.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum PipelineMsg {
    Started {
        info: StreamInfo,
    },
    SeekApplied {
        position_ms: u64,
    },
    /// Stop completed: stream torn down, position zeroed. The engine must
    /// wait for this before reporting `Stopped` — otherwise snapshots can
    /// observe in-flight frames (ack-ordering guarantee).
    Stopped,
    /// Decoder reached end of stream and the FIFO has fully drained.
    Ended,
    /// A prefetched decoder was taken over mid-stream, without stopping or
    /// re-opening the output stream. The engine should advance its queue by one
    /// (the same step `Ended` would have caused) but must **not** issue another
    /// `Play` - the pipeline is already playing this track.
    Advanced {
        info: StreamInfo,
    },
    Error {
        message: String,
    },
}

/// Which endpoint the pipeline should open streams on. `None` follows the
/// system default *at open time* (resolved by the backend).
#[derive(Debug, Clone, Default)]
pub struct StreamTarget {
    pub device_id: Option<String>,
    pub mode: OutputMode,
}

/// Lock-free state shared with the engine/UI.
pub struct PipelineShared {
    /// Frames delivered to the device since Play (incl. underrun silence:
    /// this is the audible timeline).
    pub position_frames: AtomicU64,
    pub sample_rate_hz: AtomicU32,
    pub underrun_frames: AtomicU64,
    /// Volume as f32 bits (linear gain, 0.0..=1.0).
    pub volume_bits: AtomicU32,
}

impl PipelineShared {
    pub fn position_ms(&self) -> u64 {
        let rate = self.sample_rate_hz.load(Ordering::Relaxed);
        if rate == 0 {
            return 0;
        }
        self.position_frames.load(Ordering::Relaxed) * 1000 / u64::from(rate)
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }
}

/// Factory for the platform backend (injected so tests can substitute a
/// fake; production passes [`crate::audio::output::default_backend`]).
pub type BackendFactory = fn() -> Box<dyn OutputBackend>;

pub struct Pipeline {
    commands: Sender<PipelineCmd>,
    thread: Option<JoinHandle<()>>,
}

impl Pipeline {
    /// Spawn the pipeline thread with the default target (system default
    /// device, shared mode).
    pub fn spawn(
        backend_factory: BackendFactory,
    ) -> (Self, Receiver<PipelineMsg>, Arc<PipelineShared>) {
        Self::spawn_configured(backend_factory, StreamTarget::default())
    }

    /// Spawn the pipeline thread with an explicit device/mode target. The
    /// backend is created *inside* the thread (COM/apartment rules); stream
    /// creation is lazy, on first Play.
    pub fn spawn_configured(
        backend_factory: BackendFactory,
        target: StreamTarget,
    ) -> (Self, Receiver<PipelineMsg>, Arc<PipelineShared>) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<PipelineCmd>(32);
        let (msg_tx, msg_rx) = crossbeam_channel::bounded::<PipelineMsg>(32);
        let shared = Arc::new(PipelineShared {
            position_frames: AtomicU64::new(0),
            sample_rate_hz: AtomicU32::new(0),
            underrun_frames: AtomicU64::new(0),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
        });

        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("lumen-audio-pipeline".into())
            .spawn(move || run(cmd_rx, msg_tx, thread_shared, backend_factory, target))
            .expect("failed to spawn lumen-audio-pipeline thread");

        (
            Self {
                commands: cmd_tx,
                thread: Some(thread),
            },
            msg_rx,
            shared,
        )
    }

    pub fn send(&self, cmd: PipelineCmd) -> Result<(), AudioError> {
        self.commands.try_send(cmd).map_err(|_| {
            AudioError::BackendUnavailable("pipeline command queue full/closed".into())
        })
    }

    pub fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.commands.send(PipelineCmd::Shutdown);
            let _ = thread.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ------------------------------------------------------------ thread internals

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayerState {
    Idle,
    Playing,
    Paused,
}

struct Active {
    path: PathBuf,
    decoder: TrackDecoder,
    stream: Box<dyn OutputStream>,
    stream_format: OutputFormat,
    state: PlayerState,
    eos: bool,
    /// The mode the live stream was opened with, kept so a gapless takeover can
    /// describe the path honestly in its `StreamInfo`.
    requested_mode: OutputMode,
}

/// The upcoming track's decoder, being opened on a worker thread.
///
/// `TrackDecoder` is `Send` (symphonia's reader traits carry the bound), which
/// is what makes this possible at all; it is asserted by
/// `decode::tests::decoder_is_send`.
struct Prefetch {
    path: PathBuf,
    rx: crossbeam_channel::Receiver<Result<TrackDecoder, String>>,
}

/// Spawn a worker that opens `path` and hands the decoder back.
///
/// The thread is detached: if the pipeline stops caring before the open
/// finishes, the result is simply dropped when the receiver goes away. Opening a
/// decoder does no audio work and holds no locks, so an abandoned open is
/// harmless.
fn start_prefetch(path: PathBuf) -> Prefetch {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let open_path = path.clone();
    let _ = thread::Builder::new()
        .name("lumen-audio-prefetch".into())
        .spawn(move || {
            let _ = tx.send(TrackDecoder::open(&open_path).map_err(|e| e.to_string()));
        });
    Prefetch { path, rx }
}

/// Take a finished prefetch, but only if its output format matches the live
/// stream.
///
/// A different sample rate or channel count would need a resampler the pipeline
/// does not have, so a mismatch is not papered over with silence - the caller
/// falls back to the ordinary stop/open path and the user hears an honest gap
/// rather than a subtly wrong track.
fn take_usable_prefetch(
    prefetch: &mut Option<Prefetch>,
    live: OutputFormat,
) -> Option<(PathBuf, TrackDecoder)> {
    let p = prefetch.as_ref()?;
    let decoded = match p.rx.try_recv() {
        Ok(result) => result,
        Err(_) => return None, // still opening, or its worker died
    };
    let (path, decoder) = (p.path.clone(), decoded.ok()?);
    if decoder_source_format(&decoder) != live {
        return None;
    }
    *prefetch = None;
    Some((path, decoder))
}

/// FIFO target: ~2× the device buffer, capped, per stream open.
fn fifo_target(format: &OutputFormat, buffer_frames: usize) -> usize {
    let per_ms = format.sample_rate_hz as usize / 1000 * format.channels as usize;
    (per_ms * 60).max(buffer_frames * format.channels as usize * 2)
}

#[allow(clippy::too_many_arguments)]
fn run(
    commands: Receiver<PipelineCmd>,
    messages: Sender<PipelineMsg>,
    shared: Arc<PipelineShared>,
    backend_factory: BackendFactory,
    mut stream_target: StreamTarget,
) {
    let mut backend = backend_factory();
    let mut active: Option<Active> = None;
    let mut fifo: VecDeque<f32> = VecDeque::new();
    let mut block: Vec<f32> = Vec::new();
    let mut write_buf: Vec<f32> = Vec::new();
    let mut target = 0usize;
    let mut prefetch: Option<Prefetch> = None;
    let mut gapless = true;

    tracing::info!(
        mode = ?stream_target.mode,
        device = ?stream_target.device_id,
        "audio pipeline started"
    );

    loop {
        // Render cycle while playing.
        if let Some(a) = active.as_mut() {
            if matches!(a.state, PlayerState::Playing) {
                // Fill the FIFO, and keep filling across a gapless handover.
                //
                // The loop matters. After swapping decoders the FIFO still holds
                // the *previous* track's tail, which must be discarded, and the
                // new decoder has to refill it before the shortfall below is
                // computed. Refilling outside the loop left the FIFO empty for
                // one cycle, which the pipeline then "corrected" by writing a
                // buffer of silence - the very gap this exists to remove.
                'fill: loop {
                    while fifo.len() < target && !a.eos {
                        match a.decoder.decode_next(&mut block) {
                            Ok(Some(_)) => fifo.extend(block.iter().copied()),
                            Ok(None) => a.eos = true,
                            Err(e) => {
                                send_msg(
                                    &messages,
                                    PipelineMsg::Error {
                                        message: e.to_string(),
                                    },
                                );
                                a.eos = true;
                            }
                        }
                    }

                    // Not spent, or gapless is off: nothing to hand over to.
                    if !a.eos || !gapless {
                        break 'fill;
                    }
                    // Spent. Take the prefetched decoder only if it is ready and
                    // matches the live stream; otherwise fall through to the
                    // ordinary end-of-track path below.
                    let Some((next_path, next_decoder)) =
                        take_usable_prefetch(&mut prefetch, a.stream_format)
                    else {
                        break 'fill;
                    };

                    let info = build_stream_info(
                        &next_path,
                        &next_decoder,
                        a.stream.as_ref(),
                        a.requested_mode,
                    );
                    fifo.clear();
                    a.path = next_path;
                    a.decoder = next_decoder;
                    a.eos = false;
                    a.state = PlayerState::Playing;
                    shared.position_frames.store(0, Ordering::Relaxed);
                    send_msg(&messages, PipelineMsg::Advanced { info });
                    // Loop once more, to refill from the new decoder.
                }
                let writable = match a.stream.writable_frames() {
                    Ok(w) => w,
                    Err(e) => {
                        send_msg(
                            &messages,
                            PipelineMsg::Error {
                                message: e.to_string(),
                            },
                        );
                        teardown(&mut active);
                        continue;
                    }
                };

                let channels = a.stream_format.channels as usize;
                let take_frames = writable.min(fifo.len() / channels);
                if take_frames > 0 {
                    write_buf.clear();
                    write_buf.extend(fifo.drain(..take_frames * channels));
                    let gain = shared.volume();
                    if (gain - 1.0).abs() > f32::EPSILON {
                        for s in write_buf.iter_mut() {
                            *s *= gain;
                        }
                    }
                    match a.stream.write(&write_buf) {
                        Ok(written) => {
                            shared
                                .position_frames
                                .fetch_add(written as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            send_msg(
                                &messages,
                                PipelineMsg::Error {
                                    message: e.to_string(),
                                },
                            );
                            teardown(&mut active);
                            continue;
                        }
                    }
                }

                let shortfall = writable - take_frames;
                if shortfall > 0 {
                    if a.eos {
                        // Track finished and FIFO drained: silence is the
                        // transition gap (documented; no gapless yet).
                        let _ = a.stream.write_silence(shortfall);
                        shared
                            .position_frames
                            .fetch_add(shortfall as u64, Ordering::Relaxed);
                        send_msg(&messages, PipelineMsg::Ended);
                        if let Some(a) = active.as_mut() {
                            a.state = PlayerState::Idle;
                            let _ = a.stream.stop();
                        }
                    } else {
                        // Decoder alive but FIFO starved: real underrun.
                        shared
                            .underrun_frames
                            .fetch_add(shortfall as u64, Ordering::Relaxed);
                        let _ = a.stream.write_silence(shortfall);
                        shared
                            .position_frames
                            .fetch_add(shortfall as u64, Ordering::Relaxed);
                    }
                }

                // Handle pending commands without blocking (render cadence
                // is owned by the device event).
                match commands.try_recv() {
                    Ok(cmd) => {
                        if handle_cmd(
                            cmd,
                            &mut active,
                            &mut fifo,
                            &mut target,
                            &mut prefetch,
                            &mut gapless,
                            &shared,
                            &messages,
                            backend.as_mut(),
                            &mut stream_target,
                        ) {
                            break; // Shutdown
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }

                // Wait for the next device cycle. Timeout keeps command
                // latency bounded even if the event is never signaled.
                if let Some(a) = active.as_mut() {
                    if matches!(a.state, PlayerState::Playing) {
                        let _ = a.stream.wait_ready(Duration::from_millis(50));
                    }
                }
                continue;
            }
        }

        // Idle or paused: block on commands.
        match commands.recv() {
            Ok(cmd) => {
                if handle_cmd(
                    cmd,
                    &mut active,
                    &mut fifo,
                    &mut target,
                    &mut prefetch,
                    &mut gapless,
                    &shared,
                    &messages,
                    backend.as_mut(),
                    &mut stream_target,
                ) {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    teardown(&mut active);
    tracing::info!("audio pipeline stopped");
}

fn send_msg(messages: &Sender<PipelineMsg>, msg: PipelineMsg) {
    // Terminal facts must not be lost; the engine always drains promptly.
    let _ = messages.send(msg);
}

fn teardown(active: &mut Option<Active>) {
    if let Some(mut a) = active.take() {
        let _ = a.stream.stop();
    }
}

/// Handle one command. Returns `true` only for Shutdown.
#[allow(clippy::too_many_arguments)]
fn handle_cmd(
    cmd: PipelineCmd,
    active: &mut Option<Active>,
    fifo: &mut VecDeque<f32>,
    target: &mut usize,
    prefetch: &mut Option<Prefetch>,
    gapless: &mut bool,
    shared: &Arc<PipelineShared>,
    messages: &Sender<PipelineMsg>,
    backend: &mut dyn OutputBackend,
    stream_target: &mut StreamTarget,
) -> bool {
    match cmd {
        PipelineCmd::Prefetch { path } => {
            *prefetch = path.map(start_prefetch);
        }
        PipelineCmd::Play { path } => {
            // An explicit Play supersedes whatever was being read ahead.
            *prefetch = None;
            fifo.clear();
            shared.position_frames.store(0, Ordering::Relaxed);
            shared.underrun_frames.store(0, Ordering::Relaxed);
            match open_track(backend, stream_target, path, active) {
                Ok((info, fmt)) => {
                    *target = fifo_target(&fmt, 0);
                    shared
                        .sample_rate_hz
                        .store(fmt.sample_rate_hz, Ordering::Relaxed);
                    send_msg(messages, PipelineMsg::Started { info });
                }
                Err(e) => {
                    send_msg(
                        messages,
                        PipelineMsg::Error {
                            message: e.to_string(),
                        },
                    );
                }
            }
        }
        PipelineCmd::Pause => {
            if let Some(a) = active.as_mut() {
                if matches!(a.state, PlayerState::Playing) {
                    let _ = a.stream.stop();
                    a.state = PlayerState::Paused;
                }
            }
        }
        PipelineCmd::Resume => {
            if let Some(a) = active.as_mut() {
                if matches!(a.state, PlayerState::Paused) {
                    match a.stream.start() {
                        Ok(()) => a.state = PlayerState::Playing,
                        Err(e) => send_msg(
                            messages,
                            PipelineMsg::Error {
                                message: e.to_string(),
                            },
                        ),
                    }
                }
            }
        }
        PipelineCmd::SetGapless { enabled } => {
            *gapless = enabled;
            if !*gapless {
                *prefetch = None;
            }
        }
        PipelineCmd::Seek { position_ms } => {
            // Seeking the current track leaves the upcoming one untouched, but a
            // prefetch that was read ahead against the old timeline is no longer
            // trustworthy, so it is dropped.
            *prefetch = None;
            if let Some(a) = active.as_mut() {
                match a.decoder.seek(position_ms) {
                    Ok(actual) => {
                        // Single-owner guarantee: no post-seek frame is
                        // decoded before the FIFO is cleared.
                        fifo.clear();
                        let rate = a.stream_format.sample_rate_hz;
                        shared
                            .position_frames
                            .store(actual * u64::from(rate) / 1000, Ordering::Relaxed);
                        a.eos = false;
                        send_msg(
                            messages,
                            PipelineMsg::SeekApplied {
                                position_ms: actual,
                            },
                        );
                    }
                    Err(e) => send_msg(
                        messages,
                        PipelineMsg::Error {
                            message: e.to_string(),
                        },
                    ),
                }
            }
        }
        PipelineCmd::SetOutputMode { mode } => {
            stream_target.mode = mode;
            // If a stream is active, re-open it in the new mode while keeping
            // the track/position (same code path as SwitchDevice because the
            // endpoint may now negotiate differently).
            if active
                .as_ref()
                .is_some_and(|a| matches!(a.state, PlayerState::Playing | PlayerState::Paused))
            {
                return handle_cmd(
                    PipelineCmd::SwitchDevice {
                        device_id: stream_target.device_id.clone(),
                    },
                    active,
                    fifo,
                    target,
                    prefetch,
                    gapless,
                    shared,
                    messages,
                    backend,
                    stream_target,
                );
            }
        }
        PipelineCmd::SwitchDevice { device_id } => {
            // Device change / explicit selection: rebuild the output stream on
            // the new endpoint while keeping the track, decoder position, and
            // play state. Position is re-based from the decoder's seek result,
            // independent of the new stream's clock.
            stream_target.device_id = device_id;
            // The new endpoint may negotiate a different format, so anything
            // read ahead against the old one is discarded.
            *prefetch = None;
            let Some(mut a) = active.take() else {
                return false;
            };
            let was_playing = matches!(a.state, PlayerState::Playing);
            let position_ms = shared.position_ms();

            // Stop the old stream but keep the decoder; re-seek it to the
            // current position rather than restarting the track.
            let _ = a.stream.stop();

            let source = decoder_source_format(&a.decoder);
            if let Err(e) = a.decoder.seek(position_ms) {
                send_msg(
                    messages,
                    PipelineMsg::Error {
                        message: format!("device switch: seek failed ({e})"),
                    },
                );
                *active = Some(a);
                return false;
            }
            fifo.clear();

            let (mut stream, requested_mode) =
                match open_output_stream(backend, stream_target, &source) {
                    Ok(pair) => pair,
                    Err(e) => {
                        send_msg(
                            messages,
                            PipelineMsg::Error {
                                message: format!("device switch failed: {e}"),
                            },
                        );
                        *active = Some(a);
                        return false;
                    }
                };

            if was_playing {
                if let Err(e) = stream.start() {
                    send_msg(
                        messages,
                        PipelineMsg::Error {
                            message: format!("device switch: start failed ({e})"),
                        },
                    );
                    *active = Some(a);
                    return false;
                }
            }

            let new_fmt = stream.negotiated_format();
            *target = fifo_target(&new_fmt, 0);
            shared
                .sample_rate_hz
                .store(new_fmt.sample_rate_hz, Ordering::Relaxed);
            shared.position_frames.store(
                position_ms * u64::from(new_fmt.sample_rate_hz) / 1000,
                Ordering::Relaxed,
            );

            a.stream_format = new_fmt;
            a.stream = stream;
            a.state = if was_playing {
                PlayerState::Playing
            } else {
                PlayerState::Paused
            };
            a.eos = false;
            let info = build_stream_info(&a.path, &a.decoder, a.stream.as_ref(), requested_mode);
            send_msg(messages, PipelineMsg::Started { info });
            *active = Some(a);
        }
        PipelineCmd::Stop => {
            *prefetch = None;
            fifo.clear();
            teardown(active);
            shared.position_frames.store(0, Ordering::Relaxed);
            send_msg(messages, PipelineMsg::Stopped);
        }
        PipelineCmd::Shutdown => {
            *prefetch = None;
            teardown(active);
            return true;
        }
    }
    false
}

/// Decode the source PCM format the decoder advertises (always f32).
fn decoder_source_format(decoder: &TrackDecoder) -> OutputFormat {
    let props = decoder.properties();
    OutputFormat {
        sample_rate_hz: props.sample_rate_hz.unwrap_or(44_100),
        channels: props.channels.unwrap_or(2),
        sample_format: SampleFormat::F32,
    }
}

/// Build [`StreamInfo`] from a just-opened stream.
fn build_stream_info(
    path: &std::path::Path,
    decoder: &TrackDecoder,
    stream: &dyn OutputStream,
    requested_mode: OutputMode,
) -> StreamInfo {
    let props = decoder.properties().clone();
    let fmt = stream.negotiated_format();
    StreamInfo {
        path: path.display().to_string(),
        codec: props.codec.clone(),
        container: props.container.clone(),
        source_sample_rate_hz: props.sample_rate_hz,
        source_channels: props.channels,
        source_bit_depth: props.bit_depth,
        duration_ms: props.duration_ms,
        decoded_format: "f32 interleaved".to_string(),
        output_device: stream.device_name().to_string(),
        output_endpoint_id: stream.endpoint_id().to_string(),
        requested_mode,
        output_mode: stream.mode(),
        output_sample_rate_hz: fmt.sample_rate_hz,
        output_channels: fmt.channels,
        output_sample_format: fmt.sample_format,
        mix_sample_rate_hz: stream.mix_format().map(|f| f.sample_rate_hz),
        mix_channels: stream.mix_format().map(|f| f.channels),
        conversion: stream.conversion_description(),
    }
}

/// Open an output stream, applying the mode policy:
/// - Requested `Shared` → open shared; failure is an error.
/// - Requested `Exclusive` → try exclusive; if negotiation fails, fall back
///   to shared mode and report `(requested_mode = Exclusive, actual = Shared)`;
///   if both fail, the stream open is an error.
///
/// Returns `(stream, requested_mode)`.
fn open_output_stream(
    backend: &mut dyn OutputBackend,
    stream_target: &StreamTarget,
    source: &OutputFormat,
) -> Result<(Box<dyn OutputStream>, OutputMode), AudioError> {
    match stream_target.mode {
        OutputMode::Shared => {
            let request = StreamRequest {
                mode: OutputMode::Shared,
                format: Some(*source),
            };
            let stream = backend.open_stream(stream_target.device_id.as_deref(), &request)?;
            Ok((stream, OutputMode::Shared))
        }
        OutputMode::Exclusive => {
            let request = StreamRequest {
                mode: OutputMode::Exclusive,
                format: Some(*source),
            };
            match backend.open_stream(stream_target.device_id.as_deref(), &request) {
                Ok(stream) => Ok((stream, OutputMode::Exclusive)),
                Err(exclusive_err) => {
                    tracing::info!(
                        "exclusive mode unavailable ({exclusive_err}); falling back to shared mode"
                    );
                    let fallback = StreamRequest {
                        mode: OutputMode::Shared,
                        format: Some(*source),
                    };
                    let stream = backend
                        .open_stream(stream_target.device_id.as_deref(), &fallback)
                        .map_err(|shared_err| {
                            AudioError::BackendUnavailable(format!(
                                "exclusive open failed ({exclusive_err}) and shared fallback also \
                                 failed: {shared_err}"
                            ))
                        })?;
                    Ok((stream, OutputMode::Exclusive))
                }
            }
        }
    }
}

/// Open a track: decoder first, then an output stream for the source format.
fn open_track(
    backend: &mut dyn OutputBackend,
    stream_target: &StreamTarget,
    path: PathBuf,
    active: &mut Option<Active>,
) -> Result<(StreamInfo, OutputFormat), AudioError> {
    let decoder = TrackDecoder::open(&path)?;
    let source = decoder_source_format(&decoder);

    // Reuse the live stream when the target endpoint and format are unchanged
    // and we are already playing in the requested mode.
    let reuse = active
        .as_ref()
        .map(|a| a.stream_format == source && a.state != PlayerState::Idle)
        .unwrap_or(false);

    let requested_mode = stream_target.mode;
    let stream = if reuse {
        let mut a = active.take().expect("checked above");
        let _ = a.stream.stop();
        a.stream.start()?;
        a.stream
    } else {
        let (stream, _requested) = open_output_stream(backend, stream_target, &source)?;
        let mut stream = stream;
        stream.start()?;
        stream
    };

    let info = build_stream_info(&path, &decoder, stream.as_ref(), requested_mode);
    let fmt = stream.negotiated_format();
    *active = Some(Active {
        path,
        decoder,
        stream,
        stream_format: fmt,
        state: PlayerState::Playing,
        eos: false,
        requested_mode,
    });
    Ok((info, fmt))
}
