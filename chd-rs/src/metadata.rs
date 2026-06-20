//! Types and methods relating to metadata stored in a CHD file.

use crate::error::{Error, Result};
use crate::make_tag;
use byteorder::{BigEndian, ReadBytesExt};
use std::io::{Cursor, Read, Seek, SeekFrom};

const METADATA_HEADER_SIZE: usize = 16;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

/// A list of well-known metadata tags.
#[derive(FromPrimitive, Copy, Clone)]
#[repr(u32)]
pub enum KnownMetadata {
    /// Wildcard for search (0)
    Wildcard = 0,
    /// Hard Disk (`GDDD`)
    HardDisk = make_tag(b"GDDD"),
    /// Hard Disk Identifier (`IDNT`)
    HardDiskIdent = make_tag(b"IDNT"),
    /// Hard Disk Key (`KEY `)
    HardDiskKey = make_tag(b"KEY "),
    /// PCMCIA Card Information (`CIS `)
    PcmciaCIS = make_tag(b"CIS "),
    /// Legacy CD-ROM metadata (`CHCD`)
    CdRomOld = make_tag(b"CHCD"),
    /// CD-ROM track metadata (`CHTR`)
    CdRomTrack = make_tag(b"CHTR"),
    /// CD-ROM track metadata (`CHT2`)
    CdRomTrack2 = make_tag(b"CHT2"),
    /// Legacy GD-ROM metadata (`CHGT`)
    GdRomOld = make_tag(b"CHGT"),
    /// GD-ROM track metadata (`CHGD`)
    GdRomTrack = make_tag(b"CHGD"),
    /// A/V metadata (`AVAV`)
    AudioVideo = make_tag(b"AVAV"),
    /// LaserDisc A/V metadata (`AVLD`)
    AudioVideoLaserDisc = make_tag(b"AVLD"),
}

impl KnownMetadata {
    /// Returns whether a given tag indicates that the CHD contains CDROM data.
    pub fn is_cdrom(tag: u32) -> bool {
        if let Some(tag) = FromPrimitive::from_u32(tag) {
            return matches!(
                tag,
                KnownMetadata::CdRomOld
                    | KnownMetadata::CdRomTrack
                    | KnownMetadata::CdRomTrack2
                    | KnownMetadata::GdRomOld
                    | KnownMetadata::GdRomTrack
            );
        }
        false
    }
}

/// Trait for structs that contain or represent tagged metadata.
pub trait MetadataTag {
    /// Returns the FourCC metatag that this struct represents or refers to.
    fn metatag(&self) -> u32;
}

impl MetadataTag for KnownMetadata {
    fn metatag(&self) -> u32 {
        *self as u32
    }
}

/// A complete CHD metadata entry with contents read into memory.
#[derive(Debug)]
pub struct Metadata {
    /// The FourCC metadata tag.
    pub metatag: u32,
    /// The contents of this metadata entry.
    pub value: Vec<u8>,
    /// The flags of this metadata entry.
    pub flags: u8,
    /// The index of this metadata entry relative to the beginning of the metadata section.
    pub index: u32,
    /// The length of this metadata entry.
    pub length: u32,
}

impl MetadataTag for Metadata {
    fn metatag(&self) -> u32 {
        self.metatag
    }
}

/// A reference to a metadata entry within the CHD file.
#[derive(Clone)]
pub struct MetadataRef {
    offset: u64,
    length: u32,
    flags: u8,
    index: u32,
    metatag: u32,
}

impl MetadataRef {
    fn read_into<F: Read + Seek>(&self, file: &mut F, buf: &mut [u8]) -> Result<()> {
        file.seek(SeekFrom::Start(self.offset + METADATA_HEADER_SIZE as u64))?;
        file.read_exact(buf)?;
        Ok(())
    }

    /// Read the contents of the metadata from the input stream. The `ChdMetadataRef` must have
    /// the same provenance as the input stream for a successful read.
    pub fn read<F: Read + Seek>(&self, file: &mut F) -> Result<Metadata> {
        let mut buf = vec![0u8; self.length as usize];
        self.read_into(file, &mut buf)?;
        Ok(Metadata {
            metatag: self.metatag,
            value: buf,
            flags: self.flags,
            index: self.index,
            length: self.length,
        })
    }
}

impl MetadataTag for MetadataRef {
    #[inline(always)]
    fn metatag(&self) -> u32 {
        self.metatag
    }
}

/// An iterator over references to the metadata entries of a CHD file.
/// If `unstable_lending_iterators` is enabled, metadata can be
/// more ergonomically iterated over with [`MetadataEntries`](crate::iter::MetadataEntries).
pub struct MetadataRefs<'a, F: Read + Seek + 'a> {
    pub(crate) file: &'a mut F,
    curr_offset: u64,
    curr: Option<MetadataRef>,
    // Just use a tuple because we rarely have more than 2 or 3 types of tag.
    indices: Vec<(u32, u32)>,
}

