//! Stream inspection via symphonia — the decoder boundary.
//!
//! Phase 1 uses symphonia as the authority for *stream* properties (codec,
//! sample rate, channels, bit depth, duration) and as a decodability check:
//! a file symphonia cannot probe is treated as corrupt and is not inserted
//! into the library. This is header-level verification; full decode
//! verification happens on first playback (Phase 2).
//!
//! The playback engine (Phase 2) will reuse this boundary for decoding;
//! nothing here depends on the scanner.

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::LibraryError;

/// Technical properties of the decoded PCM stream, as reported by the
/// container/codec. `None` means the container does not say — never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProperties {
    /// Codec identifier, e.g. "flac", "mp3", "pcm", "aac", "vorbis".
    pub codec: String,
    /// Container, derived from the file extension (explicit, documented).
    pub container: String,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bit_depth: Option<u32>,
    pub duration_ms: Option<u64>,
}

/// Codecs LUMEN commits to (see docs/DECISIONS.md ADR-003). Anything else
/// is a scan failure, not a row that would fail at playback time.
///
/// This must stay in step with the `symphonia` features enabled in
/// `crates/core/Cargo.toml` and with `SUPPORTED_EXTENSIONS` in
/// [`crate::library`]: a codec symphonia cannot decode is never named here.
fn codec_name(codec_type: codecs::CodecType) -> Option<&'static str> {
    match codec_type {
        codecs::CODEC_TYPE_FLAC => Some("flac"),
        codecs::CODEC_TYPE_ALAC => Some("alac"),
        codecs::CODEC_TYPE_MP1 => Some("mp1"),
        codecs::CODEC_TYPE_MP2 => Some("mp2"),
        codecs::CODEC_TYPE_MP3 => Some("mp3"),
        codecs::CODEC_TYPE_PCM_S16LE
        | codecs::CODEC_TYPE_PCM_S16BE
        | codecs::CODEC_TYPE_PCM_S24LE
        | codecs::CODEC_TYPE_PCM_S24BE
        | codecs::CODEC_TYPE_PCM_S32LE
        | codecs::CODEC_TYPE_PCM_S32BE
        | codecs::CODEC_TYPE_PCM_U8
        | codecs::CODEC_TYPE_PCM_S8
        | codecs::CODEC_TYPE_PCM_U16LE
        | codecs::CODEC_TYPE_PCM_U16BE
        | codecs::CODEC_TYPE_PCM_U24LE
        | codecs::CODEC_TYPE_PCM_U24BE
        | codecs::CODEC_TYPE_PCM_U32LE
        | codecs::CODEC_TYPE_PCM_U32BE
        | codecs::CODEC_TYPE_PCM_F32LE
        | codecs::CODEC_TYPE_PCM_F32BE
        | codecs::CODEC_TYPE_PCM_F64LE
        | codecs::CODEC_TYPE_PCM_F64BE => Some("pcm"),
        codecs::CODEC_TYPE_ADPCM_G722
        | codecs::CODEC_TYPE_ADPCM_G726
        | codecs::CODEC_TYPE_ADPCM_G726LE
        | codecs::CODEC_TYPE_ADPCM_IMA_QT
        | codecs::CODEC_TYPE_ADPCM_IMA_WAV
        | codecs::CODEC_TYPE_ADPCM_MS => Some("adpcm"),
        codecs::CODEC_TYPE_AAC => Some("aac"),
        codecs::CODEC_TYPE_VORBIS => Some("vorbis"),
        _ => None,
    }
}

/// Probe one file. Returns an error for files symphonia cannot identify or
/// that contain no playable default track — the scanner records these as
/// corrupt and moves on.
pub fn inspect(path: &Path) -> Result<AudioProperties, LibraryError> {
    let container = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let fail = |message: String| LibraryError::Probe {
        path: path.to_path_buf(),
        message,
    };

    let file = File::open(path).map_err(|e| fail(format!("open failed: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    hint.with_extension(&container);

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| fail(format!("probe failed: {e}")))?;

    let format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| fail("no playable audio track".to_string()))?;

    let params = &track.codec_params;

    let codec = codec_name(params.codec)
        .ok_or_else(|| fail(format!("unsupported codec: {}", params.codec)))?
        .to_string();

    let duration_ms = match (params.n_frames, params.time_base) {
        (Some(frames), Some(tb)) => {
            let time = tb.calc_time(frames);
            Some(time.seconds * 1000 + (time.frac * 1000.0).round() as u64)
        }
        _ => None,
    };

    Ok(AudioProperties {
        codec,
        container,
        sample_rate_hz: params.sample_rate,
        channels: params.channels.map(|mask| mask.count() as u16),
        bit_depth: params.bits_per_sample.or(params.bits_per_coded_sample),
        duration_ms,
    })
}
