use crate::block_hash::BlockChecksum;
use crate::compression::CompressionCodec;
use crate::error::{Error, Result};
use crate::header::Header;
use crate::map::{
    CompressedEntryProof, CompressionTypeLegacy, CompressionTypeV5, Map, MapEntry,
    UncompressedEntryProof,
};

#[cfg(feature = "unstable_lending_iterators")]
use crate::iter::{Hunks, MetadataEntries};

use crate::metadata::MetadataRefs;
use byteorder::{BigEndian, WriteBytesExt};
use crc::Crc;
use num_traits::ToPrimitive;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::panic::AssertUnwindSafe;

/// A CHD (MAME Compressed Hunks of Data) file.
pub struct Chd<F: Read + Seek> {
    file: F,
    header: Header,
    parent: Option<Box<Chd<F>>>,
    map: Map,
    // codecs contain Box<dyn CompressionCodec> which are all UnwindSafe.
    codecs: AssertUnwindSafe<Codecs>,
}

impl<F: Read + Seek> Chd<F> {
    /// Open a CHD file from a `Read + Seek` stream. Optionally provide a parent of the same stream
    /// type.
    ///
    /// The CHD header and hunk map are read and validated immediately.
    ///
    /// If the CHD file requires a parent, and a parent is provided, the parent hash is
    /// validated. If hash validation fails, returns [`Error::InvalidParent`](crate::Error::InvalidParent).
    ///
    /// If the CHD file does not require a parent, and a parent is provided, returns
    /// [`Error::InvalidParameter`](crate::Error::InvalidParameter).
    /// If no parent CHD is provided and the file requires a parent, then the presence of the parent
    /// will not be immediately validated. However, calls to [`read_hunk_in`](crate::Hunk::read_hunk_in)
    /// will fail with [`Error::RequiresParent`](crate::Error::RequiresParent) when a hunk is read that
    /// refers to the parent CHD.
    pub fn open(mut file: F, parent: Option<Box<Chd<F>>>) -> Result<Chd<F>> {
        let header = Header::try_read_header(&mut file)?;
        // No point in checking writable because traits are read only.
        // In the future if we want to support a Write feature, will need to ensure writable.

        if let Some(p) = parent.as_ref() {
            if !header.has_parent() {
                return Err(Error::InvalidParameter);
            }
            if p.header().sha1() != header.parent_sha1() {
                return Err(Error::InvalidParent);
            }
            // should be None for V4+
            if p.header().md5() != header.parent_md5() {
                return Err(Error::InvalidParent);
            }
        }

        let map = Map::try_read_map(&header, &mut file)?;
        let codecs = AssertUnwindSafe(header.create_compression_codecs()?);

        Ok(Chd {
            file,
            header,
            parent,
            map,
            codecs,
        })
    }

    /// Returns a reference to the CHD header for this CHD file.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Returns an iterator over references to metadata entries for this CHD file.
    ///
    /// The contents of each metadata entry are lazily read.
    pub fn metadata_refs(&mut self) -> MetadataRefs<F> {
        let offset = self.header().meta_offset();
        if let Some(offset) = offset {
            MetadataRefs::from_stream(&mut self.file, offset)
        } else {
            MetadataRefs::dead(&mut self.file)
        }
    }

    #[cfg(feature = "unstable_lending_iterators")]
    #[cfg_attr(docsrs, doc(cfg(unstable_lending_iterators)))]
    /// Returns an iterator over metadata entries for this CHD file.
    ///
    /// The contents of each metadata entry are lazily read.
    pub fn metadata(&mut self) -> MetadataEntries<F> {
        MetadataEntries::new(self.metadata_refs())
    }

    /// Returns the hunk map of this CHD File.
    pub fn map(&self) -> &Map {
        &self.map
    }

