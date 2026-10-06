//! Track decoding via symphonia: file → interleaved f32 PCM blocks.
//!
//! This is the decode half of the audio pipeline. It is deliberately
//! independent of output: the pipeline pulls one packet at a time
//! (streaming — never whole-file), converts to interleaved f32, and tracks
//! the decoded position from container timestamps.
//!
//! Error policy (per-file, never fatal to the app):
//! - open/probe failures → `AudioError`
//! - corrupt packets mid-stream → skipped (decode continues)
//! - end of stream / truncated file → clean `Ok(None)`

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::TimeBase;

use crate::error::AudioError;
use crate::library::probe::{self, AudioProperties};

/// One track being decoded. Owns the demuxer/decoder pair.
pub struct TrackDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: TimeBase,
    props: AudioProperties,
    channels: usize,
}

impl TrackDecoder {
    /// Open a track file. Verifies decodability (probe + codec) exactly like
    /// the library scanner does, then builds the streaming decoder.
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        // Reuse the library's probe for properties + supported-codec policy.
        let props = probe::inspect(path).map_err(|e| match e {
            crate::error::LibraryError::Probe { message, .. } => {
                AudioError::UnsupportedFormat(message)
            }
            other => AudioError::BackendUnavailable(other.to_string()),
        })?;

        let file = File::open(path)
            .map_err(|e| AudioError::DeviceUnavailable(format!("open {}: {e}", path.display())))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(&ext.to_ascii_lowercase());
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|e| AudioError::UnsupportedFormat(format!("probe failed: {e}")))?;

        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| AudioError::UnsupportedFormat("no playable audio track".into()))?;
        let track_id = track.id;
        let time_base = track
            .codec_params
            .time_base
            .unwrap_or(TimeBase::new(1, props.sample_rate_hz.unwrap_or(44_100)));

        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| AudioError::UnsupportedFormat(format!("codec init failed: {e}")))?;

        let channels = track.codec_params.channels.map(|m| m.count()).unwrap_or(2);

        Ok(Self {
            format,
            decoder,
            track_id,
            time_base,
            props,
            channels,
        })
    }

    pub fn properties(&self) -> &AudioProperties {
        &self.props
    }

    /// Decode the next packet into interleaved f32, appended to `out`
    /// (which is cleared first). Returns frames decoded this call, or
    /// `Ok(None)` at end of stream. Corrupt packets are skipped.
    pub fn decode_next(&mut self, out: &mut Vec<f32>) -> Result<Option<usize>, AudioError> {
        out.clear();
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None); // clean end of stream (or truncated file)
                }
                Err(e) => return Err(AudioError::BackendUnavailable(format!("demux failed: {e}"))),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(audio) => {
                    let frames = audio.frames();
                    if frames == 0 {
                        continue;
                    }
                    let mut buf = SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
                    buf.copy_interleaved_ref(audio);
                    out.extend_from_slice(buf.samples());
                    return Ok(Some(frames));
                }
                Err(SymphoniaError::DecodeError(_)) => {
                    // Corrupt packet: skip, keep decoding the rest.
                    continue;
                }
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(AudioError::BackendUnavailable(format!(
                        "decode failed: {e}"
                    )))
                }
            }
        }
    }

    /// Seek to `position_ms`. Returns the position actually landed on.
    ///
    /// Seek granularity is container/codec-dependent and never guessed:
    /// WAV/PCM lands on symphonia's 1152-frame simulated-packet boundaries
    /// (at or before the target); MP3/AAC land on frame boundaries. The
    /// reported position is always the truth used by the pipeline.
    pub fn seek(&mut self, position_ms: u64) -> Result<u64, AudioError> {
        let seconds = position_ms / 1000;
        let frac = (position_ms % 1000) as f64 / 1000.0;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: symphonia::core::units::Time::new(seconds, frac),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| AudioError::BackendUnavailable(format!("seek failed: {e}")))?;
        self.decoder.reset();
        let time = self.time_base.calc_time(seeked.actual_ts);
        Ok(time.seconds * 1000 + (time.frac * 1000.0).round() as u64)
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_wav(path: &Path, duration_ms: u64) {
        // 8 kHz mono 8-bit PCM: 8000 frames per second, tiny fixtures.
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
        File::create(path).unwrap().write_all(&file).unwrap();
    }

    fn temp_wav(name: &str, duration_ms: u64) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("lumen-decode-{}-{}", name, std::process::id()))
            .with_extension("wav");
        test_wav(&path, duration_ms);
        path
    }

    use std::path::PathBuf;

    #[test]
    fn decodes_full_duration_and_ends_cleanly() {
        let path = temp_wav("full", 1000);
        let mut dec = TrackDecoder::open(&path).unwrap();
        assert_eq!(dec.properties().sample_rate_hz, Some(8000));
        assert_eq!(dec.properties().channels, Some(1));

        let mut out = Vec::new();
        let mut frames = 0;
        while let Some(n) = dec.decode_next(&mut out).unwrap() {
            frames += n;
            assert_eq!(out.len(), n * dec.channels());
        }
        assert_eq!(frames, 8000, "one second at 8 kHz");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn seek_lands_on_documented_packet_boundary() {
        let path = temp_wav("seek", 2000);
        let mut dec = TrackDecoder::open(&path).unwrap();
        // WAV seeks floor to 1152-frame packets: 8000 → 6912 frames = 864ms.
        // Assert the documented behavior: at-or-before target, within one
        // packet (1152 frames = 144ms at 8 kHz).
        let landed = dec.seek(1000).unwrap();
        assert!(
            (856..=1000).contains(&landed),
            "landed at {landed}ms (expect within one 1152-frame packet below target)"
        );
        let landed_frames = landed * 8;

        let mut out = Vec::new();
        let mut frames = 0u64;
        while let Some(n) = dec.decode_next(&mut out).unwrap() {
            frames += n as u64;
        }
        assert_eq!(
            frames,
            16_000 - landed_frames,
            "decode continues exactly from the landed position"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn malformed_file_fails_at_open() {
        let path =
            std::env::temp_dir().join(format!("lumen-decode-bad-{}.flac", std::process::id()));
        std::fs::write(&path, b"not a real flac").unwrap();
        assert!(TrackDecoder::open(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn truncated_file_ends_gracefully() {
        let path = temp_wav("trunc", 1000);
        // Cut the file in half; the header promises more than the file has.
        let len = std::fs::metadata(&path).unwrap().len() / 2;
        let truncated: Vec<u8> = std::fs::read(&path).unwrap()[..len as usize].to_vec();
        std::fs::write(&path, truncated).unwrap();

        let mut dec = TrackDecoder::open(&path).unwrap();
        let mut out = Vec::new();
        let mut frames = 0;
        loop {
            match dec.decode_next(&mut out) {
                Ok(Some(n)) => frames += n,
                Ok(None) => break,
                Err(_) => break, // a hard error is also acceptable here
            }
        }
        assert!(frames < 8000, "must not invent the missing half");
        let _ = std::fs::remove_file(path);
    }
}
