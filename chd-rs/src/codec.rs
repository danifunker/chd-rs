//! CHD codec FourCC constants and compression-spec parsing.
//!
//! Mirrors libchdman-rs's `codec` module and chdman's `-c` syntax, so code written against
//! libchdman-rs ports unchanged. The constant values are the same FourCCs chd-rs already uses
//! internally ([`CodecType`](crate::header::CodecType)); this module re-exposes them under the
//! libchdman-rs names plus the `parse_codec_spec` / `codec_name` / `codec_exists` helpers.

use crate::error::{Error, Result};
use crate::make_tag;

/// No compression.
pub const CHD_CODEC_NONE: u32 = 0;
/// Deflate (zlib) compression (`zlib`).
pub const CHD_CODEC_ZLIB: u32 = make_tag(b"zlib");
/// Zstandard compression (`zstd`).
pub const CHD_CODEC_ZSTD: u32 = make_tag(b"zstd");
/// LZMA compression (`lzma`).
pub const CHD_CODEC_LZMA: u32 = make_tag(b"lzma");
/// Static Huffman compression (`huff`).
pub const CHD_CODEC_HUFF: u32 = make_tag(b"huff");
/// FLAC compression (`flac`).
pub const CHD_CODEC_FLAC: u32 = make_tag(b"flac");
/// CD Deflate compression (`cdzl`).
pub const CHD_CODEC_CD_ZLIB: u32 = make_tag(b"cdzl");
/// CD Zstandard compression (`cdzs`).
pub const CHD_CODEC_CD_ZSTD: u32 = make_tag(b"cdzs");
/// CD LZMA compression (`cdlz`).
pub const CHD_CODEC_CD_LZMA: u32 = make_tag(b"cdlz");
/// CD FLAC compression (`cdfl`).
pub const CHD_CODEC_CD_FLAC: u32 = make_tag(b"cdfl");
/// AV Huffman compression (`avhu`).
pub const CHD_CODEC_AVHUFF: u32 = make_tag(b"avhu");

/// Known codecs: `(code, mnemonic, human-readable name)`. The mnemonic is the FourCC string
/// chdman accepts in a `-c` spec.
const CODECS: &[(u32, &str, &str)] = &[
    (CHD_CODEC_NONE, "none", "None"),
    (CHD_CODEC_ZLIB, "zlib", "Deflate"),
    (CHD_CODEC_ZSTD, "zstd", "Zstandard"),
    (CHD_CODEC_LZMA, "lzma", "LZMA"),
    (CHD_CODEC_HUFF, "huff", "Huffman"),
    (CHD_CODEC_FLAC, "flac", "FLAC"),
    (CHD_CODEC_CD_ZLIB, "cdzl", "CD Deflate"),
    (CHD_CODEC_CD_ZSTD, "cdzs", "CD Zstandard"),
    (CHD_CODEC_CD_LZMA, "cdlz", "CD LZMA"),
    (CHD_CODEC_CD_FLAC, "cdfl", "CD FLAC"),
    (CHD_CODEC_AVHUFF, "avhu", "AV Huffman"),
];

/// Returns whether `codec` is a known CHD codec FourCC (including `CHD_CODEC_NONE`).
pub fn codec_exists(codec: u32) -> bool {
    CODECS.iter().any(|&(c, _, _)| c == codec)
}

/// Returns the human-readable name of a codec, or `None` if unknown.
pub fn codec_name(codec: u32) -> Option<&'static str> {
    CODECS
        .iter()
        .find(|&&(c, _, _)| c == codec)
        .map(|&(_, _, name)| name)
}

/// Parse a chdman-style `-c` compression spec into a 4-slot codec array.
///
/// `"none"` produces `[0; 4]` (uncompressed). Otherwise the spec is 1..=4 comma-separated
/// four-ASCII-byte mnemonics (e.g. `"lzma,zlib"`), each of which must satisfy [`codec_exists`].
/// Unused trailing slots are `0`.
///
/// ```
/// # use chd::codec::{parse_codec_spec, CHD_CODEC_LZMA, CHD_CODEC_ZLIB};
/// assert_eq!(parse_codec_spec("lzma,zlib").unwrap(), [CHD_CODEC_LZMA, CHD_CODEC_ZLIB, 0, 0]);
/// assert_eq!(parse_codec_spec("none").unwrap(), [0; 4]);
/// assert!(parse_codec_spec("bogus").is_err());
/// ```
pub fn parse_codec_spec(spec: &str) -> Result<[u32; 4]> {
    if spec == "none" {
        return Ok([0; 4]);
    }

    let mut out = [0u32; 4];
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.is_empty() || parts.len() > 4 {
        return Err(Error::InvalidParameter);
    }
    for (i, part) in parts.iter().enumerate() {
        let bytes = part.as_bytes();
        if bytes.len() != 4 {
            return Err(Error::InvalidParameter);
        }
        let tag = make_tag(&[bytes[0], bytes[1], bytes[2], bytes[3]]);
        if !codec_exists(tag) {
            return Err(Error::InvalidParameter);
        }
        out[i] = tag;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_none_and_single_and_multi() {
        assert_eq!(parse_codec_spec("none").unwrap(), [0; 4]);
        assert_eq!(parse_codec_spec("zlib").unwrap(), [CHD_CODEC_ZLIB, 0, 0, 0]);
        assert_eq!(
            parse_codec_spec("cdlz,cdzl,cdfl").unwrap(),
            [CHD_CODEC_CD_LZMA, CHD_CODEC_CD_ZLIB, CHD_CODEC_CD_FLAC, 0]
        );
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse_codec_spec("bogus").is_err()); // unknown 4-byte tag
        assert!(parse_codec_spec("zlb").is_err()); // not 4 bytes
        assert!(parse_codec_spec("zlib,zlib,zlib,zlib,zlib").is_err()); // >4
        assert!(parse_codec_spec("").is_err()); // empty token
    }

    #[test]
    fn names_and_existence() {
        assert!(codec_exists(CHD_CODEC_LZMA));
        assert!(!codec_exists(0xdead_beef));
        assert_eq!(codec_name(CHD_CODEC_ZLIB), Some("Deflate"));
        assert_eq!(codec_name(CHD_CODEC_NONE), Some("None"));
        assert_eq!(codec_name(0xdead_beef), None);
    }
}
