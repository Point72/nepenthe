//! Embed nepenthe metadata in a lockfile's leading comments.
//!
//! A `rattler_lock` lockfile is a typed document: parsing it into rattler's
//! structs and re-rendering **discards comments and unknown keys**, so metadata
//! cannot live *inside* the lock structure and survive a round-trip. Instead,
//! nepenthe bands its metadata **around** the lock — as leading YAML comment
//! lines that nepenthe writes after rendering and reads before handing the bytes
//! to a foreign parser. Leading comments are valid YAML, so the banded file is
//! still a valid `pixi.lock` that pixi/rattler read (they ignore the comment).
//!
//! The band is the **portable** half of manifest embedding: it travels with a
//! bare lock *file*, with no registry. The registry sidecar
//! ([`Registry::pull_manifest`](crate::registry::Registry::pull_manifest)) is
//! the other half — deduped and pristine, but registry-only.
//!
//! The one caveat: a foreign tool that *re-renders* the lock (e.g. pixi
//! rewriting it) strips the band, since it isn't part of the lock structure.
//! For an immutable, nepenthe-published lock this is fine.
//!
//! The manifest is encoded as one line: gzip → base64 → a `nepenthe:manifest`
//! comment, keeping the band compact even for large manifests.

use std::fmt;
use std::io::{Read, Write};

use base64::prelude::{Engine, BASE64_STANDARD};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

/// The full comment prefix for an embedded, gzip+base64-encoded manifest.
pub const MANIFEST_BAND_PREFIX: &str = "# nepenthe:manifest+gzip+b64://";

/// The common prefix of every nepenthe comment band line, used when stripping.
const BAND_LINE_PREFIX: &str = "# nepenthe:";

/// Errors raised while encoding or decoding an embedded band.
#[derive(Debug)]
pub enum EmbedError {
    /// gzip compression or decompression failed.
    Compression(std::io::Error),
    /// The base64 payload could not be decoded.
    Base64(base64::DecodeError),
    /// The decoded bytes were not valid UTF-8.
    Utf8(std::str::Utf8Error),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbedError::Compression(e) => {
                write!(f, "embedded manifest (de)compression failed: {e}")
            }
            EmbedError::Base64(e) => write!(f, "embedded manifest is not valid base64: {e}"),
            EmbedError::Utf8(e) => write!(f, "embedded manifest is not valid UTF-8: {e}"),
        }
    }
}

impl std::error::Error for EmbedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EmbedError::Compression(e) => Some(e),
            EmbedError::Base64(e) => Some(e),
            EmbedError::Utf8(e) => Some(e),
        }
    }
}

/// Encode a manifest YAML string into a single lockfile comment-band line
/// (without a trailing newline).
pub fn encode_manifest_band(manifest_yaml: &str) -> Result<String, EmbedError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(manifest_yaml.as_bytes())
        .map_err(EmbedError::Compression)?;
    let compressed = encoder.finish().map_err(EmbedError::Compression)?;
    Ok(format!(
        "{MANIFEST_BAND_PREFIX}{}",
        BASE64_STANDARD.encode(compressed)
    ))
}

/// Decode a manifest from a single comment-band line (the line must start with
/// [`MANIFEST_BAND_PREFIX`]).
fn decode_manifest_band(line: &str) -> Result<String, EmbedError> {
    let payload = line.trim_end();
    let payload = payload
        .strip_prefix(MANIFEST_BAND_PREFIX)
        .unwrap_or(payload)
        .trim();
    let compressed = BASE64_STANDARD
        .decode(payload)
        .map_err(EmbedError::Base64)?;
    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut bytes = Vec::new();
    decoder
        .read_to_end(&mut bytes)
        .map_err(EmbedError::Compression)?;
    std::str::from_utf8(&bytes)
        .map(str::to_string)
        .map_err(EmbedError::Utf8)
}

