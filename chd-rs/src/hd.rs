//! Hard-disk CHDs — chdman `createraw`/`extractraw` parity, plus the geometry helpers used by
//! `createhd`/`extracthd`.
//!
//! ## What's here
//! - [`create_from_reader`] / [`create_from_path`] — full **`createhd`**: write an HD CHD with a
//!   `GDDD` geometry record (+ optional `IDNT` ident), **byte-identical to `chdman createhd`**.
//! - [`create_raw_from_reader`] / [`create_raw_from_path`] — write a raw CHD with no metadata
//!   (chdman `createraw`), **byte-identical**, with `progress`/`cancel` callbacks.
//! - [`extract_to_writer`] / [`extract_to_path`] — stream a CHD's logical bytes out (chdman
//!   `extractraw`/`extracthd`), via the existing decoder.
//! - [`HdGeometry`], [`compute_chs`], [`format_gddd`], [`read_geometry`], [`HdCreateOptions`] —
//!   geometry helpers.
//!
//! ## Coming from libchdman-rs?
//! chd-rs keeps its generic borrowed [`Chd<F>`](crate::Chd) reader and exposes creation as free
//! functions (not methods on an owned, writeable handle). See `docs/libchdman-differences.md`.

use crate::error::{Error, Result};
use crate::metadata::Metadata;
use crate::read::ChdReader;
use crate::{write, Chd, CompressionProgress};
use std::convert::TryInto;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;

/// Cylinder/head/sector geometry plus bytes-per-sector — the data behind a `GDDD` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdGeometry {
    /// Number of cylinders.
    pub cylinders: u32,
    /// Number of heads.
    pub heads: u32,
    /// Sectors per track.
    pub sectors: u32,
    /// Bytes per sector (the CHD unit size).
    pub sector_bytes: u32,
}

impl HdGeometry {
    /// Total logical bytes implied by this geometry (`cylinders * heads * sectors * sector_bytes`).
    pub fn logical_bytes(&self) -> u64 {
        u64::from(self.cylinders)
            * u64::from(self.heads)
            * u64::from(self.sectors)
            * u64::from(self.sector_bytes)
    }
}

/// Options for [`create_raw_from_reader`] / [`create_raw_from_path`].
///
/// Matches libchdman-rs's `HdCreateOptions`. [`Default`] is hunk 4096, unit 512, codec
/// `[zlib, 0, 0, 0]`. The `geometry`/`ident` fields drive `createhd`
/// ([`create_from_reader`]/[`create_from_path`]); the `create_raw_*` functions reject them.
#[derive(Debug, Clone)]
pub struct HdCreateOptions {
    /// Logical size of the destination CHD, in bytes. Must be a multiple of `unit_size`. `0` in
    /// [`create_raw_from_path`] means "use the input file's size". The input is zero-padded to
    /// this size if it ends earlier.
    pub logical_size: u64,
    /// Hunk size in bytes (chdman default 4096). Must be a multiple of `unit_size`.
    pub hunk_size: u32,
    /// Unit (sector) size in bytes (chdman default 512).
    pub unit_size: u32,
    /// Codec slots (FourCCs from [`crate::codec`]). Default `[zlib, 0, 0, 0]`. A `0` ends the
    /// list; codecs must be contiguous from slot 0. All-zero means "uncompressed".
    pub codecs: [u32; 4],
    /// Geometry for the `GDDD` record (used by [`create_from_reader`]/[`create_from_path`]). `None`
    /// derives it from `logical_size / unit_size` via [`compute_chs`]. Ignored by `create_raw_*`
    /// (which writes no metadata — passing it there is an error).
    pub geometry: Option<HdGeometry>,
    /// Identification blob written as an `IDNT` record by [`create_from_reader`]/[`create_from_path`].
    /// Ignored by `create_raw_*` (an error there).
    pub ident: Option<Vec<u8>>,
}

