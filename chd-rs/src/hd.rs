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
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
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

/// Read a 4-byte big-endian field from a header buffer.
fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// A read/write **block-device view** over an uncompressed hard-disk CHD — the surface MAME's
/// `harddisk_image_device` exposes to a running machine. Backs per-sector [`read_sector`]/
/// [`write_sector`] (and whole-hunk reads) onto an uncompressed V5 CHD, optionally with a
/// (compressed) **parent**: unwritten hunks fall through to the parent, and writes materialise the
/// affected hunk into the diff and rewrite its map entry.
///
/// Matches libchdman-rs's `HdImage`. chd-rs's [`Chd`] is read-only, so `HdImage` holds the diff/child
/// as a raw read-write file plus an in-memory copy of the 4-byte map, and keeps the parent open as a
/// read-only [`Chd`] for fall-through reads.
///
/// [`read_sector`]: HdImage::read_sector
/// [`write_sector`]: HdImage::write_sector
pub struct HdImage {
    file: File,
    parent: Option<Box<Chd<BufReader<File>>>>,
    /// 4-byte map entries (`offset / hunk_bytes`; `0` = read from parent / zero-fill).
    map: Vec<u32>,
    geometry: HdGeometry,
    hunk_bytes: u32,
    logical_bytes: u64,
    comp_buf: Vec<u8>,
    hunk_buf: Vec<u8>,
}

const V5_HEADER_LEN: usize = 124;

impl HdImage {
    /// Open an **uncompressed** HD CHD read-write for in-place sector edits. Fails with
    /// [`Error::UnsupportedFormat`] if the CHD is compressed (MAME also rejects compressed write
    /// targets) or [`Error::MetadataNotFound`] if it has no `GDDD` geometry record.
    pub fn open(path: &Path) -> Result<Self> {
        // Probe read-only for header fields + geometry.
        let mut probe = Chd::open(BufReader::new(File::open(path).map_err(Error::from)?), None)?;
        if probe.header().compression()[0] != 0 {
            return Err(Error::UnsupportedFormat);
        }
        let geometry = read_geometry(&mut probe)?;
        let logical_bytes = probe.header().logical_bytes();
        let hunk_bytes = probe.header().hunk_size();
        let hunk_count = probe.header().hunk_count();
        drop(probe);

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(Error::from)?;
        let map = Self::read_map(&mut file, hunk_count)?;
        Ok(Self::assemble(
            file,
            None,
            map,
            geometry,
            hunk_bytes,
            logical_bytes,
        ))
    }

    /// Open `parent_path` read-only and create a fresh **uncompressed diff** at `diff_path` whose
    /// every hunk initially falls through to the parent; subsequent writes land in the diff. This is
    /// MAME's runtime strategy for writing to a compressed image. The parent's metadata is cloned
    /// into the diff (so geometry is available), and `diff_path` is created fresh (overwritten).
    pub fn open_with_diff(parent_path: &Path, diff_path: &Path) -> Result<Self> {
        {
            let mut parent = Chd::open(
                BufReader::new(File::open(parent_path).map_err(Error::from)?),
                None,
            )?;
            let logical = parent.header().logical_bytes();
            let hunk = parent.header().hunk_size();
            let unit = parent.header().unit_bytes();
            let parent_sha1 = parent.header().sha1().unwrap_or([0u8; 20]);
            if parent_sha1 == [0u8; 20] {
                // A V5 child keys its parent by the parent's overall SHA-1; an uncompressed parent
                // has none, so it can't be referenced as a diff parent.
                return Err(Error::UnsupportedFormat);
            }
            let metas: Vec<Metadata> = parent.metadata_refs().try_into()?;
            let entries: Vec<write::MetaEntry> = metas
                .iter()
                .map(|m| write::MetaEntry {
                    tag: m.metatag,
                    flags: m.flags,
                    payload: &m.value,
                })
                .collect();
            let mut out = File::create(diff_path).map_err(Error::from)?;
            write::write_empty_diff(&mut out, logical, hunk, unit, &parent_sha1, &entries)?;
        }
        Self::reopen_diff(parent_path, diff_path)
    }

