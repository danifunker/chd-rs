/// Common logic for CD-ROM decompression codecs.
use crate::cdrom::{
    CD_FRAME_SIZE, CD_MAX_SECTOR_DATA, CD_MAX_SUBCODE_DATA, CD_SYNC_HEADER, CD_SYNC_NUM_BYTES,
};
use crate::compression::ecc::ErrorCorrectedSector;
use crate::compression::lzma::LzmaCodec;
use crate::compression::zlib::ZlibCodec;
use crate::compression::zstd::ZstdCodec;
use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::error::{Error, Result};
use crate::header::CodecType;
use std::convert::TryFrom;

/// CD-ROM wrapper decompression codec (cdlz) that uses the [LZMA codec](crate::codecs::LzmaCodec)
/// for decompression of sector data and the [Deflate codec](crate::codecs::ZlibCodec) for
/// decompression of subcode data.
///
/// ## Format Details
/// CD-ROM compressed hunks have a layout with all compressed frame data in sequential order,
/// followed by compressed subcode data.
/// ```c
/// [Header, Frame0, Frame1, ..., FrameN, Subcode0, Subcode1, ..., SubcodeN]
/// ```
///
/// The slice of the input buffer from `Frame0` to `Frame1` is a single LZMA compressed stream,
/// followed by the subcode data which is a single Deflate compressed stream.
///
/// The size of the header is determined by the number of 2448-byte sized frames that can fit
/// into a hunk-sized buffer and the length of such buffer. First, the number of ECC bytes
/// are calculated as `(frames + 7) / 8`. If the hunk size is less than 65536 (0x10000) bytes,
/// then the length of the compressed sector data is stored as a 2 byte big-endian integer,
/// otherwise the length is 3 bytes, stored after the number of ECC bytes in the header.
///
/// After decompression, the data is swizzled so that each frame is followed by its corresponding
/// subcode data.
/// ```c
/// [Frame0, Subcode0, Frame1, Subcode1, ..., FrameN, SubcodeN]
/// ```
/// After swizzling, the following CD sync header will be written to
/// the first 12 bytes of each frame.
/// ```
/// pub const CD_SYNC_HEADER: [u8; 12] = [
///     0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
/// ];
/// ```
/// The ECC data is then regenerated throughout the sector.
///
/// ## Buffer Restrictions
/// Each compressed CDLZ hunk decompresses to a hunk-sized chunk. The hunk size must be a multiple of
/// 2448, the size of each CD frame.
/// The input buffer must contain exactly enough data to fill the hunk-sized output buffer
/// when decompressed.
pub type CdLzmaCodec = CdCodec<LzmaCodec, ZlibCodec>;

/// CD-ROM wrapper decompression codec (cdzl) using the [Deflate codec](crate::codecs::ZlibCodec)
/// for decompression of sector data and the [Deflate codec](crate::codecs::ZlibCodec) for
/// decompression of subcode data.
///
/// ## Format Details
/// CD-ROM compressed hunks have a layout with a header, then all compressed frame data
/// in sequential order, followed by compressed subcode data.
/// ```c
/// [Header, Frame0, Frame1, ..., FrameN, Subcode0, Subcode1, ..., SubcodeN]
/// ```
///
/// The slice of the input buffer from `Frame0` to `Frame1` is a single Deflate compressed stream,
/// followed by the subcode data which is a single Deflate compressed stream.
///
/// The size of the header is determined by the number of 2448-byte sized frames that can fit
/// into a hunk-sized buffer and the length of such buffer. First, the number of ECC bytes
/// are calculated as `(frames + 7) / 8`. If the hunk size is less than 65536 (0x10000) bytes,
/// then the length of the compressed sector data is stored as a 2 byte big-endian integer,
/// otherwise the length is 3 bytes, stored after the number of ECC bytes in the header.
///
/// After decompression, the data is swizzled so that each frame is followed by its corresponding
/// subcode data.
///
/// ```c
/// [Frame0, Subcode0, Frame1, Subcode1, ..., FrameN, SubcodeN]
/// ```
/// After swizzling, the following CD sync header will be written to
/// the first 12 bytes of each frame.
/// ```
/// pub const CD_SYNC_HEADER: [u8; 12] = [
///     0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
/// ];
/// ```
/// The ECC data is then regenerated throughout the sector.
///
/// ## Buffer Restrictions
/// Each compressed CDZL hunk decompresses to a hunk-sized chunk. The hunk size must be a multiple of
/// 2448, the size of each CD frame.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
pub type CdZlibCodec = CdCodec<ZlibCodec, ZlibCodec>;

