//! CHD writing (encode side). Gated behind the `write` feature.
//!
//! Start of the V5 writer core. The first target is an **uncompressed** raw CHD that is
//! byte-identical to `chdman createraw -c none`. The compressed path (codec selection +
//! `compress_v5_map` + SHA-1) builds on this.

use crate::compression::CompressionEncoder;
use crate::error::{Error, Result};
use crate::header::CodecType;
use crate::huffman_encode::{BitWriter, HuffEncoder};
use crate::CompressionProgress;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::{Read, Seek, Write};

const CHD_MAGIC: &[u8; 8] = b"MComprHD";
const V5_HEADER_SIZE: u32 = 124;

/// Size of an on-disk metadata entry header: `tag(4) + flags(1) + length(3) + next(8)`.
const METADATA_HEADER_SIZE: u64 = 16;
/// Metadata flag: this entry's payload is included in the overall SHA-1 (`CHD_MDFLAGS_CHECKSUM`).
pub(crate) const CHD_MDFLAGS_CHECKSUM: u8 = 0x01;

/// A metadata record to write into a freshly-created CHD. `flags` is usually
/// [`CHD_MDFLAGS_CHECKSUM`] (chdman's `write_metadata` default).
pub(crate) struct MetaEntry<'a> {
    pub tag: u32,
    pub flags: u8,
    pub payload: &'a [u8],
}

/// Build the on-disk metadata linked-list blob for a freshly-created CHD: entries are laid out in
/// order starting at file offset `meta_start`, each `tag(4) + flags(1) + len(3) + next(8) +
/// payload`, with `next` pointing at the following entry's offset (0 for the last). This is the
/// inverse of [`crate::metadata`]'s reader and reproduces chdman's sequential `write_metadata`
/// appends (the header's `meta_offset` then points at `meta_start`).
fn build_metadata_blob(entries: &[MetaEntry], meta_start: u64) -> Vec<u8> {
    // entry i's file offset (header start)
    let mut offsets = Vec::with_capacity(entries.len());
    let mut off = meta_start;
    for e in entries {
        offsets.push(off);
        off += METADATA_HEADER_SIZE + e.payload.len() as u64;
    }

    let mut blob = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let next = if i + 1 < entries.len() {
            offsets[i + 1]
        } else {
            0
        };
        let len = e.payload.len() as u32;
        blob.extend_from_slice(&e.tag.to_be_bytes());
        blob.push(e.flags);
        blob.push((len >> 16) as u8);
        blob.push((len >> 8) as u8);
        blob.push(len as u8);
        blob.extend_from_slice(&next.to_be_bytes());
        blob.extend_from_slice(e.payload);
    }
    blob
}

/// Port of `chd_file::compute_overall_sha1` (`chd.cpp:1709`): the overall SHA-1 is
/// `SHA1(raw_sha1 ‖ sorted[ tag(4 BE) ‖ SHA1(payload) ])`, over only the metadata entries with the
/// `CHECKSUM` flag, sorted by the 24-byte `(tag, sha1)` tuple (a `memcmp`, i.e. lexicographic).
/// With no checksummed metadata this is just `SHA1(raw_sha1)`.
fn compute_overall_sha1(raw_sha1: &[u8; 20], entries: &[MetaEntry]) -> [u8; 20] {
    let mut hashes: Vec<[u8; 24]> = Vec::new();
    for e in entries {
        if e.flags & CHD_MDFLAGS_CHECKSUM == 0 {
            continue;
        }
        let mut h = [0u8; 24];
        h[0..4].copy_from_slice(&e.tag.to_be_bytes());
        h[4..24].copy_from_slice(&sha1_digest(e.payload));
        hashes.push(h);
    }
    hashes.sort_unstable(); // [u8; 24] Ord == memcmp == chdman's metadata_hash_compare

    let mut hasher = Sha1::new();
    hasher.update(raw_sha1);
    for h in &hashes {
        hasher.update(h);
    }
    hasher.finalize().into()
}

// V5 map compression-type codes (== MAME's COMPRESSION_* and chd-rs's CompressionTypeV5).
const COMPRESSION_NONE: u8 = 4;
const COMPRESSION_SELF: u8 = 5;
const COMPRESSION_PARENT: u8 = 6;
const COMPRESSION_RLE_SMALL: u8 = 7;
const COMPRESSION_RLE_LARGE: u8 = 8;
const COMPRESSION_SELF_0: u8 = 9;
const COMPRESSION_SELF_1: u8 = 10;
const COMPRESSION_PARENT_SELF: u8 = 11;
const COMPRESSION_PARENT_0: u8 = 12;
const COMPRESSION_PARENT_1: u8 = 13;