impl Default for HdCreateOptions {
    fn default() -> Self {
        Self {
            logical_size: 0,
            hunk_size: 4096,
            unit_size: 512,
            codecs: [crate::CHD_CODEC_ZLIB, 0, 0, 0],
            geometry: None,
            ident: None,
        }
    }
}

/// Heuristic from chdman's `guess_chs` (`chdman.cpp:1115`): find CHS values whose product is
/// `logical_bytes / sector_size`, preferring large sectors-per-track (≤63) then large head counts
/// (≤16). If no factorization exists at the current total, the total is incremented and retried —
/// allowing a tiny round-up to a factorable shape, exactly as chdman does.
///
/// Always terminates for any positive, sector-aligned input (`(total, 1, 1)`-style shapes are
/// reached eventually). `sector_bytes` in the result is `sector_size`.
///
/// ```
/// # use chd::hd::compute_chs;
/// // 256 KiB at 512-byte sectors → 512 total sectors → 1 cyl / 16 heads / 32 sectors.
/// let g = compute_chs(256 * 1024, 512).unwrap();
/// assert_eq!((g.cylinders, g.heads, g.sectors, g.sector_bytes), (1, 16, 32, 512));
/// ```
pub fn compute_chs(logical_bytes: u64, sector_size: u32) -> Result<HdGeometry> {
    if logical_bytes == 0 || sector_size == 0 {
        return Err(Error::InvalidParameter);
    }
    if logical_bytes % u64::from(sector_size) != 0 {
        return Err(Error::InvalidParameter);
    }

    let mut total: u64 = logical_bytes / u64::from(sector_size);
    loop {
        for cur_sectors in (2u32..=63).rev() {
            if total % u64::from(cur_sectors) == 0 {
                let total_heads = total / u64::from(cur_sectors);
                for cur_heads in (2u32..=16).rev() {
                    if total_heads % u64::from(cur_heads) == 0 {
                        return Ok(HdGeometry {
                            cylinders: (total_heads / u64::from(cur_heads)) as u32,
                            heads: cur_heads,
                            sectors: cur_sectors,
                            sector_bytes: sector_size,
                        });
                    }
                }
            }
        }
        total = total.checked_add(1).ok_or(Error::InvalidParameter)?;
    }
}

/// Format a geometry record exactly as chdman writes it: `"CYLS:%d,HEADS:%d,SECS:%d,BPS:%d"`
/// followed by a NUL terminator (matching MAME's `write_metadata` convention).
pub fn format_gddd(g: HdGeometry) -> Vec<u8> {
    let mut s = format!(
        "CYLS:{},HEADS:{},SECS:{},BPS:{}",
        g.cylinders, g.heads, g.sectors, g.sector_bytes
    )
    .into_bytes();
    s.push(0);
    s
}

/// Parse a `GDDD` payload (tolerant of a trailing NUL) back into [`HdGeometry`].
fn parse_gddd(raw: &[u8]) -> Result<HdGeometry> {
    let s = std::str::from_utf8(raw)
        .map_err(|_| Error::InvalidMetadata)?
        .trim_end_matches('\0');
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(Error::InvalidMetadata);
    }
    fn parse_kv(s: &str, prefix: &str) -> Result<u32> {
        s.strip_prefix(prefix)
            .ok_or(Error::InvalidMetadata)?
            .parse::<u32>()
            .map_err(|_| Error::InvalidMetadata)
    }
    Ok(HdGeometry {
        cylinders: parse_kv(parts[0], "CYLS:")?,
        heads: parse_kv(parts[1], "HEADS:")?,
        sectors: parse_kv(parts[2], "SECS:")?,
        sector_bytes: parse_kv(parts[3], "BPS:")?,
    })
}