/// CD-ROM wrapper decompression codec (cdzs) using the [Zstandard codec](crate::codecs::ZstdCodec)
/// for decompression of sector data and the [Zstandard codec](crate::codecs::ZstdCodec) for
/// decompression of subcode data.
///
/// ## Format Details
/// CD-ROM compressed hunks have a layout with a header, then all compressed frame data
/// in sequential order, followed by compressed subcode data.
/// ```c
/// [Header, Frame0, Frame1, ..., FrameN, Subcode0, Subcode1, ..., SubcodeN]
/// ```
///
/// The slice of the input buffer from `Frame0` to `Frame1` is a single Deflate compressed stream,
/// followed by the subcode data which is a single Deflate compressed stream.
///
/// The size of the header is determined by the number of 2448-byte sized frames that can fit
/// into a hunk-sized buffer and the length of such buffer. First, the number of ECC bytes
/// are calculated as `(frames + 7) / 8`. If the hunk size is less than 65536 (0x10000) bytes,
/// then the length of the compressed sector data is stored as a 2 byte big-endian integer,
/// otherwise the length is 3 bytes, stored after the number of ECC bytes in the header.
///
/// After decompression, the data is swizzled so that each frame is followed by its corresponding
/// subcode data.
///
/// ```c
/// [Frame0, Subcode0, Frame1, Subcode1, ..., FrameN, SubcodeN]
/// ```
/// After swizzling, the following CD sync header will be written to
/// the first 12 bytes of each frame.
/// ```
/// pub const CD_SYNC_HEADER: [u8; 12] = [
///     0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
/// ];
/// ```
/// The ECC data is then regenerated throughout the sector.
///
/// ## Buffer Restrictions
/// Each compressed CDZS hunk decompresses to a hunk-sized chunk. The hunk size must be a multiple of
/// 2448, the size of each CD frame.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
pub type CdZstdCodec = CdCodec<ZstdCodec, ZstdCodec>;

impl CompressionCodecType for CdLzmaCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::LzmaCdV5
    }
}

impl CompressionCodecType for CdZlibCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::ZLibCdV5
    }
}

impl CompressionCodecType for CdZstdCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::ZstdCdV5
    }
}

impl CompressionCodec for CdZlibCodec {}
impl CompressionCodec for CdLzmaCodec {}
impl CompressionCodec for CdZstdCodec {}

// unstable(adt_const_params): const TYPE: CodecType, but marker traits bring us
// most of the way.
/// CD-ROM codec wrapper.
pub struct CdCodec<Engine: CodecImplementation, SubEngine: CodecImplementation> {
    engine: Engine,
    sub_engine: SubEngine,
    buffer: Vec<u8>,
}