    /// Returns an aggregate [`ChdInfo`](crate::ChdInfo) snapshot of this CHD's header and metadata
    /// (the data chdman's `info` subcommand reports; mirrors libchdman-rs's `Chd::info`).
    ///
    /// Walks the metadata once to derive the tag list, track count, and the `is_hd`/`is_cd`/
    /// `is_gd`/`is_dvd`/`is_av` type flags (each a metadata-tag-presence check, exactly as MAME's
    /// `check_is_*`).
    pub fn info(&mut self) -> Result<crate::ChdInfo> {
        use crate::make_tag;
        use crate::metadata::MetadataTag;

        let h = self.header();
        let version = h.version() as u32;
        let hunk_bytes = h.hunk_size();
        let unit_bytes = h.unit_bytes();
        let hunk_count = h.hunk_count();
        let logical_bytes = h.logical_bytes();
        let codecs = h.compression();
        let sha1 = h.sha1().unwrap_or([0u8; 20]);
        let raw_sha1 = h.raw_sha1().unwrap_or([0u8; 20]);
        let parent_sha1 = h.parent_sha1().unwrap_or([0u8; 20]);
        let has_parent = h.has_parent();
        let compressed = codecs[0] != 0;

        // Walk the metadata once, recording each entry's tag + its per-tag index.
        let mut metadata_tags: Vec<(u32, u32)> = Vec::new();
        let mut per_tag: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        for m in self.metadata_refs() {
            let tag = m.metatag();
            let idx = per_tag.entry(tag).or_insert(0);
            metadata_tags.push((tag, *idx));
            *idx += 1;
        }

        let has = |t: &[u8; 4]| metadata_tags.iter().any(|&(tag, _)| tag == make_tag(t));
        let count = |t: &[u8; 4]| {
            metadata_tags
                .iter()
                .filter(|&&(tag, _)| tag == make_tag(t))
                .count() as u32
        };

        Ok(crate::ChdInfo {
            version,
            hunk_bytes,
            unit_bytes,
            hunk_count,
            logical_bytes,
            codecs,
            sha1,
            raw_sha1,
            parent_sha1,
            track_count: count(b"CHT2") + count(b"CHTR") + count(b"CHGD"),
            is_hd: has(b"GDDD"),
            is_cd: has(b"CHCD") || has(b"CHTR") || has(b"CHT2"),
            is_gd: has(b"CHGT") || has(b"CHGD"),
            is_dvd: has(b"DVD "),
            is_av: has(b"AVAV"),
            metadata_tags,
            compressed,
            has_parent,
        })
    }

    /// Returns a reference to the given hunk in this CHD file.
    ///
    /// If the requested hunk is larger than the number of hunks in the CHD file,
    /// returns `Error::HunkOutOfRange`.
    pub fn hunk(&mut self, hunk_num: u32) -> Result<Hunk<F>> {
        if hunk_num >= self.header.hunk_count() {
            return Err(Error::HunkOutOfRange);
        }
        Ok(Hunk {
            inner: self,
            hunk_num,
        })
    }

    /// Allocates a buffer with the same length as the hunk size of this CHD file.
    pub fn get_hunksized_buffer(&self) -> Vec<u8> {
        let hunk_size = self.header.hunk_size() as usize;
        vec![0u8; hunk_size]
    }

    /// Verify a compressed CHD's integrity by recomputing its SHA-1 checksums and comparing them to
    /// the header (chdman `verify`). Decompresses every hunk to recompute the **raw** SHA-1 (over the
    /// logical, unpadded bytes) and the **overall** SHA-1 (`SHA1(raw_sha1 ‖ sorted checksummed
    /// metadata hashes)`); the returned [`VerifyResult`](crate::VerifyResult) carries both computed
    /// and expected values (check [`is_valid`](crate::VerifyResult::is_valid)).
    ///
    /// Returns [`Error::UnsupportedFormat`] for an **uncompressed** CHD (those carry no stored
    /// checksum — chdman likewise refuses). If the CHD references a parent, it must have been opened
    /// with that parent (parent-ref hunks are read through it). Available with the `verify` feature.
    #[cfg(feature = "verify")]
    #[cfg_attr(docsrs, doc(cfg(feature = "verify")))]
    pub fn verify(&mut self) -> Result<crate::VerifyResult> {
        use sha1::{Digest, Sha1};

        if !self.header.is_compressed() {
            return Err(Error::UnsupportedFormat);
        }
        let expected_raw_sha1 = self
            .header
            .raw_sha1()
            .or_else(|| self.header.sha1())
            .ok_or(Error::UnsupportedFormat)?;
        let expected_sha1 = self.header.sha1().unwrap_or(expected_raw_sha1);
        let logical = self.header.logical_bytes();
        let hunk_bytes = self.header.hunk_size() as u64;
        let hunk_count = self.header.hunk_count();

        // Collect the metadata first (owns its bytes), then hash the hunks.
        let metas: Vec<crate::metadata::Metadata> = self.metadata_refs().try_into()?;

        // raw_sha1 is over the *logical* (unpadded) bytes — drop the final hunk's zero padding.
        let mut hasher = Sha1::new();
        let mut comp = Vec::new();
        let mut buf = vec![0u8; hunk_bytes as usize];
        let mut remaining = logical;
        for i in 0..hunk_count {
            self.hunk(i)?.read_hunk_in(&mut comp, &mut buf)?;
            let take = remaining.min(hunk_bytes) as usize;
            hasher.update(&buf[..take]);
            remaining -= take as u64;
        }
        let computed_raw_sha1: [u8; 20] = hasher.finalize().into();
        let computed_sha1 = crate::metadata::overall_sha1(
            &computed_raw_sha1,
            metas
                .iter()
                .map(|m| (m.metatag, m.flags, m.value.as_slice())),
        );

        Ok(crate::VerifyResult {
            computed_raw_sha1,
            computed_sha1,
            expected_raw_sha1,
            expected_sha1,
        })
    }

