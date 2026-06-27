//! Re-compress a CHD into a different codec set or hunk size — chdman `copy`.
//!
//! [`copy`] reads a source CHD's logical bytes, re-compresses them with a new codec list (and,
//! optionally, a new hunk size), and clones every metadata record verbatim — byte-identical to
//! `chdman copy` for the same options. The **unit size is preserved** from the source (chdman does
//! the same), so the output's logical bytes, unit bytes, and `raw_sha1` match the source's.
//!
//! Matches libchdman-rs's `copy` module. Like the `hd` create functions, the whole logical image
//! is held in memory while writing (a streaming writer is future work).
//!
//! ⚠️ CD/GD CHDs: chdman *re-does* the legacy CD metadata on copy. chd-rs clones all metadata
//! verbatim, which is correct for HD/DVD/raw CHDs and modern CD metadata; the legacy-CD re-do
//! lands with the `cd` module (Phase E).

use crate::error::{Error, Result};
use crate::metadata::Metadata;
use crate::read::ChdReader;
use crate::{write, Chd, CompressionProgress};
use std::convert::TryInto;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// Options for [`copy`]. Matches libchdman-rs's `CopyOptions`.
#[derive(Debug, Clone, Default)]
pub struct CopyOptions {
    /// New hunk size in bytes. `None` keeps the source's hunk size. Must be a multiple of the
    /// source's unit size.
    pub hunk_size: Option<u32>,
    /// New codec slots (FourCCs from [`crate::codec`]). `[0; 4]` produces an uncompressed copy. A
    /// `0` ends the list; codecs must be contiguous from slot 0.
    pub codecs: [u32; 4],
}

/// Re-compress the CHD at `source` into `dest` with `opts.codecs` and (optionally) a new hunk size,
/// cloning all metadata — chdman `copy`. On any error (including cancellation) the partial `dest`
/// is removed.
///
/// `progress` is invoked per hunk; `cancel` is polled before each hunk and, if it returns true,
/// the copy aborts with [`Error::Cancelled`].
pub fn copy(
    source: &Path,
    dest: &Path,
    opts: CopyOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let res = copy_inner(source, dest, opts, progress, cancel);
    if res.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    res
}

fn copy_inner(
    source: &Path,
    dest: &Path,
    opts: CopyOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let f = BufReader::new(File::open(source).map_err(Error::from)?);
    let mut chd = Chd::open(f, None)?;
    let logical = chd.header().logical_bytes();
    let unit = chd.header().unit_bytes();
    let hunk = opts.hunk_size.unwrap_or_else(|| chd.header().hunk_size());

    // Snapshot every metadata record in file (linked-list) order, preserving tag/flags/payload —
    // chdman's copy clones them in the same order via `write_metadata(..., APPEND, ...)`.
    let metas: Vec<Metadata> = chd.metadata_refs().try_into()?;

    // Read the full logical image (the in-memory writer needs it; ChdReader yields full hunks, so
    // read exactly `logical` bytes — past that is last-hunk padding).
    let mut data = vec![0u8; logical as usize];
    let mut reader = ChdReader::new(chd);
    reader.read_exact(&mut data).map_err(Error::from)?;

    let codecs = write::resolve_codecs(&opts.codecs)?;
    let entries: Vec<write::MetaEntry> = metas
        .iter()
        .map(|m| write::MetaEntry {
            tag: m.metatag,
            flags: m.flags,
            payload: &m.value,
        })
        .collect();

    let mut out = File::create(dest).map_err(Error::from)?;
    write::write_create(
        &mut out, &data, hunk, unit, &codecs, &entries, None, progress, cancel,
    )
}