    /// Re-open a previously-created diff against its parent. The returned `HdImage` keeps the parent
    /// open for its whole lifetime (unwritten hunks read through it).
    pub fn reopen_diff(parent_path: &Path, diff_path: &Path) -> Result<Self> {
        // Probe the diff with its parent (validates `parent_sha1`) for header fields + geometry.
        let geometry;
        let logical_bytes;
        let hunk_bytes;
        let hunk_count;
        {
            let parent = Chd::open(
                BufReader::new(File::open(parent_path).map_err(Error::from)?),
                None,
            )?;
            let mut probe = Chd::open(
                BufReader::new(File::open(diff_path).map_err(Error::from)?),
                Some(Box::new(parent)),
            )?;
            if probe.header().compression()[0] != 0 {
                return Err(Error::UnsupportedFormat);
            }
            logical_bytes = probe.header().logical_bytes();
            hunk_bytes = probe.header().hunk_size();
            hunk_count = probe.header().hunk_count();
            geometry = read_geometry(&mut probe)?;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(diff_path)
            .map_err(Error::from)?;
        let map = Self::read_map(&mut file, hunk_count)?;
        let parent = Chd::open(
            BufReader::new(File::open(parent_path).map_err(Error::from)?),
            None,
        )?;
        Ok(Self::assemble(
            file,
            Some(Box::new(parent)),
            map,
            geometry,
            hunk_bytes,
            logical_bytes,
        ))
    }

    fn read_map(file: &mut File, hunk_count: u32) -> Result<Vec<u32>> {
        let mut raw = vec![0u8; hunk_count as usize * 4];
        file.seek(SeekFrom::Start(V5_HEADER_LEN as u64))
            .map_err(Error::from)?;
        file.read_exact(&mut raw).map_err(Error::from)?;
        Ok(raw.chunks_exact(4).map(|c| be32(c, 0)).collect())
    }

    fn assemble(
        file: File,
        parent: Option<Box<Chd<BufReader<File>>>>,
        map: Vec<u32>,
        geometry: HdGeometry,
        hunk_bytes: u32,
        logical_bytes: u64,
    ) -> Self {
        HdImage {
            file,
            parent,
            map,
            geometry,
            hunk_bytes,
            logical_bytes,
            comp_buf: Vec::new(),
            hunk_buf: vec![0u8; hunk_bytes as usize],
        }
    }

    /// Parsed `GDDD` geometry (cylinders / heads / sectors / bytes-per-sector).
    pub fn geometry(&self) -> HdGeometry {
        self.geometry
    }

    /// Bytes per sector.
    pub fn sector_size(&self) -> u32 {
        self.geometry.sector_bytes
    }

    /// Total addressable sectors (`logical_bytes / sector_size`). Valid LBAs are `0..sector_count()`.
    pub fn sector_count(&self) -> u64 {
        self.logical_bytes / u64::from(self.geometry.sector_bytes)
    }

    /// Read one sector at logical block address `lba` into `buf` (must be exactly
    /// [`sector_size`](Self::sector_size) bytes).
    pub fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        let ss = self.geometry.sector_bytes as usize;
        if buf.len() != ss || lba >= self.sector_count() {
            return Err(Error::InvalidParameter);
        }
        let byte = lba * ss as u64;
        let hunk = (byte / self.hunk_bytes as u64) as u32;
        let off = (byte % self.hunk_bytes as u64) as usize;
        let mut hunk_buf = std::mem::take(&mut self.hunk_buf);
        let res = self.read_hunk(hunk, &mut hunk_buf);
        if res.is_ok() {
            buf.copy_from_slice(&hunk_buf[off..off + ss]);
        }
        self.hunk_buf = hunk_buf;
        res
    }

    /// Write one sector at `lba` from `buf` (must be exactly [`sector_size`](Self::sector_size)
    /// bytes). Materialises the affected hunk into the diff if it was a parent reference.
    pub fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<()> {
        let ss = self.geometry.sector_bytes as usize;
        if buf.len() != ss || lba >= self.sector_count() {
            return Err(Error::InvalidParameter);
        }
        let byte = lba * ss as u64;
        let hunk = (byte / self.hunk_bytes as u64) as u32;
        let off = (byte % self.hunk_bytes as u64) as usize;

        let mut hunk_buf = std::mem::take(&mut self.hunk_buf);
        let res = match self.read_hunk(hunk, &mut hunk_buf) {
            Ok(()) => {
                hunk_buf[off..off + ss].copy_from_slice(buf);
                self.write_hunk(hunk, &hunk_buf)
            }
            Err(e) => Err(e),
        };
        self.hunk_buf = hunk_buf;
        res
    }