/// Band `manifest_yaml` onto rendered `lock_text`: prepend the manifest comment
/// line. Any existing nepenthe band is replaced, so re-banding is idempotent.
pub fn embed_manifest(lock_text: &str, manifest_yaml: &str) -> Result<String, EmbedError> {
    let band = encode_manifest_band(manifest_yaml)?;
    let clean = strip_band_str(lock_text);
    Ok(format!("{band}\n{clean}"))
}

/// Extract and decode the manifest embedded in `lock_bytes`, if any. Returns
/// `Ok(None)` when no nepenthe manifest band is present.
pub fn extract_manifest(lock_bytes: &[u8]) -> Result<Option<String>, EmbedError> {
    let text = match std::str::from_utf8(lock_bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    match text
        .lines()
        .find(|line| line.trim_start().starts_with(MANIFEST_BAND_PREFIX))
    {
        Some(line) => decode_manifest_band(line.trim_start()).map(Some),
        None => Ok(None),
    }
}

/// Remove any nepenthe comment-band lines from `lock_bytes`, returning clean
/// lock bytes that contain only the `rattler_lock` document.
pub fn strip_band(lock_bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(lock_bytes) {
        Ok(text) => strip_band_str(text).into_bytes(),
        Err(_) => lock_bytes.to_vec(),
    }
}

fn strip_band_str(lock_text: &str) -> String {
    let mut out = String::with_capacity(lock_text.len());
    for line in lock_text.lines() {
        if line.trim_start().starts_with(BAND_LINE_PREFIX) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str =
        "project:\n  name: demo\n  channels: [conda-forge]\ndependencies:\n  - numpy >=2\n";
    const LOCK: &str = "version: 7\nplatforms:\n- name: linux-64\nenvironments:\n  app: {}\n";

    #[test]
    fn band_round_trips_a_manifest() {
        let band = encode_manifest_band(MANIFEST).unwrap();
        assert!(band.starts_with(MANIFEST_BAND_PREFIX));
        assert_eq!(decode_manifest_band(&band).unwrap(), MANIFEST);
    }

    #[test]
    fn embed_then_extract_recovers_manifest() {
        let banded = embed_manifest(LOCK, MANIFEST).unwrap();
        // The band is the first line; the lock body follows unchanged.
        assert!(banded.starts_with(MANIFEST_BAND_PREFIX));
        assert!(banded.contains("version: 7"));
        assert_eq!(
            extract_manifest(banded.as_bytes()).unwrap().as_deref(),
            Some(MANIFEST)
        );
    }

    #[test]
    fn strip_band_yields_clean_lock() {
        let banded = embed_manifest(LOCK, MANIFEST).unwrap();
        let clean = strip_band(banded.as_bytes());
        let clean_str = std::str::from_utf8(&clean).unwrap();
        assert!(!clean_str.contains("nepenthe:manifest"));
        assert!(clean_str.contains("version: 7"));
        // Stripping a banded lock leaves the original document.
        assert_eq!(extract_manifest(&clean).unwrap(), None);
    }

    #[test]
    fn re_embedding_replaces_the_existing_band() {
        let once = embed_manifest(LOCK, MANIFEST).unwrap();
        let twice = embed_manifest(&once, "project:\n  name: other\n").unwrap();
        // Exactly one band line remains.
        let bands = twice
            .lines()
            .filter(|l| l.starts_with(MANIFEST_BAND_PREFIX))
            .count();
        assert_eq!(bands, 1);
        assert_eq!(
            extract_manifest(twice.as_bytes()).unwrap().as_deref(),
            Some("project:\n  name: other\n")
        );
    }

    #[test]
    fn extract_returns_none_without_a_band() {
        assert_eq!(extract_manifest(LOCK.as_bytes()).unwrap(), None);
        // Non-UTF8 input is tolerated (no band, no error).
        assert_eq!(extract_manifest(&[0xff, 0xfe, 0x00]).unwrap(), None);
    }
}