#[inline]
fn round_up(x: u64, align: u64) -> u64 {
    ((x + align - 1) / align) * align
}

#[inline]
fn get_u16be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
#[inline]
fn get_u24be(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}
#[inline]
fn get_u48be(b: &[u8]) -> u64 {
    ((b[0] as u64) << 40)
        | ((b[1] as u64) << 32)
        | ((b[2] as u64) << 24)
        | ((b[3] as u64) << 16)
        | ((b[4] as u64) << 8)
        | b[5] as u64
}
#[inline]
fn put_u48be(b: &mut [u8], v: u64) {
    b[0] = (v >> 40) as u8;
    b[1] = (v >> 32) as u8;
    b[2] = (v >> 24) as u8;
    b[3] = (v >> 16) as u8;
    b[4] = (v >> 8) as u8;
    b[5] = v as u8;
}

/// Resolve a `[u32; 4]` codec-FourCC list (as in [`crate::codec`] / `HdCreateOptions`) into the
/// ordered, contiguous [`CodecType`] list the writers expect. A `0` ends the list; a non-zero slot
/// after a `0` (a gap) is rejected, since the slot index is significant (it becomes the map's
/// `COMPRESSION_TYPE_<slot>`). An all-zero list yields an empty vec (= uncompressed).
pub(crate) fn resolve_codecs(codecs: &[u32; 4]) -> Result<Vec<CodecType>> {
    use num_traits::FromPrimitive;
    let mut out = Vec::new();
    let mut ended = false;
    for &c in codecs {
        if c == 0 {
            ended = true;
            continue;
        }
        if ended {
            return Err(Error::InvalidParameter); // gapped codec lists are unsupported
        }
        out.push(CodecType::from_u32(c).ok_or(Error::UnsupportedFormat)?);
    }
    Ok(out)
}

/// Port of MAME's `chd_file::bits_for_value`: number of bits needed to hold `value`.
#[inline]
fn bits_for_value(mut value: u64) -> u8 {
    let mut result = 0u8;
    while value != 0 {
        value >>= 1;
        result += 1;
    }
    result
}

#[inline]
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize().into()
}

/// Precomputed parent-hunk hashes for parent dedup when writing a child CHD. Keyed by the
/// `(crc16, sha1)` of a `hunk_bytes`-sized window at each unit-aligned offset in the parent's
/// (padded) logical image; the value is that **unit offset** (the `COMPRESSION_PARENT` reference).
pub(crate) struct ParentRef {
    map: HashMap<(u16, [u8; 20]), u64>,
    sha1: [u8; 20],
}

/// Build a [`ParentRef`] from the parent's padded logical image (`hunk_count * hunk_bytes` bytes,
/// last hunk zero-padded). Port of `chd_file_compressor::async_walk_parent` (`chd.cpp:3208`) + the
/// hashmap insert in `compress_continue` (`chd.cpp:3084`): for parent hunk `h` it hashes `units`
/// windows (`units = hunk_bytes/unit_bytes`, or **1** for the last hunk) at unit offsets
/// `h*uph + unit`, each a `hunk_bytes` window starting at that unit — so a child hunk can match the
/// parent at any unit-aligned position, not just hunk boundaries. First occurrence of a hash wins.
pub(crate) fn build_parent_ref(
    padded_img: &[u8],
    hunk_bytes: u32,
    unit_bytes: u32,
    hunk_count: u32,
    sha1: [u8; 20],
) -> ParentRef {
    let uph = (hunk_bytes / unit_bytes) as u64;
    let hb = hunk_bytes as usize;
    let ub = unit_bytes as usize;
    let mut map: HashMap<(u16, [u8; 20]), u64> = HashMap::new();
    for h in 0..hunk_count as u64 {
        let units = if h == hunk_count as u64 - 1 { 1 } else { uph };
        for unit in 0..units {
            let pos = h * uph + unit;
            let start = pos as usize * ub;
            let window = &padded_img[start..start + hb];
            let crc = crate::block_hash::CRC16.checksum(window);
            let key = (crc, sha1_digest(window));
            map.entry(key).or_insert(pos);
        }
    }
    ParentRef { map, sha1 }
}