impl<Engine: CodecImplementation, SubEngine: CodecImplementation> CodecImplementation
    for CdCodec<Engine, SubEngine>
{
    fn new(hunk_size: u32) -> Result<Self> {
        if hunk_size % CD_FRAME_SIZE != 0 {
            return Err(Error::CodecError);
        }

        let buffer = vec![0u8; hunk_size as usize];
        Ok(CdCodec {
            engine: Engine::new((hunk_size / CD_FRAME_SIZE) * CD_MAX_SECTOR_DATA)?,
            sub_engine: SubEngine::new((hunk_size / CD_FRAME_SIZE) * CD_MAX_SUBCODE_DATA)?,
            buffer,
        })
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult> {
        // https://github.com/rtissera/libchdr/blob/cdcb714235b9ff7d207b703260706a364282b063/src/libchdr_chd.c#L647
        let frames = output.len() / CD_FRAME_SIZE as usize;
        let complen_bytes = if output.len() < 65536 { 2 } else { 3 };
        let ecc_bytes = (frames + 7) / 8;
        let header_bytes = ecc_bytes + complen_bytes;

        // Extract compressed length of base
        #[allow(clippy::identity_op)]
        let mut sector_compressed_len: u32 =
            (input[ecc_bytes + 0] as u32) << 8 | input[ecc_bytes + 1] as u32;
        if complen_bytes > 2 {
            sector_compressed_len = sector_compressed_len << 8 | input[ecc_bytes + 2] as u32;
        }

        // decode frame data
        let frame_res = self.engine.decompress(
            &input[header_bytes..][..sector_compressed_len as usize],
            &mut self.buffer[..frames * CD_MAX_SECTOR_DATA as usize],
        )?;

        #[cfg(feature = "want_subcode")]
        let sub_res = self.sub_engine.decompress(
            &input[header_bytes + sector_compressed_len as usize..],
            &mut self.buffer[frames * CD_MAX_SECTOR_DATA as usize..]
                [..frames * CD_MAX_SUBCODE_DATA as usize],
        )?;

        #[cfg(not(feature = "want_subcode"))]
        let sub_res = DecompressResult::default();

        // Decompressed data has layout
        // [Frame0, Frame1, ..., FrameN, Subcode0, Subcode1, ..., SubcodeN]
        // We need to reassemble the data to be
        // [Frame0, Subcode0, Frame1, Subcode1, ..., FrameN, SubcodeN]

        // Reassemble frame data to expected layout.
        for (frame_num, chunk) in self.buffer[..frames * CD_MAX_SECTOR_DATA as usize]
            .chunks_exact(CD_MAX_SECTOR_DATA as usize)
            .enumerate()
        {
            output[frame_num * CD_FRAME_SIZE as usize..][..CD_MAX_SECTOR_DATA as usize]
                .copy_from_slice(chunk);
        }

        // Reassemble subcode data to expected layout.
        #[cfg(feature = "want_subcode")]
        for (frame_num, chunk) in self.buffer[frames * CD_MAX_SECTOR_DATA as usize..]
            .chunks_exact(CD_MAX_SUBCODE_DATA as usize)
            .enumerate()
        {
            output[frame_num * CD_FRAME_SIZE as usize + CD_MAX_SECTOR_DATA as usize..]
                [..CD_MAX_SUBCODE_DATA as usize]
                .copy_from_slice(chunk);
        }

        // Recreate ECC data
        #[cfg(feature = "want_raw_data_sector")]
        for frame_num in 0..frames {
            let mut sector = <&mut [u8; CD_MAX_SECTOR_DATA as usize]>::try_from(
                &mut output[frame_num * CD_FRAME_SIZE as usize..][..CD_MAX_SECTOR_DATA as usize],
            )?;
            if (input[frame_num / 8] & (1 << (frame_num % 8))) != 0 {
                sector[0..12].copy_from_slice(&CD_SYNC_HEADER);
                sector.generate_ecc();
            }
        }

        Ok(frame_res + sub_res)
    }
}

/// De-swizzle a hunk of interleaved CD frames (`[sector(2352) ‖ subcode(96)] × frames`) into a
/// sector run (`frames × 2352`) followed by a subcode run (`frames × 96`) at the start of `buffer` —
/// the encode inverse of the decoder's re-swizzle. Shared by the CD wrapper encoders
/// (`cdlz`/`cdzl`/`cdzs` via [`CdEncoder`], and `cdfl`).
#[cfg(feature = "write")]
pub(crate) fn deswizzle_cd_frames(input: &[u8], buffer: &mut [u8], frames: usize) {
    let (sect, sub, frame) = (
        CD_MAX_SECTOR_DATA as usize,
        CD_MAX_SUBCODE_DATA as usize,
        CD_FRAME_SIZE as usize,
    );
    let sect_total = frames * sect;
    for f in 0..frames {
        let src = &input[f * frame..];
        buffer[f * sect..][..sect].copy_from_slice(&src[..sect]);
        buffer[sect_total + f * sub..][..sub].copy_from_slice(&src[sect..][..sub]);
    }
}

/// CD-ROM wrapper **compression** codec — the encode mirror of [`CdCodec`]. Generic over the
/// sector engine `Engine` and subcode engine `SubEngine` (both [`CodecEncodeImplementation`]).
///
/// Reproduces MAME's `chd_cd_compressor::compress` (`chdcodec.cpp:351`): de-swizzles the hunk's
/// interleaved `[sector(2352) ‖ subcode(96)]` frames into a sector run followed by a subcode run;
/// for every frame that is a verifiable data sector (sync header present **and** valid ECC) it sets
/// that frame's bit in the ECC-flag bitmap and zeroes the sync header + ECC P/Q (regenerated on
/// decode); compresses the sector run with `Engine` and the subcode run with `SubEngine`; and emits
/// `[ecc_flags ‖ complen ‖ sector_stream ‖ subcode_stream]` where `complen` (the sector stream
/// length) is 2 bytes when the hunk is `< 65536` else 3. Returns [`Error::CompressionError`] if the
/// sector stream alone is not smaller than the hunk (MAME's `complen >= srclen` check).
#[cfg(feature = "write")]
pub struct CdEncoder<Engine, SubEngine> {
    engine: Engine,
    sub_engine: SubEngine,
    buffer: Vec<u8>,
}

#[cfg(feature = "write")]
impl<Engine, SubEngine> crate::compression::CodecEncodeImplementation
    for CdEncoder<Engine, SubEngine>
where
    Engine: crate::compression::CodecEncodeImplementation,
    SubEngine: crate::compression::CodecEncodeImplementation,
{
    fn new(hunk_size: u32) -> Result<Self> {
        if hunk_size % CD_FRAME_SIZE != 0 {
            return Err(Error::CodecError);
        }
        let frames = hunk_size / CD_FRAME_SIZE;
        Ok(CdEncoder {
            engine: Engine::new(frames * CD_MAX_SECTOR_DATA)?,
            sub_engine: SubEngine::new(frames * CD_MAX_SUBCODE_DATA)?,
            buffer: vec![0u8; (frames * (CD_MAX_SECTOR_DATA + CD_MAX_SUBCODE_DATA)) as usize],
        })
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let frames = input.len() / CD_FRAME_SIZE as usize;
        let complen_bytes = if input.len() < 65536 { 2 } else { 3 };
        let ecc_bytes = frames.div_ceil(8);
        let header_bytes = ecc_bytes + complen_bytes;
        let sect_total = frames * CD_MAX_SECTOR_DATA as usize;
        let sub_total = frames * CD_MAX_SUBCODE_DATA as usize;

        deswizzle_cd_frames(input, &mut self.buffer, frames);

        // strip the sync header + ECC of verifiable data sectors, recording which in the bitmap
        let mut ecc_flags = vec![0u8; ecc_bytes];
        for f in 0..frames {
            let off = f * CD_MAX_SECTOR_DATA as usize;
            let mut sector = <&mut [u8; CD_MAX_SECTOR_DATA as usize]>::try_from(
                &mut self.buffer[off..off + CD_MAX_SECTOR_DATA as usize],
            )?;
            if sector[..CD_SYNC_NUM_BYTES] == CD_SYNC_HEADER && sector.verify_ecc() {
                ecc_flags[f / 8] |= 1 << (f % 8);
                sector[..CD_SYNC_NUM_BYTES].fill(0);
                sector.clear_ecc();
            }
        }

        // compress the sector run after the header; bail (NONE fallback) if it doesn't shrink
        let base_n = self
            .engine
            .compress(&self.buffer[..sect_total], &mut output[header_bytes..])?;
        if base_n >= input.len() {
            return Err(Error::CompressionError);
        }

        // compress the subcode run after the sector stream
        let sub_n = self.sub_engine.compress(
            &self.buffer[sect_total..sect_total + sub_total],
            &mut output[header_bytes + base_n..],
        )?;

        // header: ECC-flag bitmap followed by the sector-stream length (BE, 2 or 3 bytes)
        output[..ecc_bytes].copy_from_slice(&ecc_flags);
        if complen_bytes > 2 {
            output[ecc_bytes] = (base_n >> 16) as u8;
            output[ecc_bytes + 1] = (base_n >> 8) as u8;
            output[ecc_bytes + 2] = base_n as u8;
        } else {
            output[ecc_bytes] = (base_n >> 8) as u8;
            output[ecc_bytes + 1] = base_n as u8;
        }

        Ok(header_bytes + base_n + sub_n)
    }
}

#[cfg(feature = "write")]
impl<Engine, SubEngine> crate::compression::CompressionEncoder for CdEncoder<Engine, SubEngine>
where
    Engine: crate::compression::CodecEncodeImplementation + Send + Sync,
    SubEngine: crate::compression::CodecEncodeImplementation + Send + Sync,
{
}

#[cfg(all(
    test,
    feature = "write",
    feature = "want_raw_data_sector",
    feature = "want_subcode"
))]
mod cd_encode_tests {
    use super::{CdEncoder, CdLzmaCodec, CdZlibCodec};
    use crate::cdrom::{CD_FRAME_SIZE, CD_MAX_SECTOR_DATA, CD_MODE_OFFSET, CD_SYNC_HEADER};
    use crate::compression::ecc::ErrorCorrectedSector;
    use crate::compression::lzma::LzmaEncoder;
    use crate::compression::zlib::ZlibEncoder;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    /// A CD hunk of `frames` 2448-byte frames. Even frames are MODE1 data sectors (sync header +
    /// freshly-generated valid P/Q ECC, so the encoder strips them); odd frames are audio (no sync,
    /// stored verbatim). Subcode is pseudo-random.
    fn build_cd_hunk(frames: usize) -> Vec<u8> {
        let mut x: u32 = 0x9e37_79b9;
        let mut rng = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xff) as u8
        };
        let mut hunk = vec![0u8; frames * CD_FRAME_SIZE as usize];
        for f in 0..frames {
            let frame = &mut hunk[f * CD_FRAME_SIZE as usize..][..CD_FRAME_SIZE as usize];
            let (sector, subcode) = frame.split_at_mut(CD_MAX_SECTOR_DATA as usize);
            if f % 2 == 0 {
                sector[..CD_SYNC_HEADER.len()].copy_from_slice(&CD_SYNC_HEADER);
                sector[CD_MODE_OFFSET] = 1; // MODE1
                for b in &mut sector[16..] {
                    *b = rng();
                }
                // overwrite the P/Q ECC area with valid codes so the encoder strips this sector
                let mut s =
                    <&mut [u8; CD_MAX_SECTOR_DATA as usize]>::try_from(&mut sector[..]).unwrap();
                s.generate_ecc();
            } else {
                for b in sector.iter_mut() {
                    *b = rng();
                }
            }
            for b in subcode.iter_mut() {
                *b = rng();
            }
        }
        hunk
    }

    fn roundtrip<E, S, D>(mut enc: CdEncoder<E, S>, mut dec: D, hunk_size: u32)
    where
        E: CodecEncodeImplementation,
        S: CodecEncodeImplementation,
        D: CodecImplementation,
    {
        let hunk = build_cd_hunk((hunk_size / CD_FRAME_SIZE) as usize);
        let mut comp = vec![0u8; hunk_size as usize];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(n < hunk.len(), "expected the CD hunk to shrink");

        let mut out = vec![0u8; hunk_size as usize];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), hunk_size as usize);
        assert_eq!(out, hunk, "CD codec round-trip mismatch");
    }

    #[test]
    fn cd_zlib_roundtrips() {
        let hs = 8 * CD_FRAME_SIZE;
        roundtrip::<ZlibEncoder, ZlibEncoder, _>(
            CdEncoder::new(hs).unwrap(),
            CdZlibCodec::new(hs).unwrap(),
            hs,
        );
    }

    #[test]
    fn cd_lzma_roundtrips() {
        let hs = 8 * CD_FRAME_SIZE;
        roundtrip::<LzmaEncoder, ZlibEncoder, _>(
            CdEncoder::new(hs).unwrap(),
            CdLzmaCodec::new(hs).unwrap(),
            hs,
        );
    }
}