    #[cfg_attr(docsrs, doc(cfg(unstable_lending_iterators)))]
    #[cfg(feature = "unstable_lending_iterators")]
    /// Returns an iterator over the hunks of this CHD file.
    pub fn hunks(&mut self) -> Hunks<F> {
        Hunks::new(self)
    }

    /// Consumes the `Chd` and returns the underlying reader and parent if present.
    pub fn into_inner(self) -> (F, Option<Box<Chd<F>>>) {
        (self.file, self.parent)
    }

    /// Returns a mutable reference to the inner stream.
    pub fn inner(&mut self) -> &mut F {
        &mut self.file
    }

    /// Returns a mutable reference to the inner parent stream if present.
    pub fn inner_parent(&mut self) -> Option<&mut F> {
        self.parent.as_deref_mut().map(|f| f.inner())
    }
}

/// A reference to a compressed Hunk in a CHD file.
pub struct Hunk<'a, F: Read + Seek> {
    inner: &'a mut Chd<F>,
    hunk_num: u32,
}

impl<'a, F: Read + Seek> Hunk<'a, F> {
    /// Buffer the compressed bytes into the hunk buffer.
    fn read_compressed_in(
        &mut self,
        map_entry: CompressedEntryProof,
        comp_buf: &mut Vec<u8>,
    ) -> Result<()> {
        let offset = map_entry.block_offset();
        let length = map_entry.block_size();

        comp_buf.resize(length as usize, 0);

        self.inner.file.seek(SeekFrom::Start(offset))?;
        let read = self.inner.file.read(comp_buf)?;
        if read != length as usize {
            return Err(Error::ReadError);
        }
        Ok(())
    }

    fn read_uncompressed(
        &mut self,
        map_entry: UncompressedEntryProof,
        dest: &mut [u8],
    ) -> Result<usize> {
        let offset = map_entry.block_offset();
        let length = map_entry.block_size();

        if dest.len() != length as usize {
            return Err(Error::InvalidParameter);
        }
        self.inner.file.seek(SeekFrom::Start(offset))?;
        let read = self.inner.file.read(dest)?;
        Ok(read)
    }