/// Write a complete **compressed** V5 CHD containing `data`, using a single codec, intended to
/// be byte-identical to `chdman createraw -c <codec> -hs <hunk_bytes> -us <unit_bytes>`.
///
/// Layout (verified against chdman 0.288): 124-byte header, then the compressed hunks
/// byte-packed starting at offset 124, then the compressed map (`compress_v5_map`) at the end.
/// Each hunk uses the codec if it shrinks below `hunk_bytes`, else is stored uncompressed
/// (`COMPRESSION_NONE`). `raw_sha1 = SHA1(logical data)`, `sha1 = SHA1(raw_sha1)` (no metadata).
///
/// Self-hunk dedup matches chdman: a hunk byte-identical to an **earlier written** hunk is stored
/// as a `COMPRESSION_SELF` reference (keyed by the whole-hunk crc16 + sha1, first occurrence wins)
/// instead of being re-compressed. Parent-hunk refs are not yet supported (no-parent only). The
/// codec must have an encoder (`init_encoder`).
///
/// This is the single-codec convenience over [`write_raw`]; it is exactly `write_raw(.., &[codec])`.
pub fn write_raw_compressed<W: Write + Seek>(
    out: &mut W,
    data: &[u8],
    hunk_bytes: u32,
    unit_bytes: u32,
    codec: CodecType,
) -> Result<()> {
    write_raw(out, data, hunk_bytes, unit_bytes, &[codec])
}

/// Write a complete **compressed** V5 CHD using a codec **list** (1..=4 codecs), intended to be
/// byte-identical to `chdman createraw -c <c0[,c1[,c2[,c3]]]>`.
///
/// Per hunk it reproduces MAME's `chd_compressor_group::find_best_compressor` (`chdcodec.cpp:737`):
/// it tries every codec in `codecs` **in slot order**, keeping the result that is **strictly
/// smaller** than the current best (so ties go to the earlier slot), with the baseline being
/// "store uncompressed" at `hunk_bytes`. The winning slot index becomes the map's
/// `COMPRESSION_TYPE_<slot>` (0..3); if no codec beats `hunk_bytes` the hunk is `COMPRESSION_NONE`.
/// Self-hunk dedup is applied first (see [`write_raw_compressed`]). The header's `compression[0..4]`
/// records the FourCCs in slot order (unused slots zero).
///
/// `codecs` must be non-empty and at most 4 entries. For the uncompressed (no-codec) format use
/// [`write_raw_uncompressed`]. Every codec must have an encoder (`init_encoder`).
pub fn write_raw<W: Write + Seek>(
    out: &mut W,
    data: &[u8],
    hunk_bytes: u32,
    unit_bytes: u32,
    codecs: &[CodecType],
) -> Result<()> {
    write_raw_inner(
        out,
        data,
        hunk_bytes,
        unit_bytes,
        codecs,
        &[],
        None,
        &mut |_, _, _| {},
        &|| false,
    )
}

