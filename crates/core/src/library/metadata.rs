//! Tag extraction (lofty) and metadata normalization.
//!
//! Normalization contract (keep in sync with docs/ARCHITECTURE.md):
//! 1. Missing/empty title falls back to the file name stem at persistence
//!    time (the extraction layer leaves it `None`).
//! 2. Missing artist / album / album artist stay `NULL` — LUMEN never
//!    invents strings like "Unknown Artist". Presentation is a UI concern.
//! 3. `*_normalized` columns: trimmed, interior whitespace collapsed,
//!    Unicode-lowercased (see [`normalize`]).
//! 4. `3/12`-style track/disc fields keep the first component (lofty already
//!    parses this form).
//! 5. Year comes from the tag's parsed year value; no guessing from strings
//!    like "circa 1994?".
//! 6. Genre values are split on ';' (ID3 convention); containers with
//!    multi-value genre fields (Vorbis) arrive already split.

use std::path::Path;

use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::tag::{Accessor, ItemKey};

use crate::error::LibraryError;

/// Artwork extracted from inside the audio file. Bytes are validated
/// separately (magic bytes are authoritative; the tag's MIME is advisory).
#[derive(Debug)]
pub struct EmbeddedArtwork {
    pub data: Vec<u8>,
    pub mime_hint: Option<String>,
}

/// Tags as read from the file, before persistence. All fields are optional:
/// absent metadata stays absent.
#[derive(Debug, Default)]
pub struct ExtractedMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genres: Vec<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub embedded_artwork: Option<EmbeddedArtwork>,
}

/// Normalized form for search/sort columns: trim, collapse interior
/// whitespace, lowercase (Unicode-aware).
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Trim to `Some` only when non-empty after trimming.
fn non_empty(value: Option<impl AsRef<str>>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.as_ref().trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn split_genres(raw: &str) -> Vec<String> {
    raw.split(';')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .collect()
}

/// Extract tags from one file. Errors here mean "tag layer unreadable" and
/// are per-file failures — the scanner continues with other files.
pub fn extract(path: &Path) -> Result<ExtractedMetadata, LibraryError> {
    let tagged = lofty::read_from_path(path).map_err(|e| LibraryError::Metadata {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut out = ExtractedMetadata::default();

    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Ok(out); // No tags at all — valid file, empty metadata.
    };

    out.title = non_empty(tag.title());
    out.artist = non_empty(tag.artist());
    out.album = non_empty(tag.album());
    out.album_artist = non_empty(tag.get_string(ItemKey::AlbumArtist));
    out.track_no = tag.track();
    out.disc_no = tag.disk();
    out.year = tag.date().map(|ts| ts.year as i32);

    let mut genres: Vec<String> = tag
        .get_strings(ItemKey::Genre)
        .flat_map(split_genres)
        .collect();
    genres.dedup();
    out.genres = genres;

    // Deterministic embedded-artwork pick: front cover if flagged, else the
    // first picture in tag order.
    let pictures = tag.pictures();
    let pick = pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first());
    if let Some(picture) = pick {
        out.embedded_artwork = Some(EmbeddedArtwork {
            data: picture.data().to_vec(),
            mime_hint: picture.mime_type().map(|m| m.as_str().to_string()),
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_and_lowercases() {
        assert_eq!(normalize("  Aphex \t Twin  "), "aphex twin");
        assert_eq!(normalize("Björk"), "björk");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
    }

    #[test]
    fn non_empty_filters_blank_values() {
        assert_eq!(non_empty(Some(" x ")), Some("x".to_string()));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None::<&str>), None);
    }

    #[test]
    fn genres_split_on_semicolon() {
        assert_eq!(
            split_genres("Ambient; IDM ;Electronic"),
            vec!["Ambient", "IDM", "Electronic"]
        );
        assert_eq!(split_genres(""), Vec::<String>::new());
    }
}
