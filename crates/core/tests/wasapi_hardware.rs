//! Hardware verification: real WASAPI, real default Windows output device.
//!
//! These tests are `#[ignore]` by default — they produce actual sound and
//! depend on machine audio hardware. Run explicitly:
//!
//!   cargo test -p lumen-core --test wasapi_hardware -- --ignored --nocapture
//!
//! What they verify (and what they cannot):
//! - Device enumeration, shared-mode open, format negotiation, event-driven
//!   rendering, underrun-free delivery of a full track, position truthfulness.
//! - They CANNOT verify audibility (that needs a human). Frame delivery and
//!   zero underruns are the strongest machine-checkable playback signals.

#![cfg(windows)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lumen_core::audio::output::{self, OutputMode};
use lumen_core::audio::pipeline::{Pipeline, PipelineCmd, PipelineMsg, StreamTarget};

/// 44.1 kHz stereo 16-bit sine wave (440 Hz) — a format that almost always
/// differs from the 48 kHz mix format, exercising AUTOCONVERTPCM honestly.
fn sine_wav(path: &PathBuf, seconds: f64, freq: f64) {
    let rate = 44_100u32;
    let channels = 2u16;
    let frames = (seconds * f64::from(rate)) as usize;
    let mut pcm = Vec::with_capacity(frames * 2 * 2);
    for i in 0..frames {
        let t = i as f64 / f64::from(rate);
        let sample = (0.25 * (2.0 * std::f64::consts::PI * freq * t).sin() * 32767.0) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
        pcm.extend_from_slice(&sample.to_le_bytes()); // L = R
    }
    let byte_rate = rate * u32::from(channels) * 2;
    let mut w = Vec::new();
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(&pcm);
    let mut file = Vec::new();
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(w.len() as u32).to_le_bytes());
    file.extend_from_slice(&w);
    std::fs::write(path, file).unwrap();
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lumen-hw-{}-{}.wav", name, std::process::id()))
}

#[test]
#[ignore = "hardware: enumerates real audio devices"]
fn enumerate_real_devices() {
    let backend = output::default_backend();
    let devices = backend.enumerate_devices().expect("enumerate failed");
    println!("devices ({}):", devices.len());
    for d in &devices {
        println!(
            "  {} {} [{}]",
            if d.is_default { "*" } else { " " },
            d.name,
            d.id
        );
    }
    assert!(!devices.is_empty(), "no render devices on this machine");
    let default = backend.default_device().unwrap();
    assert!(default.is_some(), "no default render device");
}

#[test]
#[ignore = "hardware: plays audible tone through the default device"]
fn play_sine_through_default_device() {
    let path = temp_path("sine");
    sine_wav(&path, 2.0, 440.0);

    let (mut pipeline, msgs, shared) = Pipeline::spawn(output::default_backend);
    pipeline
        .send(PipelineCmd::Play { path: path.clone() })
        .unwrap();

    let started = wait_msg(&msgs, Duration::from_secs(5), |m| {
        matches!(m, PipelineMsg::Started { .. })
    });
    let PipelineMsg::Started { info } = started else {
        panic!("expected Started, got {started:?}");
    };

    println!("STREAM INFO (the honest playback path):");
    println!(
        "  source     : {} {}Hz/{}ch ({})",
        info.codec,
        info.source_sample_rate_hz.unwrap_or(0),
        info.source_channels.unwrap_or(0),
        info.container
    );
    println!("  decoded    : {}", info.decoded_format);
    println!(
        "  device     : {} [{:?}]",
        info.output_device, info.output_mode
    );
    println!(
        "  negotiated : {}Hz/{}ch",
        info.output_sample_rate_hz, info.output_channels
    );
    if let (Some(rate), Some(ch)) = (info.mix_sample_rate_hz, info.mix_channels) {
        println!("  mix format : {rate}Hz/{ch}ch");
    }
    println!("  conversion : {}", info.conversion);

    assert_eq!(info.source_sample_rate_hz, Some(44_100));
    assert_eq!(info.output_mode, OutputMode::Shared);

    // Wait for the track to finish (2 s of audio + margin).
    let ended = wait_msg(&msgs, Duration::from_secs(8), |m| {
        matches!(m, PipelineMsg::Ended)
    });
    assert!(matches!(ended, PipelineMsg::Ended), "got {ended:?}");

    let position_ms = shared.position_ms();
    let underruns = shared
        .underrun_frames
        .load(std::sync::atomic::Ordering::Relaxed);
    println!("  position   : {position_ms}ms of ~2000ms");
    println!("  underruns  : {underruns} frames");

    assert!(
        (1800..=2400).contains(&position_ms),
        "device timeline should track the track length, got {position_ms}ms"
    );
    assert_eq!(underruns, 0, "no underruns expected on a local sine file");

    pipeline.shutdown();
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore = "hardware: pause/stop/seek against the real device"]
fn transport_against_real_device() {
    let path = temp_path("transport");
    sine_wav(&path, 10.0, 330.0);

    let (mut pipeline, msgs, shared) = Pipeline::spawn(output::default_backend);
    pipeline
        .send(PipelineCmd::Play { path: path.clone() })
        .unwrap();
    wait_msg(&msgs, Duration::from_secs(5), |m| {
        matches!(m, PipelineMsg::Started { .. })
    });

    std::thread::sleep(Duration::from_millis(800));
    pipeline.send(PipelineCmd::Pause).unwrap();
    // Let the pipeline process the pause before sampling: frames written in
    // the (async) command window still count — that is correct behavior.
    std::thread::sleep(Duration::from_millis(300));
    let pos_at_pause = shared.position_ms();
    std::thread::sleep(Duration::from_millis(500));
    let pos_after_pause = shared.position_ms();
    assert_eq!(
        pos_at_pause, pos_after_pause,
        "position must freeze while paused"
    );

    pipeline.send(PipelineCmd::Resume).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        shared.position_ms() > pos_at_pause,
        "position must advance after resume"
    );

    pipeline
        .send(PipelineCmd::Seek { position_ms: 5000 })
        .unwrap();
    let seeked = wait_msg(&msgs, Duration::from_secs(3), |m| {
        matches!(m, PipelineMsg::SeekApplied { .. })
    });
    match seeked {
        PipelineMsg::SeekApplied { position_ms } => {
            assert!(
                (4900..=5100).contains(&position_ms),
                "seek landed at {position_ms}ms"
            );
        }
        _ => panic!("expected SeekApplied, got {seeked:?}"),
    }

    pipeline.send(PipelineCmd::Stop).unwrap();
    pipeline.shutdown();
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore = "hardware: probes each render device for real exclusive support"]
fn exclusive_probe_each_device() {
    let backend = output::default_backend();
    let devices = backend.enumerate_devices().expect("enumerate failed");
    println!("probing {} devices for exclusive support:", devices.len());
    for d in &devices {
        let mut backend = output::default_backend();
        let request = lumen_core::audio::output::StreamRequest {
            mode: OutputMode::Exclusive,
            format: Some(lumen_core::audio::output::OutputFormat {
                sample_rate_hz: 48_000,
                channels: 2,
                sample_format: lumen_core::audio::output::SampleFormat::F32,
            }),
        };
        match backend.open_stream(Some(&d.id), &request) {
            Ok(mut s) => {
                let fmt = s.negotiated_format();
                println!(
                    "  OK   {} — opened {:?} {}Hz/{}ch {:?}",
                    d.name,
                    s.mode(),
                    fmt.sample_rate_hz,
                    fmt.channels,
                    fmt.sample_format
                );
                let _ = s.start();
                let _ = s.stop();
            }
            Err(e) => println!("  FAIL {} — {}", d.name, e),
        }
    }
}