/// Core of [`write_raw`] with metadata + progress/cancel hooks (used by the public `hd` create
/// surface, including `createhd`).
///
/// `metadata` (possibly empty) is written **between the header and the compressed hunks** (so the
/// header's `meta_offset` is `V5_HEADER_SIZE` and the hunks start after the metadata blob), exactly
/// as chdman lays out a compressed `createhd`. The overall SHA-1 then includes the checksummed
/// metadata ([`compute_overall_sha1`]).
///
/// `progress(bytes_done, bytes_total, compressed_bytes_so_far)` is invoked once per hunk (before
/// processing it) and once more at the end; `cancel()` is polled before each hunk and, if it
/// returns true, the function returns [`Error::Cancelled`] **before any bytes are written to
/// `out`** — the output is assembled in memory and only flushed after the hunk loop, so a
/// cancelled `out` is left untouched. The byte output is independent of the callbacks.
pub(crate) fn write_raw_inner<W: Write + Seek>(
    out: &mut W,
    data: &[u8],
    hunk_bytes: u32,
    unit_bytes: u32,
    codecs: &[CodecType],
    metadata: &[MetaEntry],
    parent: Option<&ParentRef>,
    progress: &mut dyn FnMut(u64, u64, u64),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    if hunk_bytes == 0 || unit_bytes == 0 || hunk_bytes % unit_bytes != 0 {
        return Err(Error::InvalidParameter);
    }
    if codecs.is_empty() || codecs.len() > 4 {
        return Err(Error::InvalidParameter);
    }

    // metadata (if any) sits right after the header; hunks start after it.
    let meta_blob = build_metadata_blob(metadata, V5_HEADER_SIZE as u64);
    let meta_offset = if meta_blob.is_empty() {
        0u64
    } else {
        V5_HEADER_SIZE as u64
    };
    let data_start = V5_HEADER_SIZE as u64 + meta_blob.len() as u64;

    let logical_bytes = data.len() as u64;
    let hunk_bytes_u64 = hunk_bytes as u64;
    let hunk_count = round_up(logical_bytes, hunk_bytes_u64) / hunk_bytes_u64;

    let mut encoders: Vec<Box<dyn CompressionEncoder>> = Vec::with_capacity(codecs.len());
    for &c in codecs {
        encoders.push(c.init_encoder(hunk_bytes)?);
    }

    let mut data_stream: Vec<u8> = Vec::new();
    let mut rawmap = vec![0u8; hunk_count as usize * 12];
    let mut hunk_buf = vec![0u8; hunk_bytes as usize];
    let mut comp_buf = vec![0u8; hunk_bytes as usize];
    let mut best_buf = vec![0u8; hunk_bytes as usize];
    // self-hunk map: (whole-hunk crc16, sha1) -> the first hunk number with that data.
    let mut self_map: HashMap<(u16, [u8; 20]), u32> = HashMap::new();

    for i in 0..hunk_count {
        if cancel() {
            return Err(Error::Cancelled);
        }
        progress(
            (i * hunk_bytes_u64).min(logical_bytes),
            logical_bytes,
            data_stream.len() as u64,
        );

        // assemble the hunk (zero-padded past the logical end)
        let start = (i * hunk_bytes_u64) as usize;
        let end = ((i + 1) * hunk_bytes_u64).min(logical_bytes) as usize;
        hunk_buf[..end - start].copy_from_slice(&data[start..end]);
        for b in &mut hunk_buf[end - start..] {
            *b = 0;
        }

        let crc = crate::block_hash::CRC16.checksum(&hunk_buf);
        let sha1 = sha1_digest(&hunk_buf);
        let base = i as usize * 12;

        // SELF: identical to an earlier written hunk -> reference it, store nothing.
        if let Some(&refhunk) = self_map.get(&(crc, sha1)) {
            rawmap[base] = COMPRESSION_SELF;
            // complen (1..4) = 0, crc (10..12) = 0 (rawmap is zero-initialized).
            put_u48be(&mut rawmap[base + 4..base + 10], refhunk as u64);
            continue;
        }

        // PARENT: identical to a (unit-aligned window of the) parent -> reference it. Checked after
        // SELF, matching chdman's `compress_continue` priority. Not added to the self map (chdman
        // only adds hunks it actually writes).
        if let Some(p) = parent {
            if let Some(&refunit) = p.map.get(&(crc, sha1)) {
                rawmap[base] = COMPRESSION_PARENT;
                put_u48be(&mut rawmap[base + 4..base + 10], refunit);
                continue;
            }
        }

        // find_best_compressor: baseline is "store NONE" at hunk_bytes; a codec wins only if it
        // is strictly smaller than the current best, earliest slot first.
        let mut best_type = COMPRESSION_NONE;
        let mut best_len = hunk_bytes;
        for (slot, enc) in encoders.iter_mut().enumerate() {
            if let Ok(n) = enc.compress(&hunk_buf, &mut comp_buf) {
                let n = n as u32;
                if n < best_len {
                    best_len = n;
                    best_type = slot as u8; // COMPRESSION_TYPE_<slot> (0..3)
                    best_buf[..n as usize].copy_from_slice(&comp_buf[..n as usize]);
                }
            }
        }

        let offset = data_start + data_stream.len() as u64;
        let (type_byte, complen): (u8, u32) = if best_type == COMPRESSION_NONE {
            data_stream.extend_from_slice(&hunk_buf);
            (COMPRESSION_NONE, hunk_bytes)
        } else {
            data_stream.extend_from_slice(&best_buf[..best_len as usize]);
            (best_type, best_len)
        };

        rawmap[base] = type_byte;
        rawmap[base + 1] = (complen >> 16) as u8;
        rawmap[base + 2] = (complen >> 8) as u8;
        rawmap[base + 3] = complen as u8;
        put_u48be(&mut rawmap[base + 4..base + 10], offset);
        rawmap[base + 10] = (crc >> 8) as u8;
        rawmap[base + 11] = crc as u8;

        self_map.insert((crc, sha1), i as u32);
    }

    progress(logical_bytes, logical_bytes, data_stream.len() as u64);

    let map_offset = data_start + data_stream.len() as u64;
    let stored_map = compress_v5_map(&rawmap, hunk_count as u32, hunk_bytes, unit_bytes);

    let raw_sha1 = sha1_digest(data);
    let overall_sha1 = compute_overall_sha1(&raw_sha1, metadata);

    // --- header ---
    let mut hdr = [0u8; V5_HEADER_SIZE as usize];
    hdr[0..8].copy_from_slice(CHD_MAGIC);
    hdr[8..12].copy_from_slice(&V5_HEADER_SIZE.to_be_bytes());
    hdr[12..16].copy_from_slice(&5u32.to_be_bytes());
    // compression[0..4]: one u32 FourCC per slot, in order; unused slots stay zero.
    for (slot, &c) in codecs.iter().enumerate() {
        hdr[16 + slot * 4..16 + slot * 4 + 4].copy_from_slice(&(c as u32).to_be_bytes());
    }
    hdr[32..40].copy_from_slice(&logical_bytes.to_be_bytes());
    hdr[40..48].copy_from_slice(&map_offset.to_be_bytes());
    hdr[48..56].copy_from_slice(&meta_offset.to_be_bytes());
    hdr[56..60].copy_from_slice(&hunk_bytes.to_be_bytes());
    hdr[60..64].copy_from_slice(&unit_bytes.to_be_bytes());
    hdr[64..84].copy_from_slice(&raw_sha1);
    hdr[84..104].copy_from_slice(&overall_sha1);
    if let Some(p) = parent {
        hdr[104..124].copy_from_slice(&p.sha1);
    }

    out.write_all(&hdr)?;
    out.write_all(&meta_blob)?;
    out.write_all(&data_stream)?;
    out.write_all(&stored_map)?;
    Ok(())
}