/// Read the `GDDD` geometry record from an opened HD CHD.
///
/// Returns [`Error::MetadataNotFound`] if there is no `GDDD` record (e.g. a `createraw` CHD).
pub fn read_geometry<F: Read + Seek>(chd: &mut Chd<F>) -> Result<HdGeometry> {
    let gddd_tag = crate::make_tag(b"GDDD");
    let metas: Vec<Metadata> = chd.metadata_refs().try_into()?;
    let entry = metas
        .iter()
        .find(|m| m.metatag == gddd_tag)
        .ok_or(Error::MetadataNotFound)?;
    parse_gddd(&entry.value)
}

/// Write `reader`'s contents into a **raw** CHD (chdman `createraw`) at `out`, byte-identical to
/// chdman for the same input and options.
///
/// The whole input is read into memory (the in-memory writer requires it; a streaming writer is
/// future work). If the input is shorter than `opts.logical_size` it is zero-padded; if longer,
/// [`Error::InvalidParameter`] is returned. `progress` is invoked per hunk; `cancel` is polled
/// before each hunk and, if it returns true, [`Error::Cancelled`] is returned **before anything is
/// written to `out`** (the output is assembled in memory and flushed only on success).
///
/// `opts.geometry` / `opts.ident` are for `createhd` — pass them to [`create_from_reader`]
/// instead; setting either here returns [`Error::UnsupportedFormat`] (the raw path writes no
/// metadata).
pub fn create_raw_from_reader<R: Read, W: Write + Seek>(
    reader: R,
    out: &mut W,
    opts: HdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    if opts.geometry.is_some() || opts.ident.is_some() {
        return Err(Error::UnsupportedFormat);
    }
    let data = write::read_and_pad(reader, opts.logical_size, opts.unit_size, opts.hunk_size)?;
    let codecs = write::resolve_codecs(&opts.codecs)?;
    write::write_create(
        out,
        &data,
        opts.hunk_size,
        opts.unit_size,
        &codecs,
        &[],
        progress,
        cancel,
    )
}

/// File convenience over [`create_raw_from_reader`]. If `opts.logical_size` is `0`, the input
/// file's size is used. On any error (including cancellation) the partial `out_path` is removed.
pub fn create_raw_from_path(
    in_path: &Path,
    out_path: &Path,
    opts: HdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    create_from_path_impl(in_path, out_path, opts, false, progress, cancel)
}

/// Full **`createhd`**: stream `reader` into a hard-disk CHD with a `GDDD` geometry record (+ an
/// optional `IDNT` ident from `opts.ident`), **byte-identical to `chdman createhd`** for the same
/// input/options. Geometry is `opts.geometry` or, if `None`, derived via [`compute_chs`].
///
/// Same I/O model as [`create_raw_from_reader`] (whole input in memory, zero-padded to
/// `logical_size`, `progress`/`cancel` callbacks; cancel returns [`Error::Cancelled`] before any
/// bytes hit `out`). The compressed form computes the metadata-inclusive overall SHA-1; the
/// uncompressed form leaves the SHA-1 fields zero, exactly as chdman does.
pub fn create_from_reader<R: Read, W: Write + Seek>(
    reader: R,
    out: &mut W,
    opts: HdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let data = write::read_and_pad(reader, opts.logical_size, opts.unit_size, opts.hunk_size)?;
    let logical = data.len() as u64;

    // geometry: explicit, else derived from the logical size + sector (unit) size.
    let geom = match opts.geometry {
        Some(g) => g,
        None => compute_chs(logical, opts.unit_size)?,
    };
    let gddd = format_gddd(geom);

    // chdman writes GDDD first, then (if present) IDNT; both with the CHECKSUM flag.
    let mut entries = vec![write::MetaEntry {
        tag: crate::make_tag(b"GDDD"),
        flags: write::CHD_MDFLAGS_CHECKSUM,
        payload: &gddd,
    }];
    if let Some(ident) = &opts.ident {
        entries.push(write::MetaEntry {
            tag: crate::make_tag(b"IDNT"),
            flags: write::CHD_MDFLAGS_CHECKSUM,
            payload: ident,
        });
    }

    let codecs = write::resolve_codecs(&opts.codecs)?;
    write::write_create(
        out,
        &data,
        opts.hunk_size,
        opts.unit_size,
        &codecs,
        &entries,
        progress,
        cancel,
    )
}

