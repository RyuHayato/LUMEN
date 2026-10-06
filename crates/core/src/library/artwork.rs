//! Artwork discovery and content-addressed storage.
//!
//! Strategy (documented in docs/ARCHITECTURE.md):
//! - Priority: (1) embedded front cover, (2) sidecar file in the track's
//!   directory. No network artwork, ever.
//! - Image validity is determined by magic bytes — never by file extension
//!   or tag MIME claims.
//! - Storage is content-addressed (`<cache>/<aa>/<sha256>.<ext>`) so 500
//!   tracks of one album store one image, not 500.
//! - Sidecar candidates are a fixed, deterministic list. We never scan a
//!   directory for "any image" — a folder full of photos must not leak into
//!   the library.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::LibraryError;

/// Conventional sidecar names, in priority order, checked case-insensitively
/// with jpg/jpeg/png extensions.
const SIDECAR_STEMS: &[&str] = &["cover", "folder", "front", "album", "artwork"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png"];

/// Validate image bytes by magic number. Returns the canonical MIME type.
/// `None` = not an image LUMEN will store.
pub fn validate_image(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        Some("image/jpeg")
    } else if data.len() >= 8 && data[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        Some("image/png")
    } else {
        None
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        _ => "jpg",
    }
}

/// Deterministic sidecar lookup in one directory.
/// Returns the first existing conventional artwork file in priority order.
pub fn find_sidecar(dir: &Path) -> Option<PathBuf> {
    // Directory listing is needed anyway for case-insensitive matching;
    // a failed listing just means "no sidecar".
    let entries = fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort_unstable_by_key(|n| n.to_lowercase());

    for stem in SIDECAR_STEMS {
        for ext in IMAGE_EXTENSIONS {
            let wanted = format!("{stem}.{ext}");
            if let Some(found) = names.iter().find(|n| n.to_lowercase() == wanted) {
                return Some(dir.join(found));
            }
        }
    }
    None
}

/// Where an artwork image came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkSource {
    Embedded,
    Sidecar,
}

impl ArtworkSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Sidecar => "sidecar",
        }
    }
}

/// An artwork image written to the content-addressed cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtwork {
    pub hash: String,
    /// Path relative to the artwork cache directory (stored in the DB).
    pub rel_path: String,
    pub mime: &'static str,
    pub size_bytes: u64,
    pub source: ArtworkSource,
}

/// Store image bytes in the cache, deduplicating by content.
/// Returns `Err` for invalid images (caller decides how to categorize).
pub fn store(
    cache_dir: &Path,
    data: &[u8],
    source: ArtworkSource,
) -> Result<StoredArtwork, LibraryError> {
    let mime = validate_image(data).ok_or_else(|| LibraryError::Artwork {
        path: cache_dir.to_path_buf(),
        message: format!("{} artwork is not a valid JPEG/PNG", source.as_str()),
    })?;

    let hash = hex_sha256(data);
    let rel_path = format!("{}/{}.{}", &hash[..2], hash, extension_for_mime(mime));
    let full_path = cache_dir.join(&rel_path);

    if !full_path.exists() {
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|e| LibraryError::Artwork {
                path: parent.to_path_buf(),
                message: format!("create artwork dir failed: {e}"),
            })?;
        }
        fs::write(&full_path, data).map_err(|e| LibraryError::Artwork {
            path: full_path.clone(),
            message: format!("write artwork failed: {e}"),
        })?;
    }

    Ok(StoredArtwork {
        hash,
        rel_path,
        mime,
        size_bytes: data.len() as u64,
        source,
    })
}

fn hex_sha256(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46];
    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];

    #[test]
    fn magic_bytes_are_authoritative() {
        assert_eq!(validate_image(JPEG), Some("image/jpeg"));
        assert_eq!(validate_image(PNG), Some("image/png"));
        assert_eq!(validate_image(b"not an image"), None);
        assert_eq!(validate_image(&[0xFF, 0xD8]), None); // truncated
    }

    #[test]
    fn sidecar_priority_is_deterministic() {
        let dir = std::env::temp_dir().join(format!("lumen-art-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("FRONT.PNG"), PNG).unwrap();
        fs::write(dir.join("Cover.jpg"), JPEG).unwrap();

        let found = find_sidecar(&dir).unwrap();
        assert!(
            found
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("cover.jpg"),
            "cover.* must beat front.*, got {found:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_deduplicates_identical_content() {
        let dir = std::env::temp_dir().join(format!("lumen-art-store-{}", std::process::id()));
        let first = store(&dir, JPEG, ArtworkSource::Embedded).unwrap();
        let second = store(&dir, JPEG, ArtworkSource::Sidecar).unwrap();
        assert_eq!(first.hash, second.hash);
        assert!(dir.join(&first.rel_path).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_rejects_invalid_images() {
        let dir = std::env::temp_dir().join(format!("lumen-art-bad-{}", std::process::id()));
        assert!(store(&dir, b"garbage", ArtworkSource::Embedded).is_err());
    }
}