/// Port of `chd_file::compress_v5_map` (`chd.cpp:2071`). Compresses the in-memory `rawmap`
/// (`hunk_count × 12` bytes: `[type, complen:u24, offset:u48, crc16:u16]`) into the stored V5
/// map: a 16-byte header followed by the RLE+Huffman bitstream. Byte-identical to chdman.
pub(crate) fn compress_v5_map(
    rawmap: &[u8],
    hunk_count: u32,
    hunk_bytes: u32,
    unit_bytes: u32,
) -> Vec<u8> {
    let mapcrc = crate::block_hash::CRC16.checksum(&rawmap[..(hunk_count as usize) * 12]);

    // --- RLE-compress the compression types, feeding a 16-code / 8-bit Huffman histogram ---
    let mut compression_rle: Vec<u8> = Vec::with_capacity(hunk_count as usize);
    let mut encoder = HuffEncoder::new(16, 8);
    encoder.histo_reset();

    let mut max_self: u32 = 0;
    let mut last_self: u32 = 0;
    let mut max_parent: u64 = 0;
    let mut last_parent: u64 = 0;
    let mut max_complen: u32 = 0;
    let mut lastcomp: u8 = 0;
    let mut count: i64 = 0;
    let units_per_hunk = (hunk_bytes / unit_bytes) as u64;

    for hunknum in 0..hunk_count {
        let base = hunknum as usize * 12;
        let mut curcomp = rawmap[base];

        if curcomp == COMPRESSION_SELF {
            let refhunk = get_u48be(&rawmap[base + 4..]) as u32;
            if refhunk == last_self {
                curcomp = COMPRESSION_SELF_0;
            } else if refhunk == last_self + 1 {
                curcomp = COMPRESSION_SELF_1;
            } else {
                max_self = max_self.max(refhunk);
            }
            last_self = refhunk;
        } else if curcomp == COMPRESSION_PARENT {
            let refunit = get_u48be(&rawmap[base + 4..]);
            if refunit == (hunknum as u64 * hunk_bytes as u64) / unit_bytes as u64 {
                curcomp = COMPRESSION_PARENT_SELF;
            } else if refunit == last_parent {
                curcomp = COMPRESSION_PARENT_0;
            } else if refunit == last_parent + units_per_hunk {
                curcomp = COMPRESSION_PARENT_1;
            } else {
                max_parent = max_parent.max(refunit);
            }
            last_parent = refunit;
        } else {
            max_complen = max_complen.max(get_u24be(&rawmap[base + 1..]));
        }

        if curcomp == lastcomp {
            count += 1;
        }
        if curcomp != lastcomp || hunknum == hunk_count - 1 {
            while count != 0 {
                if count < 3 {
                    encoder.histo_one(lastcomp as u32);
                    compression_rle.push(lastcomp);
                    count -= 1;
                } else if count <= 3 + 15 {
                    encoder.histo_one(COMPRESSION_RLE_SMALL as u32);
                    compression_rle.push(COMPRESSION_RLE_SMALL);
                    encoder.histo_one((count - 3) as u32);
                    compression_rle.push((count - 3) as u8);
                    count = 0;
                } else {
                    let this_count = count.min(3 + 16 + 255);
                    encoder.histo_one(COMPRESSION_RLE_LARGE as u32);
                    compression_rle.push(COMPRESSION_RLE_LARGE);
                    encoder.histo_one(((this_count - 3 - 16) >> 4) as u32);
                    compression_rle.push(((this_count - 3 - 16) >> 4) as u8);
                    encoder.histo_one(((this_count - 3 - 16) & 15) as u32);
                    compression_rle.push(((this_count - 3 - 16) & 15) as u8);
                    count -= this_count;
                }
            }
            if curcomp != lastcomp {
                encoder.histo_one(curcomp as u32);
                compression_rle.push(curcomp);
                lastcomp = curcomp;
            }
        }
    }

    let lengthbits = bits_for_value(max_complen as u64);
    let selfbits = bits_for_value(max_self as u64);
    let parentbits = bits_for_value(max_parent);

    // --- compute + export the tree, then encode the RLE token stream ---
    let mut bitbuf = BitWriter::new();
    encoder.compute_tree_from_histo().expect("map huffman tree");
    encoder.export_tree_rle(&mut bitbuf);
    for &b in &compression_rle {
        encoder.encode_one(&mut bitbuf, b as u32);
    }

    // --- per-entry extra data (re-walking the RLE stream to recover each hunk's type) ---
    let mut lastcomp2: u8 = 0;
    let mut count2: i64 = 0;
    let mut src = 0usize;
    let mut firstoffs: u64 = 0;
    for hunknum in 0..hunk_count {
        let base = hunknum as usize * 12;
        let length = get_u24be(&rawmap[base + 1..]);
        let offset = get_u48be(&rawmap[base + 4..]);
        let crc = get_u16be(&rawmap[base + 10..]);

        if count2 == 0 {
            let val = compression_rle[src];
            src += 1;
            if val == COMPRESSION_RLE_SMALL {
                count2 = 2 + compression_rle[src] as i64;
                src += 1;
            } else if val == COMPRESSION_RLE_LARGE {
                count2 = 2 + 16 + ((compression_rle[src] as i64) << 4);
                src += 1;
                count2 += compression_rle[src] as i64;
                src += 1;
            } else {
                lastcomp2 = val;
            }
        } else {
            count2 -= 1;
        }

        match lastcomp2 {
            0..=3 => {
                bitbuf.write(length, lengthbits as i32);
                bitbuf.write(crc as u32, 16);
                if firstoffs == 0 {
                    firstoffs = offset;
                }
            }
            COMPRESSION_NONE => {
                bitbuf.write(crc as u32, 16);
                if firstoffs == 0 {
                    firstoffs = offset;
                }
            }
            COMPRESSION_SELF => {
                bitbuf.write(offset as u32, selfbits as i32);
            }
            COMPRESSION_PARENT => {
                bitbuf.write(offset as u32, parentbits as i32);
            }
            // compact pseudo-codecs carry no extra data
            COMPRESSION_SELF_0
            | COMPRESSION_SELF_1
            | COMPRESSION_PARENT_SELF
            | COMPRESSION_PARENT_0
            | COMPRESSION_PARENT_1 => {}
            _ => {}
        }
    }

    let bitstream = bitbuf.flush();
    let complen = bitstream.len() as u32;

    let mut out = vec![0u8; 16 + bitstream.len()];
    out[0..4].copy_from_slice(&complen.to_be_bytes());
    put_u48be(&mut out[4..10], firstoffs);
    out[10..12].copy_from_slice(&mapcrc.to_be_bytes());
    out[12] = lengthbits;
    out[13] = selfbits;
    out[14] = parentbits;
    out[15] = 0;
    out[16..].copy_from_slice(&bitstream);
    out
}