#[test]
#[ignore = "hardware: attempts a real exclusive-mode open + negotiation"]
fn exclusive_mode_attempt() {
    let path = temp_path("exclusive");
    sine_wav(&path, 2.0, 440.0);

    let target = StreamTarget {
        device_id: None,
        mode: OutputMode::Exclusive,
    };
    let (mut pipeline, msgs, shared) = Pipeline::spawn_configured(output::default_backend, target);
    pipeline
        .send(PipelineCmd::Play { path: path.clone() })
        .unwrap();

    let started = wait_msg(&msgs, Duration::from_secs(5), |m| {
        matches!(m, PipelineMsg::Started { .. })
    });
    let PipelineMsg::Started { info } = started else {
        panic!("expected Started, got {started:?}");
    };

    println!("EXCLUSIVE-MODE ATTEMPT:");
    println!("  requested    : {:?}", info.requested_mode);
    println!("  actual mode  : {:?}", info.output_mode);
    println!("  device       : {}", info.output_device);
    println!(
        "  source       : {}Hz/{}ch ({})",
        info.source_sample_rate_hz.unwrap_or(0),
        info.source_channels.unwrap_or(0),
        info.codec
    );
    println!(
        "  negotiated   : {}Hz/{}ch {:?}",
        info.output_sample_rate_hz, info.output_channels, info.output_sample_format
    );
    println!("  conversion   : {}", info.conversion);

    // Honest outcome: either exclusive opened with our source rate/channels,
    // or we fell back to shared and the info says so (requested=Exclusive,
    // output_mode=Shared). We never resample in this path.
    if info.output_mode == OutputMode::Exclusive {
        assert_eq!(
            info.output_sample_rate_hz,
            info.source_sample_rate_hz.unwrap()
        );
        assert_eq!(info.output_channels, info.source_channels.unwrap());
    } else {
        assert_eq!(info.requested_mode, OutputMode::Exclusive);
        assert_eq!(info.output_mode, OutputMode::Shared);
    }

    let _ = wait_msg(&msgs, Duration::from_secs(8), |m| {
        matches!(m, PipelineMsg::Ended)
    });
    let underruns = shared
        .underrun_frames
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(underruns, 0, "no underruns expected");

    pipeline.shutdown();
    let _ = std::fs::remove_file(path);
}

fn wait_msg(
    msgs: &crossbeam_channel::Receiver<PipelineMsg>,
    timeout: Duration,
    pred: impl Fn(&PipelineMsg) -> bool,
) -> PipelineMsg {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        assert!(remaining > Duration::ZERO, "timed out waiting for message");
        let msg = msgs.recv_timeout(remaining).expect("message");
        if let PipelineMsg::Error { message } = &msg {
            panic!("pipeline error while waiting: {message}");
        }
        if pred(&msg) {
            return msg;
        }
    }
}