    fn read_hunk_legacy(&mut self, comp_buf: &mut Vec<u8>, dest: &mut [u8]) -> Result<usize> {
        let map_entry = self
            .inner
            .map()
            .get_entry(self.hunk_num as usize)
            .ok_or(Error::HunkOutOfRange)?;

        match map_entry {
            MapEntry::LegacyEntry(entry) => {
                let block_len = entry.block_size() as usize;
                let block_crc = entry.hunk_crc();
                let block_off = entry.block_offset();

                match entry.hunk_type()? {
                    CompressionTypeLegacy::Compressed => {
                        // buffer the compressed data
                        let proof = entry.prove_compressed()?;
                        self.read_compressed_in(proof, comp_buf)?;
                        let res = &self
                            .inner
                            .codecs
                            .first_mut()
                            .decompress(&comp_buf[..block_len], dest)?;

                        Crc::<u32>::verify_block_checksum(block_crc, dest, res.total_out())
                    }
                    CompressionTypeLegacy::Uncompressed => {
                        let proof = entry.prove_uncompressed()?;
                        let res = self.read_uncompressed(proof, dest)?;
                        Crc::<u32>::verify_block_checksum(block_crc, dest, res)
                    }
                    CompressionTypeLegacy::Mini => {
                        let mut cursor = Cursor::new(dest);
                        cursor.write_u64::<BigEndian>(entry.block_offset())?;
                        let dest = cursor.into_inner();
                        let mut bytes_read_into = std::mem::size_of::<u64>();

                        // todo: optimize this operation
                        for off in
                            std::mem::size_of::<u64>()..self.inner.header().hunk_size() as usize
                        {
                            dest[off] = dest[off - 8];
                            bytes_read_into += 1;
                        }

                        Crc::<u32>::verify_block_checksum(block_crc, dest, bytes_read_into)
                    }
                    CompressionTypeLegacy::SelfHunk => {
                        let mut self_hunk = self.inner.hunk(block_off as u32)?;
                        let res = self_hunk.read_hunk_in(comp_buf, dest)?;
                        Ok(res)
                    }
                    CompressionTypeLegacy::ParentHunk => match self.inner.parent.as_deref_mut() {
                        None => Err(Error::RequiresParent),
                        Some(parent) => {
                            let mut parent = parent.hunk(block_off as u32)?;
                            let res = parent.read_hunk_in(comp_buf, dest)?;
                            Ok(res)
                        }
                    },
                    CompressionTypeLegacy::ExternalCompressed => Err(Error::UnsupportedFormat),
                    CompressionTypeLegacy::Invalid => Err(Error::InvalidData),
                }
            }
            _ => Err(Error::InvalidParameter),
        }
    }

    fn read_hunk_v5(&mut self, comp_buf: &mut Vec<u8>, dest: &mut [u8]) -> Result<usize> {
        let map_entry = self
            .inner
            .map()
            .get_entry(self.hunk_num as usize)
            .ok_or(Error::HunkOutOfRange)?;

        let has_parent = self.inner.header.has_parent();

        match map_entry {
            MapEntry::V5Compressed(entry) => {
                let block_off = entry.block_offset()?;
                let block_crc = Some(entry.hunk_crc()?);
                match entry.hunk_type()? {
                    comptype @ CompressionTypeV5::CompressionType0
                    | comptype @ CompressionTypeV5::CompressionType1
                    | comptype @ CompressionTypeV5::CompressionType2
                    | comptype @ CompressionTypeV5::CompressionType3 => {
                        // buffer the compressed data
                        let proof = entry.prove_compressed()?;

                        self.read_compressed_in(proof, comp_buf)?;

                        if let Some(codec) = self.inner.codecs.get_mut(comptype.to_usize().unwrap())
                        {
                            let res = codec.decompress(comp_buf, dest)?;
                            Crc::<u16>::verify_block_checksum(block_crc, dest, res.total_out())
                        } else {
                            Err(Error::UnsupportedFormat)
                        }
                    }
                    CompressionTypeV5::CompressionNone => {
                        let proof = entry.prove_uncompressed()?;
                        let res = self.read_uncompressed(proof, dest)?;
                        Crc::<u16>::verify_block_checksum(block_crc, dest, res)
                    }
                    CompressionTypeV5::CompressionSelf => {
                        let mut self_hunk = self.inner.hunk(block_off as u32)?;
                        let res = self_hunk.read_hunk_in(comp_buf, dest)?;
                        Ok(res)
                    }
                    CompressionTypeV5::CompressionParent => {
                        let hunk_bytes = self.inner.header().hunk_size();
                        let unit_bytes = self.inner.header().unit_bytes();
                        let units_in_hunk = hunk_bytes / unit_bytes;

                        match self.inner.parent.as_deref_mut() {
                            None => Err(Error::RequiresParent),
                            Some(parent) => {
                                let mut buf = vec![0u8; hunk_bytes as usize];

                                let mut parent_hunk =
                                    parent.hunk(block_off as u32 / units_in_hunk)?;
                                let res_1 = parent_hunk.read_hunk_in(comp_buf, &mut buf)?;

                                if block_off % units_in_hunk as u64 == 0 {
                                    dest.copy_from_slice(&buf);
                                    return Ok(res_1);
                                }

                                let remainder_in_hunk = block_off as usize % units_in_hunk as usize;
                                let hunk_split = (units_in_hunk as usize - remainder_in_hunk)
                                    * unit_bytes as usize;

                                dest[..hunk_split].copy_from_slice(
                                    &buf[remainder_in_hunk * unit_bytes as usize..][..hunk_split],
                                );

                                let mut parent_hunk =
                                    parent.hunk((block_off as u32 / units_in_hunk) + 1)?;
                                let _res_2 = parent_hunk.read_hunk_in(comp_buf, &mut buf)?;

                                dest[hunk_split..].copy_from_slice(
                                    &buf[..remainder_in_hunk
                                        * self.inner.header().unit_bytes() as usize],
                                );
                                Crc::<u16>::verify_block_checksum(
                                    block_crc,
                                    dest,
                                    hunk_split + remainder_in_hunk * unit_bytes as usize,
                                )
                            }
                        }
                    }
                    _ => Err(Error::UnsupportedFormat),
                }
            }
            MapEntry::V5Uncompressed(entry) => {
                match (entry.block_offset()?, has_parent) {
                    (0, false) => {
                        dest.fill(0);
                        Ok(dest.len())
                    }
                    (0, true) => {
                        if let Some(parent) = self.inner.parent.as_deref_mut() {
                            let mut parent = parent.hunk(self.hunk_num)?;
                            let res = parent.read_hunk_in(comp_buf, dest)?;
                            Ok(res)
                        } else {
                            Err(Error::RequiresParent)
                        }
                    }
                    (_offset, _) => {
                        // read_uncompressed will handle the proper offset for us automatically.
                        let proof = entry.prove_uncompressed()?;
                        let res = self.read_uncompressed(proof, dest)?;
                        Ok(res)
                    }
                }
            }
            MapEntry::LegacyEntry(_) => Err(Error::InvalidParameter),
        }
    }