/// Write a complete **uncompressed** (compression = none) V5 CHD containing `data`,
/// byte-identical to `chdman createraw -c none -hs <hunk_bytes> -us <unit_bytes>`.
///
/// Layout (verified against chdman 0.288): 124-byte header, then a `hunk_count × 4`-byte map
/// (each entry = the hunk's file offset divided by `hunk_bytes`), zero-padding up to the next
/// `hunk_bytes` boundary, then the raw hunks (the final hunk zero-padded to `hunk_bytes`).
/// chdman leaves the SHA-1 fields zero for uncompressed CHDs and does not verify them.
pub fn write_raw_uncompressed<W: Write + Seek>(
    out: &mut W,
    data: &[u8],
    hunk_bytes: u32,
    unit_bytes: u32,
) -> Result<()> {
    write_uncompressed_inner(out, data, hunk_bytes, unit_bytes, &[])
}

/// Core of [`write_raw_uncompressed`] with metadata support (used by `createhd -c none`).
///
/// `metadata` (possibly empty) is written **after the map and before the data** (so the header's
/// `meta_offset` is `V5_HEADER_SIZE + map_size`, and `data_start` rounds up past the metadata to
/// the next hunk boundary), exactly as chdman lays out an uncompressed `createhd`. The SHA-1 fields
/// stay zero (chdman computes no SHA-1 for uncompressed CHDs).
pub(crate) fn write_uncompressed_inner<W: Write + Seek>(
    out: &mut W,
    data: &[u8],
    hunk_bytes: u32,
    unit_bytes: u32,
    metadata: &[MetaEntry],
) -> Result<()> {
    if hunk_bytes == 0 || unit_bytes == 0 || hunk_bytes % unit_bytes != 0 {
        return Err(Error::InvalidParameter);
    }

    let logical_bytes = data.len() as u64;
    let hunk_bytes_u64 = hunk_bytes as u64;
    let hunk_count = round_up(logical_bytes, hunk_bytes_u64) / hunk_bytes_u64;
    let map_offset: u64 = V5_HEADER_SIZE as u64;
    let map_size = hunk_count * 4;

    // metadata sits right after the map; the data rounds up past it to a hunk boundary.
    let meta_start = map_offset + map_size;
    let meta_blob = build_metadata_blob(metadata, meta_start);
    let meta_offset = if meta_blob.is_empty() { 0 } else { meta_start };
    let data_start = round_up(meta_start + meta_blob.len() as u64, hunk_bytes_u64);

    // --- header (124 bytes, big-endian) ---
    let mut hdr = [0u8; V5_HEADER_SIZE as usize];
    hdr[0..8].copy_from_slice(CHD_MAGIC);
    hdr[8..12].copy_from_slice(&V5_HEADER_SIZE.to_be_bytes());
    hdr[12..16].copy_from_slice(&5u32.to_be_bytes());
    // compression[4] = 0 (none) — already zero
    hdr[32..40].copy_from_slice(&logical_bytes.to_be_bytes());
    hdr[40..48].copy_from_slice(&map_offset.to_be_bytes());
    hdr[48..56].copy_from_slice(&meta_offset.to_be_bytes());
    hdr[56..60].copy_from_slice(&hunk_bytes.to_be_bytes());
    hdr[60..64].copy_from_slice(&unit_bytes.to_be_bytes());
    // raw_sha1 (64), sha1 (84), parent_sha1 (104) = 0 for uncompressed CHDs
    out.write_all(&hdr)?;

    // --- map: each entry = (data_start + i*hunk_bytes) / hunk_bytes ---
    let base = (data_start / hunk_bytes_u64) as u32;
    for i in 0..hunk_count as u32 {
        out.write_all(&(base + i).to_be_bytes())?;
    }

    // --- metadata blob, then pad up to the data start ---
    out.write_all(&meta_blob)?;
    let pad = data_start - (meta_start + meta_blob.len() as u64);
    if pad > 0 {
        out.write_all(&vec![0u8; pad as usize])?;
    }

    // --- raw hunks (zero-pad the final hunk) ---
    for i in 0..hunk_count {
        let start = (i * hunk_bytes_u64) as usize;
        let end = ((i + 1) * hunk_bytes_u64).min(logical_bytes) as usize;
        out.write_all(&data[start..end])?;
        let short = hunk_bytes as usize - (end - start);
        if short > 0 {
            out.write_all(&vec![0u8; short])?;
        }
    }

    Ok(())
}

