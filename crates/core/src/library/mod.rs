//! Library subsystem: scanning, metadata, artwork, persistence, queries.
//!
//! Pipeline (see docs/ARCHITECTURE.md §9):
//! filesystem → [`scanner`] (walk + classify) → [`metadata`] (lofty) +
//! [`probe`] (symphonia) → [`artwork`] → [`store`] (single DB writer) →
//! [`query`] (read API for the app shell).

pub mod artwork;
pub mod metadata;
pub mod probe;
pub mod query;
pub mod scanner;
pub mod store;

/// Audio containers LUMEN accepts, matched against the extensions a real
/// music folder actually contains.
///
/// Every entry must be (a) openable by symphonia with the features enabled in
/// `crates/core/Cargo.toml` and (b) carry a codec that
/// [`crate::library::probe::codec_name`] recognises.
///
/// Deliberately absent, because symphonia 0.5 cannot decode them: `.opus`
/// (the codec exists, the decoder does not), `.ape`, `.wv`, `.wma`, `.mpc`,
/// DSD (`.dsf`/`.dff`), AC3/DTS. Rejecting those up front with a visible
/// reason is honest; a library row that dies at playback time is not.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // Lossless
    "flac", "aiff", "aif", "aifc", "caf", "wav", "wave", // Lossy
    "mp3", "mp2", "mp1", "aac", "m4a", "m4b", "m4p", "mp4", "ogg", "oga", "mka",
];

/// Case-insensitive check whether a file extension (with or without dot)
/// belongs to a supported audio format.
pub fn is_supported_audio_extension(extension: &str) -> bool {
    let ext = extension.trim_start_matches('.');
    SUPPORTED_EXTENSIONS
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known))
}

/// Facts about a file observed on disk during a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedFile {
    pub size_bytes: u64,
    /// Modification time in milliseconds since the Unix epoch.
    pub mtime_ms: i64,
}

/// Facts the database stored about a file the last time it was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredFile {
    pub size_bytes: u64,
    pub mtime_ms: i64,
}

/// How an observed file relates to what the library knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanClassification {
    /// On disk, unknown to the library → extract metadata and insert.
    New,
    /// On disk, matches stored size+mtime → skip (no re-read needed).
    Unchanged,
    /// On disk, but size or mtime changed → re-extract and update.
    Modified,
    /// Known to the library but no longer on disk → mark missing
    /// (never delete the row: files come back with removable drives).
    Missing,
}

/// Classify a (possibly absent) observed file against a (possibly absent)
/// stored record. Returns `None` for the meaningless (absent, absent) pair.
pub fn classify(
    observed: Option<&ObservedFile>,
    stored: Option<&StoredFile>,
) -> Option<ScanClassification> {
    match (observed, stored) {
        (Some(_), None) => Some(ScanClassification::New),
        (Some(o), Some(s)) => Some(
            if o.size_bytes == s.size_bytes && o.mtime_ms == s.mtime_ms {
                ScanClassification::Unchanged
            } else {
                ScanClassification::Modified
            },
        ),
        (None, Some(_)) => Some(ScanClassification::Missing),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions_match_the_format_commitment() {
        for ext in [
            "flac", "FLAC", ".flac", "wav", "mp3", "m4a", "aac", "ogg", ".Mp3", "aiff", "m4b",
            "caf", "mka", "oga", "mp4",
        ] {
            assert!(is_supported_audio_extension(ext), "{ext} must be supported");
        }
        // Rejected on purpose: symphonia 0.5 has no decoder for these.
        for ext in [
            "exe", "txt", "wma", "cue", "", ".", "pdf", "jpg", "opus", "ape", "wv",
        ] {
            assert!(
                !is_supported_audio_extension(ext),
                "{ext} must not be silently supported"
            );
        }
    }

    #[test]
    fn classification_covers_all_states() {
        let observed = ObservedFile {
            size_bytes: 100,
            mtime_ms: 1_000,
        };
        let stored = StoredFile {
            size_bytes: 100,
            mtime_ms: 1_000,
        };
        let changed = StoredFile {
            size_bytes: 100,
            mtime_ms: 2_000,
        };

        assert_eq!(
            classify(Some(&observed), None),
            Some(ScanClassification::New)
        );
        assert_eq!(
            classify(Some(&observed), Some(&stored)),
            Some(ScanClassification::Unchanged)
        );
        assert_eq!(
            classify(Some(&observed), Some(&changed)),
            Some(ScanClassification::Modified)
        );
        assert_eq!(
            classify(None, Some(&stored)),
            Some(ScanClassification::Missing)
        );
        assert_eq!(classify(None, None), None);
    }
}