    /// Decompresses the hunk into output, using the provided temporary buffer to hold the
    /// compressed hunk. The size of the output buffer must be equal to the hunk size of the
    /// CHD file.
    ///
    /// Returns the number of bytes decompressed on success, which should be the length of
    /// the output buffer.
    ///
    /// If the hunk refers to a parent CHD that was not provided, this will return
    /// [`Error::RequiresParent`](crate::Error::RequiresParent).
    ///
    /// If the provided output buffer is the wrong length, this will return
    /// If the hunk refers to a parent CHD that was not provided, this will return
    /// [`Error::OutOfMemory`](crate::Error::OutOfMemory).
    pub fn read_hunk_in(
        &mut self,
        compressed_buffer: &mut Vec<u8>,
        output: &mut [u8],
    ) -> Result<usize> {
        if output.len() != self.inner.header.hunk_size() as usize {
            return Err(Error::OutOfMemory);
        }

        match self.inner.map() {
            Map::V5(_) => self.read_hunk_v5(compressed_buffer, output),
            Map::Legacy(_) => self.read_hunk_legacy(compressed_buffer, output),
        }
    }

    /// Read the raw, compressed contents of the hunk into the provided buffer.
    ///
    /// Returns the number of bytes read on success.
    pub fn read_raw_in(&mut self, output: &mut Vec<u8>) -> Result<usize> {
        let map_entry = self
            .inner
            .map()
            .get_entry(self.hunk_num as usize)
            .ok_or(Error::HunkOutOfRange)?;

        let (offset, size) = match map_entry {
            MapEntry::V5Compressed(map_entry) => {
                (map_entry.block_offset()?, map_entry.block_size()?)
            }
            MapEntry::V5Uncompressed(map_entry) => {
                (map_entry.block_offset()?, map_entry.block_size())
            }
            MapEntry::LegacyEntry(map_entry) => (map_entry.block_offset(), map_entry.block_size()),
        };

        output.resize(size as usize, 0);
        self.inner.file.seek(SeekFrom::Start(offset))?;
        let read = self.inner.file.read(output)?;
        Ok(read)
    }

    #[allow(clippy::len_without_is_empty)]
    /// Returns the length of this hunk in bytes.
    pub fn len(&self) -> usize {
        self.inner.header.hunk_size() as usize
    }
}

pub(crate) enum Codecs {
    Single(Box<dyn CompressionCodec>),
    Four([Box<dyn CompressionCodec>; 4]),
}