/// Write an **empty uncompressed diff** CHD: a V5 `compression = none` file whose 4-byte map is
/// all zeros (every hunk reads from the parent) with `parent_sha1` set, plus the (cloned) metadata.
/// No data hunks are written; the file ends padded to the first hunk-aligned offset so the runtime
/// writer ([`crate::hd::HdImage`]) can append materialised hunks there. Layout matches an
/// uncompressed CHD (`header → map → metadata → pad`), exactly what MAME's `create(.., parent)`
/// produces for a diff and what its (and chd-rs's) reader expects.
pub(crate) fn write_empty_diff<W: Write + Seek>(
    out: &mut W,
    logical_bytes: u64,
    hunk_bytes: u32,
    unit_bytes: u32,
    parent_sha1: &[u8; 20],
    metadata: &[MetaEntry],
) -> Result<()> {
    if hunk_bytes == 0 || unit_bytes == 0 || hunk_bytes % unit_bytes != 0 {
        return Err(Error::InvalidParameter);
    }
    let hunk_bytes_u64 = hunk_bytes as u64;
    let hunk_count = round_up(logical_bytes, hunk_bytes_u64) / hunk_bytes_u64;
    let map_offset = V5_HEADER_SIZE as u64;
    let map_size = hunk_count * 4;

    let meta_start = map_offset + map_size;
    let meta_blob = build_metadata_blob(metadata, meta_start);
    let meta_offset = if meta_blob.is_empty() { 0 } else { meta_start };
    let data_start = round_up(meta_start + meta_blob.len() as u64, hunk_bytes_u64);

    let mut hdr = [0u8; V5_HEADER_SIZE as usize];
    hdr[0..8].copy_from_slice(CHD_MAGIC);
    hdr[8..12].copy_from_slice(&V5_HEADER_SIZE.to_be_bytes());
    hdr[12..16].copy_from_slice(&5u32.to_be_bytes());
    // compression[0..4] = 0 (none) — already zero
    hdr[32..40].copy_from_slice(&logical_bytes.to_be_bytes());
    hdr[40..48].copy_from_slice(&map_offset.to_be_bytes());
    hdr[48..56].copy_from_slice(&meta_offset.to_be_bytes());
    hdr[56..60].copy_from_slice(&hunk_bytes.to_be_bytes());
    hdr[60..64].copy_from_slice(&unit_bytes.to_be_bytes());
    // raw_sha1 (64) + sha1 (84) stay zero (uncompressed); parent_sha1 (104) links the parent.
    hdr[104..124].copy_from_slice(parent_sha1);
    out.write_all(&hdr)?;

    // all-zero map: every hunk falls through to the parent until written.
    out.write_all(&vec![0u8; map_size as usize])?;

    out.write_all(&meta_blob)?;
    let pad = data_start - (meta_start + meta_blob.len() as u64);
    if pad > 0 {
        out.write_all(&vec![0u8; pad as usize])?;
    }
    Ok(())
}