    /// Read whole hunk `hunk` into `dest` (`hunk_bytes` long): from the diff if materialised, else
    /// the parent, else zeros.
    fn read_hunk(&mut self, hunk: u32, dest: &mut [u8]) -> Result<()> {
        let entry = self.map[hunk as usize];
        if entry != 0 {
            self.file
                .seek(SeekFrom::Start(entry as u64 * self.hunk_bytes as u64))
                .map_err(Error::from)?;
            self.file.read_exact(dest).map_err(Error::from)?;
        } else if let Some(parent) = self.parent.as_deref_mut() {
            let mut comp = std::mem::take(&mut self.comp_buf);
            let res = parent
                .hunk(hunk)
                .and_then(|mut h| h.read_hunk_in(&mut comp, dest));
            self.comp_buf = comp;
            res?;
        } else {
            dest.fill(0);
        }
        Ok(())
    }

    /// Write whole hunk `hunk` (`hunk_bytes` long), materialising it (appending a fresh hunk-aligned
    /// block + updating the map entry) if it was a parent reference, else overwriting in place.
    fn write_hunk(&mut self, hunk: u32, data: &[u8]) -> Result<()> {
        let entry = self.map[hunk as usize];
        let offset = if entry != 0 {
            entry as u64 * self.hunk_bytes as u64
        } else {
            // Append at EOF (which stays hunk-aligned: the data region starts aligned and every
            // appended hunk is exactly hunk_bytes), then record the new map entry on disk.
            let eof = self.file.seek(SeekFrom::End(0)).map_err(Error::from)?;
            let new_entry = (eof / self.hunk_bytes as u64) as u32;
            self.map[hunk as usize] = new_entry;
            self.file
                .seek(SeekFrom::Start(V5_HEADER_LEN as u64 + hunk as u64 * 4))
                .map_err(Error::from)?;
            self.file
                .write_all(&new_entry.to_be_bytes())
                .map_err(Error::from)?;
            eof
        };
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(Error::from)?;
        self.file.write_all(data).map_err(Error::from)?;
        Ok(())
    }

    /// Flush buffered writes to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush().map_err(Error::from)
    }
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

    /// `HdImage` diff round-trip: against a compressed parent, written sectors persist in the diff,
    /// unwritten sectors fall through to the parent, and a reopen sees the same image.
    #[test]
    fn hd_image_diff_roundtrip() {
        let dir = std::env::temp_dir();
        let parent_in = dir.join("chdrs_hdimg_parent.bin");
        let parent_chd = dir.join("chdrs_hdimg_parent.chd");
        let diff_chd = dir.join("chdrs_hdimg_diff.chd");

        // 256 KiB deterministic image → compressed (zlib) parent with a GDDD record.
        let img: Vec<u8> = (0..256 * 1024).map(|i| (i * 7 + 3) as u8).collect();
        std::fs::write(&parent_in, &img).unwrap();
        create_from_path(
            &parent_in,
            &parent_chd,
            HdCreateOptions {
                codecs: [crate::CHD_CODEC_ZLIB, 0, 0, 0],
                ..Default::default()
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();

        // Expected merged image: parent with the written sectors overlaid.
        let mut expected = img.clone();
        let written: [u64; 4] = [5, 100, 200, 511];

        {
            let mut hd = HdImage::open_with_diff(&parent_chd, &diff_chd).unwrap();
            let ss = hd.sector_size() as usize;
            assert_eq!(ss, 512);
            assert_eq!(hd.sector_count(), 512);
            for &lba in &written {
                let pat = vec![(lba as u8).wrapping_mul(3).wrapping_add(1); ss];
                hd.write_sector(lba, &pat).unwrap();
                expected[lba as usize * ss..][..ss].copy_from_slice(&pat);
            }
            hd.flush().unwrap();

            // written sector reads back; an unwritten one falls through to the parent.
            let mut buf = vec![0u8; ss];
            hd.read_sector(5, &mut buf).unwrap();
            assert_eq!(buf, expected[5 * ss..6 * ss]);
            hd.read_sector(50, &mut buf).unwrap();
            assert_eq!(
                buf,
                img[50 * ss..51 * ss],
                "unwritten sector should read the parent"
            );
        }

        // reopen and verify the whole image (written persisted + parent fall-through).
        {
            let mut hd = HdImage::reopen_diff(&parent_chd, &diff_chd).unwrap();
            let ss = hd.sector_size() as usize;
            let mut buf = vec![0u8; ss];
            for lba in 0..hd.sector_count() {
                hd.read_sector(lba, &mut buf).unwrap();
                assert_eq!(buf, expected[lba as usize * ss..][..ss], "sector {lba}");
            }
        }

        for p in [&parent_in, &parent_chd, &diff_chd] {
            let _ = std::fs::remove_file(p);
        }
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
