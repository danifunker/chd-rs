//! DVD CHDs — chdman `createdvd` / `extractdvd` parity.
//!
//! Simpler than CDs: flat 2048-byte sectors, no ECC, no tracks — just compressed sectors plus a
//! single (effectively empty) `DVD ` metadata record so MAME recognises the file as a DVD. The
//! create path is [`hd::create_from_reader`](crate::hd::create_from_reader) with a `DVD ` record
//! instead of `GDDD`; output is **byte-identical to `chdman createdvd`**.
//!
//! Matches libchdman-rs's `dvd` module.

use crate::error::{Error, Result};
use crate::metadata::Metadata;
use crate::read::ChdReader;
use crate::{
    write, Chd, CompressionProgress, CHD_CODEC_FLAC, CHD_CODEC_HUFF, CHD_CODEC_LZMA, CHD_CODEC_ZLIB,
};
use std::convert::TryInto;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

/// DVD logical sector size in bytes.
pub const DVD_SECTOR_SIZE: u32 = 2048;
/// chdman's default DVD hunk size (`2 * 2048 = 4096`).
pub const DEFAULT_HUNK_SIZE: u32 = 2 * DVD_SECTOR_SIZE;

/// Options for DVD CHD creation. Matches libchdman-rs's `DvdCreateOptions`.
#[derive(Debug, Clone)]
pub struct DvdCreateOptions {
    /// Logical size in bytes. Must be a multiple of 2048. `0` in [`create_from_iso`] means "use the
    /// input file's size". The input is zero-padded to this size if it ends earlier.
    pub logical_size: u64,
    /// Hunk size in bytes. Default `4096`. Must be a multiple of 2048.
    pub hunk_size: u32,
    /// Codec slots. Default `[lzma, zlib, huff, flac]` (chdman's `do_create_dvd` reuses the HD
    /// default codec set). `[0; 4]` produces an uncompressed DVD CHD.
    pub codecs: [u32; 4],
}

impl Default for DvdCreateOptions {
    fn default() -> Self {
        Self {
            logical_size: 0,
            hunk_size: DEFAULT_HUNK_SIZE,
            codecs: [
                CHD_CODEC_LZMA,
                CHD_CODEC_ZLIB,
                CHD_CODEC_HUFF,
                CHD_CODEC_FLAC,
            ],
        }
    }
}

/// Stream `reader` into a DVD CHD at `out`, **byte-identical to `chdman createdvd`**.
///
/// Writes the flat 2048-byte sectors plus the `DVD ` metadata record (a single NUL byte — chdman's
/// `write_metadata(DVD_METADATA_TAG, 0, "")` stores the string's NUL terminator). Same I/O model as
/// [`hd::create_from_reader`](crate::hd::create_from_reader): whole input in memory, zero-padded to
/// `logical_size`, `progress`/`cancel` callbacks (cancel → [`Error::Cancelled`] before any bytes
/// hit `out`).
pub fn create_from_reader<R: Read, W: Write + std::io::Seek>(
    reader: R,
    out: &mut W,
    opts: DvdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    if opts.hunk_size == 0 || opts.hunk_size % DVD_SECTOR_SIZE != 0 {
        return Err(Error::InvalidParameter);
    }
    let data = write::read_and_pad(reader, opts.logical_size, DVD_SECTOR_SIZE, opts.hunk_size)?;

    // The `DVD ` tag (trailing space) with a 1-byte NUL payload — chdman writes an empty C string,
    // whose stored length includes the terminator. CHECKSUM flag, like all of chdman's records.
    let dvd_payload = [0u8; 1];
    let entries = [write::MetaEntry {
        tag: crate::make_tag(b"DVD "),
        flags: write::CHD_MDFLAGS_CHECKSUM,
        payload: &dvd_payload,
    }];

    let codecs = write::resolve_codecs(&opts.codecs)?;
    write::write_create(
        out,
        &data,
        opts.hunk_size,
        DVD_SECTOR_SIZE,
        &codecs,
        &entries,
        progress,
        cancel,
    )
}

/// File-input convenience over [`create_from_reader`]. If `opts.logical_size` is `0`, the input
/// file's size is used. On any error (including cancellation) the partial `out_path` is removed.
pub fn create_from_iso(
    iso_path: &Path,
    out_path: &Path,
    opts: DvdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let mut effective = opts;
    if effective.logical_size == 0 {
        effective.logical_size = std::fs::metadata(iso_path).map_err(Error::from)?.len();
    }
    let reader = BufReader::new(File::open(iso_path).map_err(Error::from)?);
    let mut out = File::create(out_path).map_err(Error::from)?;
    let res = create_from_reader(reader, &mut out, effective, progress, cancel);
    if res.is_err() {
        drop(out);
        let _ = std::fs::remove_file(out_path);
    }
    res
}

/// Returns whether the CHD at `chd_path` carries the `DVD ` metadata record.
fn is_dvd<F: Read + std::io::Seek>(chd: &mut Chd<F>) -> Result<bool> {
    let dvd_tag = crate::make_tag(b"DVD ");
    let metas: Vec<Metadata> = chd.metadata_refs().try_into()?;
    Ok(metas.iter().any(|m| m.metatag == dvd_tag))
}

/// Stream a DVD CHD's logical bytes to `writer` (chdman `extractdvd`). Rejects CHDs without the
/// `DVD ` metadata tag with [`Error::UnsupportedFormat`] (use [`crate::hd::extract_to_writer`] for
/// HD/raw CHDs).
pub fn extract_to_writer<W: Write>(
    chd_path: &Path,
    mut writer: W,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let f = BufReader::new(File::open(chd_path).map_err(Error::from)?);
    let mut chd = Chd::open(f, None)?;
    if !is_dvd(&mut chd)? {
        return Err(Error::UnsupportedFormat);
    }
    let logical = chd.header().logical_bytes();
    let hunk_size = chd.header().hunk_size() as usize;

    let mut reader = ChdReader::new(chd);
    let mut buf = vec![0u8; hunk_size.max(1)];
    let mut remaining = logical;
    let mut done = 0u64;
    while remaining > 0 {
        let want = remaining.min(hunk_size as u64) as usize;
        reader.read_exact(&mut buf[..want]).map_err(Error::from)?;
        writer.write_all(&buf[..want]).map_err(Error::from)?;
        remaining -= want as u64;
        done += want as u64;
        progress(done);
    }
    Ok(())
}

/// File-output convenience over [`extract_to_writer`].
pub fn extract_to_iso(
    chd_path: &Path,
    iso_path: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let mut out = BufWriter::new(File::create(iso_path).map_err(Error::from)?);
    extract_to_writer(chd_path, &mut out, progress)?;
    out.flush().map_err(Error::from)?;
    Ok(())
}