/// Read all of `reader` into memory and zero-pad to `logical_size` (or the read length if 0),
/// validating `hunk_size`/`unit_size` and that the logical size is unit-aligned and ≥ the input.
/// Shared by the `hd`/`dvd`/`copy` create paths (the in-memory writer needs the whole image).
pub(crate) fn read_and_pad<R: Read>(
    mut reader: R,
    logical_size: u64,
    unit_size: u32,
    hunk_size: u32,
) -> Result<Vec<u8>> {
    if unit_size == 0 || hunk_size == 0 || hunk_size % unit_size != 0 {
        return Err(Error::InvalidParameter);
    }
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    let logical = if logical_size != 0 {
        logical_size
    } else {
        data.len() as u64
    };
    if logical % u64::from(unit_size) != 0 || data.len() as u64 > logical {
        return Err(Error::InvalidParameter);
    }
    data.resize(logical as usize, 0);
    Ok(data)
}

/// Create dispatch for the `hd`/`dvd`/`copy` modules: write `data` (already padded to the logical
/// size) with the given codec list + metadata, choosing the uncompressed or compressed writer and
/// adapting the numeric per-hunk callback to a [`CompressionProgress`]. An empty `codecs` writes an
/// uncompressed CHD (no per-hunk progress hook; `cancel` checked once up front).
pub(crate) fn write_create<W: Write + Seek>(
    out: &mut W,
    data: &[u8],
    hunk_size: u32,
    unit_size: u32,
    codecs: &[CodecType],
    metadata: &[MetaEntry],
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let logical = data.len() as u64;
    if codecs.is_empty() {
        if cancel() {
            return Err(Error::Cancelled);
        }
        write_uncompressed_inner(out, data, hunk_size, unit_size, metadata)?;
        progress(CompressionProgress {
            bytes_done: logical,
            bytes_total: logical,
            ratio: 1.0,
        });
        return Ok(());
    }

    let mut prog = |done: u64, total: u64, comp: u64| {
        progress(CompressionProgress {
            bytes_done: done,
            bytes_total: total,
            ratio: if done == 0 {
                1.0
            } else {
                comp as f64 / done as f64
            },
        });
    };
    write_raw_inner(
        out, data, hunk_size, unit_size, codecs, metadata, None, &mut prog, cancel,
    )
}