impl<'a, F: Read + Seek + 'a> MetadataRefs<'a, F> {
    pub(crate) fn from_stream(file: &'a mut F, initial_offset: u64) -> Self {
        MetadataRefs {
            file,
            curr_offset: initial_offset,
            curr: None,
            indices: Vec::new(),
        }
    }

    pub(crate) fn dead(file: &'a mut F) -> Self {
        MetadataRefs {
            file,
            curr_offset: 0,
            curr: None,
            indices: Vec::new(),
        }
    }
}

impl<'a, F: Read + Seek + 'a> TryFrom<MetadataRefs<'a, F>> for Vec<Metadata> {
    type Error = Error;

    fn try_from(mut value: MetadataRefs<'a, F>) -> std::result::Result<Self, Self::Error> {
        let metas = &mut value;
        let metas: Vec<_> = metas.collect();
        metas.iter().map(|e| e.read(&mut value.file)).collect()
    }
}

impl<'a, F: Read + Seek + 'a> Iterator for MetadataRefs<'a, F> {
    // really need GATs to do this properly...
    type Item = MetadataRef;

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr_offset == 0 {
            return None;
        }

        fn next_inner<'a, F: Read + Seek + 'a>(s: &mut MetadataRefs<'a, F>) -> Result<MetadataRef> {
            let mut raw_header: [u8; METADATA_HEADER_SIZE] = [0; METADATA_HEADER_SIZE];
            s.file.seek(SeekFrom::Start(s.curr_offset))?;
            let count = s.file.read(&mut raw_header)?;
            if count != METADATA_HEADER_SIZE {
                return Err(Error::MetadataNotFound);
            }
            let mut cursor = Cursor::new(raw_header);
            cursor.seek(SeekFrom::Start(0))?;

            // extract data
            let metatag = cursor.read_u32::<BigEndian>()?;
            let length = cursor.read_u32::<BigEndian>()?;
            let next = cursor.read_u64::<BigEndian>()?;

            let flags = length >> 24;
            // mask off flags
            let length = length & 0x00ffffff;

            let mut index = 0;

            for indice in s.indices.iter_mut() {
                if indice.0 == metatag {
                    index = indice.1;
                    // increment current index
                    indice.1 += 1;
                    break;
                }
            }

            if index == 0 {
                s.indices.push((metatag, 1))
            }

            let new = MetadataRef {
                offset: s.curr_offset,
                length,
                metatag,
                flags: flags as u8,
                index,
            };

            s.curr_offset = next;
            s.curr = Some(new.clone());
            Ok(new)
        }
        next_inner(self).ok()
    }
}

/// Metadata flag: this entry's payload is included in the overall SHA-1 (MAME's
/// `CHD_MDFLAGS_CHECKSUM`). chdman writes this on every metadata record by default.
#[cfg(feature = "write")]
pub const METADATA_FLAG_CHECKSUM: u8 = 0x01;

// In-place metadata writer for existing V5 CHDs (chdman `addmeta`/`delmeta`). These mutate the
// file's metadata linked list, so they take a `Read + Write + Seek` handle (e.g. a `File` opened
// read-write) rather than chd-rs's read-only [`Chd`]. Ports of `chd_file::write_metadata` /
// `delete_metadata` (chd.cpp:1542/1641); byte-identical to chdman.
#[cfg(feature = "write")]
mod edit {
    use super::{Error, Result, METADATA_HEADER_SIZE};
    use sha1::{Digest, Sha1};
    use std::io::{Read, Seek, SeekFrom, Write};

    // V5 header field offsets.
    const VERSION_OFF: u64 = 12;
    const COMPRESSION_OFF: u64 = 16;
    const META_OFFSET_FIELD: u64 = 48;
    const RAW_SHA1_OFF: u64 = 64;
    const SHA1_OFF: u64 = 84;
    const CHD_MDFLAGS_CHECKSUM: u8 = 0x01;

    struct Found {
        found: bool,
        offset: u64,
        prev: u64,
        next: u64,
        length: u32,
    }

    fn read_u32be<F: Read + Seek>(file: &mut F, off: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        file.seek(SeekFrom::Start(off))?;
        file.read_exact(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }

    fn read_u64be<F: Read + Seek>(file: &mut F, off: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        file.seek(SeekFrom::Start(off))?;
        file.read_exact(&mut b)?;
        Ok(u64::from_be_bytes(b))
    }

    /// Verify the handle is a V5 CHD (the only version chd-rs writes/edits).
    fn check_v5<F: Read + Seek>(file: &mut F) -> Result<()> {
        let mut magic = [0u8; 8];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut magic)?;
        if &magic != b"MComprHD" {
            return Err(Error::InvalidFile);
        }
        if read_u32be(file, VERSION_OFF)? != 5 {
            return Err(Error::UnsupportedVersion);
        }
        Ok(())
    }