impl Codecs {
    pub fn first_mut(&mut self) -> &mut Box<dyn CompressionCodec> {
        match self {
            Codecs::Single(c) => c,
            Codecs::Four([c, ..]) => c,
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Box<dyn CompressionCodec>> {
        if index == 0 {
            match self {
                Codecs::Single(c) => Some(c),
                Codecs::Four([c, ..]) => Some(c),
            }
        } else {
            match self {
                Codecs::Four(a) => Some(&mut a[index]),
                _ => None,
            }
        }
    }
}

#[cfg(all(test, feature = "write"))]
mod verify_tests {
    use crate::Chd;
    use std::io::Cursor;

    /// Deterministic mixed-compressibility bytes.
    fn make_input(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut x: u32 = 0x2545_f491;
        for i in 0..len {
            let b = match (i / 96) % 3 {
                0 => 0u8,
                1 => b"the quick brown fox "[i % 20],
                _ => {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    (x & 0xff) as u8
                }
            };
            v.push(b);
        }
        v
    }

    /// `verify()` accepts a freshly written compressed CHD, and detects a corrupted stored raw SHA-1
    /// (data path) and a corrupted metadata payload (overall-SHA-1 path) independently.
    #[test]
    fn verify_detects_data_and_metadata_corruption() {
        // createhd (256 KiB) writes a GDDD record → exercises the metadata-inclusive overall SHA-1.
        let input = make_input(256 * 1024);
        let mut cur = Cursor::new(Vec::new());
        crate::hd::create_from_reader(
            &input[..],
            &mut cur,
            crate::hd::HdCreateOptions {
                codecs: [crate::CHD_CODEC_ZLIB, 0, 0, 0],
                ..Default::default()
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();
        let bytes = cur.into_inner();

        // pristine → valid
        let mut chd = Chd::open(Cursor::new(bytes.clone()), None).unwrap();
        let r = chd.verify().unwrap();
        assert!(r.is_valid(), "fresh CHD should verify: {r:?}");

        // corrupt the stored raw SHA-1 (header byte 64) → raw mismatch, overall mismatch.
        let mut c1 = bytes.clone();
        c1[64] ^= 0xff;
        let r1 = Chd::open(Cursor::new(c1), None).unwrap().verify().unwrap();
        assert!(
            !r1.raw_sha1_valid(),
            "corrupted stored raw SHA-1 must be detected"
        );
        assert!(!r1.is_valid());

        // corrupt a metadata payload byte → overall mismatch, but the data (raw) is intact.
        let meta_off = u64::from_be_bytes(bytes[48..56].try_into().unwrap()) as usize;
        let mut c2 = bytes.clone();
        c2[meta_off + 16 + 8] ^= 0xff; // 16-byte entry header, then into the GDDD payload
        let r2 = Chd::open(Cursor::new(c2), None).unwrap().verify().unwrap();
        assert!(
            r2.raw_sha1_valid(),
            "data is intact so raw SHA-1 should still match"
        );
        assert!(
            !r2.overall_sha1_valid(),
            "corrupted metadata must fail the overall SHA-1"
        );
        assert!(!r2.is_valid());
    }

    /// `verify()` hashes the *logical* (unpadded) bytes: a createraw CHD with a partial last hunk
    /// (no metadata, so overall = SHA1(raw_sha1)) verifies.
    #[test]
    fn verify_partial_last_hunk_and_uncompressed() {
        // 5 full 4096-hunks + 3 × 512 units = partial last hunk.
        let input = make_input(4096 * 5 + 512 * 3);
        let mut cur = Cursor::new(Vec::new());
        crate::write::write_raw(
            &mut cur,
            &input,
            4096,
            512,
            &[crate::header::CodecType::ZLibV5],
        )
        .unwrap();
        let mut chd = Chd::open(Cursor::new(cur.into_inner()), None).unwrap();
        assert!(
            chd.verify().unwrap().is_valid(),
            "partial-last-hunk CHD should verify (raw SHA-1 over logical bytes)"
        );

        // uncompressed CHDs carry no checksum → verify refuses.
        let mut cur2 = Cursor::new(Vec::new());
        crate::write::write_raw_uncompressed(&mut cur2, &input, 4096, 512).unwrap();
        let mut chd2 = Chd::open(Cursor::new(cur2.into_inner()), None).unwrap();
        assert!(matches!(
            chd2.verify(),
            Err(crate::Error::UnsupportedFormat)
        ));
    }
}