/// File convenience over [`create_from_reader`] (chdman `createhd`). If `opts.logical_size` is `0`,
/// the input file's size is used. On any error (including cancellation) the partial `out_path` is
/// removed.
pub fn create_from_path(
    in_path: &Path,
    out_path: &Path,
    opts: HdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    create_from_path_impl(in_path, out_path, opts, true, progress, cancel)
}

/// Shared file-create body for [`create_raw_from_path`] (`with_metadata = false`) and
/// [`create_from_path`] (`with_metadata = true`): default the logical size to the input file's
/// size, run the matching reader-based create, and remove the partial output on error.
fn create_from_path_impl(
    in_path: &Path,
    out_path: &Path,
    opts: HdCreateOptions,
    with_metadata: bool,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let mut effective = opts;
    if effective.logical_size == 0 {
        effective.logical_size = std::fs::metadata(in_path).map_err(Error::from)?.len();
    }
    let reader = BufReader::new(File::open(in_path).map_err(Error::from)?);
    let mut out = File::create(out_path).map_err(Error::from)?;
    let res = if with_metadata {
        create_from_reader(reader, &mut out, effective, progress, cancel)
    } else {
        create_raw_from_reader(reader, &mut out, effective, progress, cancel)
    };
    if res.is_err() {
        drop(out);
        let _ = std::fs::remove_file(out_path);
    }
    res
}

/// Stream the logical contents of the CHD at `chd_path` to `writer` (chdman `extractraw` /
/// `extracthd`). `progress` reports cumulative logical bytes written. Works for any readable CHD
/// (any codec/version chd-rs decodes); the output is the exact logical image.
pub fn extract_to_writer<W: Write>(
    chd_path: &Path,
    mut writer: W,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let f = BufReader::new(File::open(chd_path).map_err(Error::from)?);
    let chd = Chd::open(f, None)?;
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

/// File convenience over [`extract_to_writer`].
pub fn extract_to_path(
    chd_path: &Path,
    out_path: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let mut out = BufWriter::new(File::create(out_path).map_err(Error::from)?);
    extract_to_writer(chd_path, &mut out, progress)?;
    out.flush().map_err(Error::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_chs_matches_chdman_examples() {
        // 256 KiB @ 512 → 512 sectors → 1/16/32 (verified against chdman createhd).
        let g = compute_chs(256 * 1024, 512).unwrap();
        assert_eq!(
            (g.cylinders, g.heads, g.sectors, g.sector_bytes),
            (1, 16, 32, 512)
        );
        assert_eq!(g.logical_bytes(), 256 * 1024);

        // odd/prime-ish totals still terminate and factor.
        let g2 = compute_chs(1024 * 512, 512).unwrap(); // 1024 sectors
        assert_eq!(g2.logical_bytes(), 1024 * 512);
    }

    #[test]
    fn compute_chs_rejects_bad_input() {
        assert!(compute_chs(0, 512).is_err());
        assert!(compute_chs(1000, 0).is_err());
        assert!(compute_chs(513, 512).is_err()); // not sector-aligned
    }

    #[test]
    fn gddd_format_parse_roundtrip() {
        let g = HdGeometry {
            cylinders: 1,
            heads: 16,
            sectors: 32,
            sector_bytes: 512,
        };
        let raw = format_gddd(g);
        assert_eq!(raw.last(), Some(&0u8), "GDDD must be NUL-terminated");
        assert_eq!(
            &raw[..raw.len() - 1],
            b"CYLS:1,HEADS:16,SECS:32,BPS:512".as_slice()
        );
        assert_eq!(parse_gddd(&raw).unwrap(), g);
        // also parse without the trailing NUL
        assert_eq!(parse_gddd(&raw[..raw.len() - 1]).unwrap(), g);
    }
}