    fn sha1_20(data: &[u8]) -> [u8; 20] {
        let mut h = Sha1::new();
        h.update(data);
        h.finalize().into()
    }

    /// Port of `metadata_find` (chd.cpp:2804): walk the linked list from `meta_offset`, returning
    /// the `index`-th entry whose tag matches (or `found=false` with `prev` = the last entry, so a
    /// caller can append). `index == u32::MAX` never matches (chdman's `CHDMETAINDEX_APPEND`).
    fn metadata_find<F: Read + Seek>(
        file: &mut F,
        meta_offset: u64,
        tag: u32,
        index: u32,
    ) -> Result<Found> {
        let mut offset = meta_offset;
        let mut prev = 0u64;
        let mut idx = index as i32;
        while offset != 0 {
            let mut hdr = [0u8; METADATA_HEADER_SIZE];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut hdr)?;
            let etag = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
            let elen = ((hdr[5] as u32) << 16) | ((hdr[6] as u32) << 8) | hdr[7] as u32;
            let enext = u64::from_be_bytes([
                hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
            ]);
            if etag == tag {
                if idx == 0 {
                    return Ok(Found {
                        found: true,
                        offset,
                        prev,
                        next: enext,
                        length: elen,
                    });
                }
                idx -= 1;
            }
            prev = offset;
            offset = enext;
        }
        Ok(Found {
            found: false,
            offset: 0,
            prev,
            next: 0,
            length: 0,
        })
    }

    /// Port of `metadata_set_previous_next` (chd.cpp:2858): point the previous entry (or, if
    /// `prev == 0`, the header's `meta_offset` field) at `next`.
    fn set_previous_next<F: Write + Seek>(file: &mut F, prev: u64, next: u64) -> Result<()> {
        let off = if prev == 0 {
            META_OFFSET_FIELD
        } else {
            prev + 8
        };
        file.seek(SeekFrom::Start(off))?;
        file.write_all(&next.to_be_bytes())?;
        Ok(())
    }

    /// Port of `compute_overall_sha1` (chd.cpp:1709) reading the on-disk metadata list:
    /// `SHA1(raw_sha1 ‖ sorted[tag(4 BE) ‖ SHA1(payload)])` over CHECKSUM-flagged entries.
    fn overall_sha1<F: Read + Seek>(
        file: &mut F,
        raw_sha1: &[u8; 20],
        meta_offset: u64,
    ) -> Result<[u8; 20]> {
        let mut hashes: Vec<[u8; 24]> = Vec::new();
        let mut offset = meta_offset;
        while offset != 0 {
            let mut hdr = [0u8; METADATA_HEADER_SIZE];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut hdr)?;
            let etag = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
            let eflags = hdr[4];
            let elen = ((hdr[5] as u32) << 16) | ((hdr[6] as u32) << 8) | hdr[7] as u32;
            let enext = u64::from_be_bytes([
                hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
            ]);
            if eflags & CHD_MDFLAGS_CHECKSUM != 0 {
                let mut payload = vec![0u8; elen as usize];
                file.read_exact(&mut payload)?; // positioned at offset + 16 after the header read
                let mut h = [0u8; 24];
                h[0..4].copy_from_slice(&etag.to_be_bytes());
                h[4..24].copy_from_slice(&sha1_20(&payload));
                hashes.push(h);
            }
            offset = enext;
        }
        hashes.sort_unstable();
        let mut hasher = Sha1::new();
        hasher.update(raw_sha1);
        for h in &hashes {
            hasher.update(h);
        }
        Ok(hasher.finalize().into())
    }

    /// Port of `metadata_update_hash` (chd.cpp:2890): for a **compressed** V5 CHD, recompute the
    /// overall SHA-1 from the current metadata and write it to the header. Uncompressed CHDs keep
    /// zero SHA-1 (chdman computes none), so this is a no-op for them.
    fn update_hash<F: Read + Write + Seek>(file: &mut F) -> Result<()> {
        if read_u32be(file, COMPRESSION_OFF)? == 0 {
            return Ok(());
        }
        let mut raw = [0u8; 20];
        file.seek(SeekFrom::Start(RAW_SHA1_OFF))?;
        file.read_exact(&mut raw)?;
        let meta_offset = read_u64be(file, META_OFFSET_FIELD)?;
        let overall = overall_sha1(file, &raw, meta_offset)?;
        file.seek(SeekFrom::Start(SHA1_OFF))?;
        file.write_all(&overall)?;
        Ok(())
    }

    pub fn write_metadata<F: Read + Write + Seek>(
        file: &mut F,
        tag: u32,
        index: u32,
        data: &[u8],
        flags: u8,
    ) -> Result<()> {
        if data.is_empty() || data.len() >= 16 * 1024 * 1024 {
            return Err(Error::InvalidParameter);
        }
        check_v5(file)?;
        let meta_offset = read_u64be(file, META_OFFSET_FIELD)?;
        let found = metadata_find(file, meta_offset, tag, index)?;

        let mut finished = false;
        if found.found {
            if data.len() as u32 <= found.length {
                // overwrite in place; update the length field if it shrank
                file.seek(SeekFrom::Start(found.offset + METADATA_HEADER_SIZE as u64))?;
                file.write_all(data)?;
                if data.len() as u32 != found.length {
                    let l = data.len() as u32;
                    file.seek(SeekFrom::Start(found.offset + 5))?;
                    file.write_all(&[(l >> 16) as u8, (l >> 8) as u8, l as u8])?;
                }
                finished = true;
            } else {
                // doesn't fit — unlink the old entry, then append below
                set_previous_next(file, found.prev, found.next)?;
            }
        }

        if !finished {
            // append a fresh entry at EOF and link the previous entry (or header) to it
            let new_offset = file.seek(SeekFrom::End(0))?;
            let l = data.len() as u32;
            let mut hdr = [0u8; METADATA_HEADER_SIZE];
            hdr[0..4].copy_from_slice(&tag.to_be_bytes());
            hdr[4] = flags;
            hdr[5] = (l >> 16) as u8;
            hdr[6] = (l >> 8) as u8;
            hdr[7] = l as u8;
            // next (8..16) = 0
            file.write_all(&hdr)?;
            file.write_all(data)?;
            set_previous_next(file, found.prev, new_offset)?;
        }

        update_hash(file)?;
        Ok(())
    }

    pub fn delete_metadata<F: Read + Write + Seek>(
        file: &mut F,
        tag: u32,
        index: u32,
    ) -> Result<()> {
        check_v5(file)?;
        let meta_offset = read_u64be(file, META_OFFSET_FIELD)?;
        let found = metadata_find(file, meta_offset, tag, index)?;
        if !found.found {
            return Err(Error::MetadataNotFound);
        }
        // unlink only — chdman's delete_metadata does NOT recompute the overall SHA-1.
        set_previous_next(file, found.prev, found.next)
    }
}

/// Write (add or overwrite) a metadata record in an existing **V5** CHD, byte-identical to
/// `chdman addmeta`. Mutates the on-disk metadata linked list, so it takes a `Read + Write + Seek`
/// handle (e.g. `OpenOptions::new().read(true).write(true).open(path)`), not chd-rs's read-only
/// [`Chd`](crate::Chd).
///
/// If a record with `(tag, index)` exists and the new `data` fits in its slot it is overwritten in
/// place; otherwise a new entry is appended at end-of-file and linked in. `index == u32::MAX`
/// always appends (chdman's `CHDMETAINDEX_APPEND`). `flags` is usually [`METADATA_FLAG_CHECKSUM`].
/// For a **compressed** CHD the overall SHA-1 is recomputed; uncompressed CHDs keep zero SHA-1.
///
/// `data` is written verbatim — chdman's *text* form stores a trailing NUL (`"abc"` → 4 bytes),
/// while its *file* form stores the raw bytes; replicate whichever you need in `data`.
#[cfg(feature = "write")]
#[cfg_attr(docsrs, doc(cfg(feature = "write")))]
pub fn write_metadata<F: Read + std::io::Write + Seek>(
    file: &mut F,
    tag: u32,
    index: u32,
    data: &[u8],
    flags: u8,
) -> Result<()> {
    edit::write_metadata(file, tag, index, data, flags)
}

/// Delete a metadata record from an existing **V5** CHD, byte-identical to `chdman delmeta`.
/// Unlinks the `(tag, index)` entry from the on-disk list (its bytes remain as dead space, exactly
/// as chdman leaves them). Like chdman, this does **not** recompute the overall SHA-1.
///
/// Takes a `Read + Write + Seek` handle (see [`write_metadata`]). Returns
/// [`Error::MetadataNotFound`](crate::Error::MetadataNotFound) if no matching record exists.
#[cfg(feature = "write")]
#[cfg_attr(docsrs, doc(cfg(feature = "write")))]
pub fn delete_metadata<F: Read + std::io::Write + Seek>(
    file: &mut F,
    tag: u32,
    index: u32,
) -> Result<()> {
    edit::delete_metadata(file, tag, index)
}
