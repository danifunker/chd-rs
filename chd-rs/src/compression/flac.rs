use std::io::Cursor;
use std::marker::PhantomData;
use std::mem;

use byteorder::{BigEndian, ByteOrder, LittleEndian, WriteBytesExt};
use claxon::frame::FrameReader;

use crate::cdrom::{CD_FRAME_SIZE, CD_MAX_SECTOR_DATA, CD_MAX_SUBCODE_DATA};
use crate::compression::zlib::ZlibCodec;
use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::error::{Error, Result};
use crate::header::CodecType;

/// Generic block decoder for FLAC.
///
/// Defaults assume 2 channel interleaved FLAC.
/// The byte order determines the endianness of the output data.
struct FlacCodec<T: ByteOrder, const CHANNELS: usize = 2> {
    buffer: Vec<i32>,
    _byteorder: PhantomData<T>,
}

impl<T: ByteOrder, const CHANNELS: usize> CodecImplementation for FlacCodec<T, CHANNELS> {
    fn new(hunk_bytes: u32) -> Result<Self>
    where
        Self: Sized,
    {
        if hunk_bytes % (CHANNELS * mem::size_of::<i16>()) as u32 != 0 {
            return Err(Error::CodecError);
        }

        Ok(FlacCodec {
            buffer: Vec::new(),
            _byteorder: PhantomData::default(),
        })
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult> {
        let comp_buf = Cursor::new(input);

        // Number of samples to write to the buffer.
        let sample_len = output.len() / (CHANNELS * mem::size_of::<i16>());

        // We don't need to create a fake header since claxon will read raw FLAC frames just fine.
        // We just need to be careful not to read past the number of blocks in the input buffer.
        let mut frame_read = FrameReader::new(comp_buf);

        let mut cursor = Cursor::new(output);

        // Buffer to hold decompressed FLAC block data.
        let mut block_buf = mem::take(&mut self.buffer);

        // A little bit of a misnomer. 1 'sample' refers to a sample for all channels.
        let mut samples_written = 0;

        while samples_written < sample_len {
            // Loop through all blocks until we have enough samples written.
            match frame_read.read_next_or_eof(block_buf) {
                Ok(Some(block)) => {
                    // We assume 2 channels (by default), so we can use claxon's stereo_samples
                    // iterator for slightly better performance.
                    #[cfg(not(feature = "nonstandard_channel_count"))]
                    for (l, r) in block.stereo_samples() {
                        cursor.write_i16::<T>(l as i16)?;
                        cursor.write_i16::<T>(r as i16)?;
                        samples_written += 1;
                    }

                    // This is generic over number of assumed channels, but is broken effectively
                    // for any value other than 2.
                    // What we really want here is specialization for CHANNELS = 2 ...
                    #[cfg(feature = "nonstandard_channel_count")]
                    for sample in 0..block.len() / block.channels() {
                        for channel in 0..block.channels() {
                            let sample_data = block.sample(channel, sample) as u16;
                            cursor.write_i16::<T>(sample_data as i16)?;
                        }
                        samples_written += 1;
                    }

                    block_buf = block.into_buffer();
                }
                _ => {
                    // If frame_read dies our buffer just gets eaten. The Error return for a failed
                    // read does not expose the inner buffer.
                    return Err(Error::DecompressionError);
                }
            }
        }

        self.buffer = block_buf;
        let bytes_in = frame_read.into_inner().position();
        Ok(DecompressResult::new(
            samples_written * 4,
            bytes_in as usize,
        ))
    }
}

/// Raw FLAC (flac) decompression codec.
///
/// ## Format details
/// Raw FLAC expects the first byte as either 'L' (0x4C) or 'B' (0x42) to indicate the endianness
/// of the output data, followed by the compressed FLAC data.
///
/// FLAC compressed audio data is assumed to be 2-channel 16-bit signed integer PCM.
/// The audio data is decompressed in interleaved format, with the left channel first, then
/// the right channel for each sample, for 32 bits each sample.
///
/// ## Buffer Restrictions
/// Each compressed FLAC hunk decompresses to a hunk-sized chunk.
/// The input buffer must contain enough samples to fill the hunk-sized output buffer.
pub struct RawFlacCodec {
    be: FlacCodec<BigEndian>,
    le: FlacCodec<LittleEndian>,
}

impl CompressionCodec for RawFlacCodec {}

impl CompressionCodecType for RawFlacCodec {
    fn codec_type(&self) -> CodecType
    where
        Self: Sized,
    {
        CodecType::FlacV5
    }
}

impl CodecImplementation for RawFlacCodec {
    fn new(hunk_bytes: u32) -> Result<Self> {
        Ok(RawFlacCodec {
            be: FlacCodec::new(hunk_bytes)?,
            le: FlacCodec::new(hunk_bytes)?,
        })
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult> {
        match input[0] {
            b'L' => self.le.decompress(&input[1..], output),
            b'B' => self.be.decompress(&input[1..], output),
            _ => Err(Error::DecompressionError),
        }
    }
}

/// Port of MAME `chd_flac_compressor::blocksize` (`chdcodec.cpp:1520`): the FLAC block size in
/// samples = `bytes / 4`, halved while `> 2048` (clamped to the ~2k "sweet spot").
#[cfg(feature = "write")]
fn flac_blocksize(bytes: u32) -> u32 {
    let mut bs = bytes / 4;
    while bs > 2048 {
        bs /= 2;
    }
    bs
}

/// Raw FLAC (`flac`) compression codec.
///
/// Backed by [`libflac-rs`](https://docs.rs/libflac-rs), a bit-exact pure-Rust port of the
/// libFLAC 1.4.3 encoder. Reproduces MAME's `chd_flac_compressor`: it encodes the hunk twice as
/// 2-channel 16-bit PCM — once interpreting the bytes little-endian, once big-endian — at FLAC
/// level 8 (block size = [`flac_blocksize`], streamable-subset off, MD5 off, raw frames with no
/// STREAMINFO/seektable), keeps the smaller, and prepends the `'L'`/`'B'` endian flag byte. Ties
/// keep `'L'` (MAME writes `'L'` first and switches to `'B'` only when big-endian is *strictly*
/// smaller).
///
/// ⚠️ **Byte-identity to a given chdman build is libm-dependent.** libflac-rs's floating-point
/// parity is validated against glibc libm, so the output is byte-identical to a glibc-built chdman
/// but may differ by a few bytes from an MSVC/Windows chdman. The output is always
/// round-trip-correct (it decodes back to the original PCM via [`RawFlacCodec`]).
///
/// ## Buffer Restrictions
/// The hunk size must be a multiple of 4 (2 channels × 2 bytes). The compressed FLAC stream plus
/// the 1-byte endian flag must fit in `hunk_size`; otherwise [`compress`](RawFlacEncoder::compress)
/// returns [`Error::CompressionError`] so the writer falls back to storing the hunk uncompressed
/// (matching MAME's `hunkbytes - 1` destination buffer + `complen + 1 >= hunkbytes` checks).
#[cfg(feature = "write")]
pub struct RawFlacEncoder {
    hunk_bytes: u32,
    block_size: u32,
}

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for RawFlacEncoder {
    fn new(hunk_size: u32) -> Result<Self> {
        // 2-channel 16-bit PCM: the hunk must be a whole number of stereo samples.
        if hunk_size % (2 * mem::size_of::<i16>()) as u32 != 0 {
            return Err(Error::CodecError);
        }
        Ok(RawFlacEncoder {
            hunk_bytes: hunk_size,
            block_size: flac_blocksize(hunk_size),
        })
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        // MAME encodes into a `hunkbytes - 1` buffer (1 byte reserved for the endian flag);
        // a stream overflowing it throws COMPRESSION_ERROR -> the driver stores the hunk NONE.
        let cap = self.hunk_bytes as usize - 1;
        let enc = libflac_rs::Encoder::new(libflac_rs::EncoderConfig::chd(self.block_size));

        // little-endian interpretation -> 'L'
        let le: Vec<i32> = input
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
            .collect();
        let le_frames = enc.encode_frames(&le);

        // big-endian interpretation -> 'B'
        let be: Vec<i32> = input
            .chunks_exact(2)
            .map(|b| i16::from_be_bytes([b[0], b[1]]) as i32)
            .collect();
        let be_frames = enc.encode_frames(&be);

        if le_frames.len() > cap || be_frames.len() > cap {
            return Err(Error::CompressionError);
        }

        // pick the smaller; ties keep 'L' (MAME switches to 'B' only when strictly smaller).
        let (flag, frames) = if be_frames.len() < le_frames.len() {
            (b'B', &be_frames)
        } else {
            (b'L', &le_frames)
        };

        let total = frames.len() + 1;
        // `complen + 1 >= hunkbytes` throws in MAME; also guard the caller's buffer.
        if total >= self.hunk_bytes as usize || total > output.len() {
            return Err(Error::CompressionError);
        }
        output[0] = flag;
        output[1..total].copy_from_slice(frames);
        Ok(total)
    }
}

#[cfg(feature = "write")]
impl crate::compression::CompressionEncoder for RawFlacEncoder {}

#[cfg(all(test, feature = "write"))]
mod tests {
    use super::*;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    /// Smooth 16-bit stereo PCM (a bounded triangle wave per channel) that FLAC actually
    /// compresses, so the round-trip exercises the codec instead of overflowing to NONE.
    fn audio_hunk(bytes: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(bytes);
        let (mut l, mut r, mut dl, mut dr): (i32, i32, i32, i32) = (0, 0, 37, 53);
        while v.len() < bytes {
            l += dl;
            if !(-20000..=20000).contains(&l) {
                dl = -dl;
                l += 2 * dl;
            }
            r += dr;
            if !(-18000..=18000).contains(&r) {
                dr = -dr;
                r += 2 * dr;
            }
            v.extend_from_slice(&(l as i16).to_le_bytes());
            v.extend_from_slice(&(r as i16).to_le_bytes());
        }
        v.truncate(bytes);
        v
    }

    /// Cross-crate integration: libflac-rs's raw frames (with the CHD `'L'`/`'B'` endian-trial
    /// wrapper) decode back through chd-rs's existing FLAC decoder and round-trip exactly.
    #[test]
    fn flac_encode_roundtrips_through_decoder() {
        let hunk = audio_hunk(4096);

        let mut enc = RawFlacEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(
            n < hunk.len(),
            "expected flac to shrink the smooth audio hunk"
        );
        assert!(
            comp[0] == b'L' || comp[0] == b'B',
            "missing endian flag byte, got {:#x}",
            comp[0]
        );

        let mut dec = RawFlacCodec::new(4096).unwrap();
        let mut out = vec![0u8; 4096];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), 4096);
        assert_eq!(out, hunk, "flac round-trip mismatch");
    }

    /// Incompressible (random) data should overflow the FLAC stream past the hunk budget, so the
    /// encoder reports `CompressionError` and the writer falls back to NONE (matching MAME).
    #[test]
    fn flac_rejects_incompressible() {
        let mut x: u32 = 0x1234_5678;
        let hunk: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xff) as u8
            })
            .collect();
        let mut enc = RawFlacEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        assert!(enc.compress(&hunk, &mut comp).is_err());
    }
}

/// CD-ROM wrapper decompression codec (cdfl) using the FLAC
/// for decompression of sector data and the [Deflate codec](crate::codecs::ZlibCodec) for
/// decompression of subcode data.
///
/// ## Format Details
/// FLAC compressed audio data is assumed to be 2-channel 16-bit signed integer PCM.
/// The audio data is decompressed in interleaved format, with the left channel first, then
/// the right channel for each sample, for 32 bits each sample.
///
/// CD-ROM wrapped FLAC is always written to the output stream in big-endian byte order.
///
/// CD-ROM compressed hunks have a layout with all compressed frame data in sequential order,
/// followed by compressed subcode data.
///
/// ```c
/// [Frame0, Frame1, ..., FrameN, Subcode0, Subcode1, ..., SubcodeN]
/// ```
/// Unlike CDLZ or CDZL, there is no header before the compressed data begins.
/// The length of the compressed data is determined by the number of 2448-sized frames
/// that can fit into the hunk-sized output buffer. Following the FLAC compressed blocks,
/// the subcode data is a single Deflate stream.
///
/// After decompression, the data is swizzled so that each frame is followed by its corresponding
/// subcode data.
/// ```c
/// [Frame0, Subcode0, Frame1, Subcode1, ..., FrameN, SubcodeN]
/// ```
/// FLAC compressed frames does not require manual reconstruction of the sync header or ECC bytes.
///
/// ## Buffer Restrictions
/// Each compressed CDFL hunk decompresses to a hunk-sized chunk. The hunk size must be a multiple
/// of 2448, the size of each CD frame. The input buffer must contain enough samples to fill
/// the number of CD sectors that can fit into the output buffer.
pub struct CdFlacCodec {
    // cdfl always writes in big endian.
    engine: FlacCodec<BigEndian>,
    sub_engine: ZlibCodec,
    buffer: Vec<u8>,
}

impl CompressionCodec for CdFlacCodec {}

impl CompressionCodecType for CdFlacCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::FlacCdV5
    }
}

impl CodecImplementation for CdFlacCodec {
    fn new(hunk_size: u32) -> Result<Self>
    where
        Self: Sized,
    {
        if hunk_size % CD_FRAME_SIZE != 0 {
            return Err(Error::CodecError);
        }

        // The size of the FLAC data in each cdfl hunk, excluding the subcode data.
        let max_frames = hunk_size / CD_FRAME_SIZE;
        let flac_data_size = max_frames * CD_MAX_SECTOR_DATA;

        // neither FlacCodec nor ZlibCodec actually make use of hunk_size.
        Ok(CdFlacCodec {
            engine: FlacCodec::new(flac_data_size)?,
            sub_engine: ZlibCodec::new(hunk_size)?,
            buffer: vec![0u8; hunk_size as usize],
        })
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult> {
        let total_frames = output.len() / CD_FRAME_SIZE as usize;
        let frame_res = self.engine.decompress(
            input,
            &mut self.buffer[..total_frames * CD_MAX_SECTOR_DATA as usize],
        )?;

        #[cfg(feature = "want_subcode")]
        let sub_res = self.sub_engine.decompress(
            &input[frame_res.total_in()..],
            &mut self.buffer[total_frames * CD_MAX_SECTOR_DATA as usize..]
                [..total_frames * CD_MAX_SUBCODE_DATA as usize],
        )?;

        #[cfg(not(feature = "want_subcode"))]
        let sub_res = DecompressResult::default();

        // Decompressed FLAC data has layout
        // [Frame0, Frame1, ..., FrameN, Subcode0, Subcode1, ..., SubcodeN]
        // We need to reassemble the data to be
        // [Frame0, Subcode0, Frame1, Subcode1, ..., FrameN, SubcodeN]

        // Reassemble frame data to expected layout.
        for (frame_num, chunk) in self.buffer[..total_frames * CD_MAX_SECTOR_DATA as usize]
            .chunks_exact(CD_MAX_SECTOR_DATA as usize)
            .enumerate()
        {
            output[frame_num * CD_FRAME_SIZE as usize..][..CD_MAX_SECTOR_DATA as usize]
                .copy_from_slice(chunk);
        }

        // Reassemble subcode data to expected layout.
        #[cfg(feature = "want_subcode")]
        for (frame_num, chunk) in self.buffer[total_frames * CD_MAX_SECTOR_DATA as usize..]
            .chunks_exact(CD_MAX_SUBCODE_DATA as usize)
            .enumerate()
        {
            output[frame_num * CD_FRAME_SIZE as usize + CD_MAX_SECTOR_DATA as usize..]
                [..CD_MAX_SUBCODE_DATA as usize]
                .copy_from_slice(chunk);
        }

        Ok(frame_res + sub_res)
    }
}

/// Port of MAME `chd_cd_flac_compressor::blocksize` (`chdcodec.cpp:1686`): the FLAC block size in
/// samples = `bytes / 4`, halved while `> 2352` (`MAX_SECTOR_DATA`). Note the threshold is the CD
/// sector size, not the `2048` of the raw-FLAC [`flac_blocksize`].
#[cfg(feature = "write")]
fn cd_flac_blocksize(bytes: u32) -> u32 {
    let mut bs = bytes / 4;
    while bs > CD_MAX_SECTOR_DATA {
        bs /= 2;
    }
    bs
}

/// CD-ROM wrapper **FLAC** compression codec (cdfl) — the encode mirror of [`CdFlacCodec`].
///
/// Reproduces MAME's `chd_cd_flac_compressor::compress` (`chdcodec.cpp:1634`), which is **unlike**
/// the [`CdEncoder`](crate::compression::cdrom::CdEncoder)-based `cdzl`/`cdlz`/`cdzs`: there is **no
/// ECC strip** and **no header**. It de-swizzles the hunk's interleaved `[sector(2352) ‖
/// subcode(96)]` frames into a sector run followed by a subcode run, FLAC-encodes the sector run as
/// 2-channel 16-bit PCM interpreted **big-endian** (matching MAME's host-swap on a little-endian
/// build; the decoder always reads big-endian), raw-deflates the subcode run with the same
/// [`ZlibEncoder`](crate::compression::zlib::ZlibEncoder) the other CD codecs use, and emits
/// `[flac_stream ‖ deflate_stream]` (the FLAC stream is self-delimiting, so no length field is
/// stored). Returns [`Error::CompressionError`] if the result is not smaller than the hunk (MAME's
/// `complen >= srclen`), so the writer falls back to storing the hunk uncompressed.
///
/// ⚠️ **Byte-identity is libm-gated** (see [`RawFlacEncoder`]): validated byte-identical to a
/// **glibc** chdman 0.288, round-trip-correct against any build.
#[cfg(feature = "write")]
pub struct CdFlacEncoder {
    frames: u32,
    block_size: u32,
    sub_engine: super::zlib::ZlibEncoder,
    buffer: Vec<u8>,
}

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for CdFlacEncoder {
    fn new(hunk_size: u32) -> Result<Self> {
        if hunk_size % CD_FRAME_SIZE != 0 {
            return Err(Error::CodecError);
        }
        let frames = hunk_size / CD_FRAME_SIZE;
        Ok(CdFlacEncoder {
            frames,
            block_size: cd_flac_blocksize(frames * CD_MAX_SECTOR_DATA),
            sub_engine: super::zlib::ZlibEncoder::new(frames * CD_MAX_SUBCODE_DATA)?,
            buffer: vec![0u8; hunk_size as usize],
        })
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let frames = self.frames as usize;
        let sect_total = frames * CD_MAX_SECTOR_DATA as usize;
        let sub_total = frames * CD_MAX_SUBCODE_DATA as usize;

        // de-swizzle [sector ‖ subcode] frames into the sector run then the subcode run (no ECC strip)
        for f in 0..frames {
            let src = &input[f * CD_FRAME_SIZE as usize..];
            self.buffer[f * CD_MAX_SECTOR_DATA as usize..][..CD_MAX_SECTOR_DATA as usize]
                .copy_from_slice(&src[..CD_MAX_SECTOR_DATA as usize]);
            self.buffer[sect_total + f * CD_MAX_SUBCODE_DATA as usize..]
                [..CD_MAX_SUBCODE_DATA as usize]
                .copy_from_slice(
                    &src[CD_MAX_SECTOR_DATA as usize..][..CD_MAX_SUBCODE_DATA as usize],
                );
        }

        // FLAC-encode the sector run as big-endian interleaved 16-bit stereo (no endian flag byte).
        let samples: Vec<i32> = self.buffer[..sect_total]
            .chunks_exact(2)
            .map(|b| i16::from_be_bytes([b[0], b[1]]) as i32)
            .collect();
        let enc = libflac_rs::Encoder::new(libflac_rs::EncoderConfig::chd(self.block_size));
        let flac = enc.encode_frames(&samples);
        if flac.len() >= input.len() || flac.len() > output.len() {
            return Err(Error::CompressionError);
        }
        output[..flac.len()].copy_from_slice(&flac);

        // raw-deflate the subcode run directly after the FLAC stream
        let sub_n = self.sub_engine.compress(
            &self.buffer[sect_total..sect_total + sub_total],
            &mut output[flac.len()..],
        )?;

        let total = flac.len() + sub_n;
        if total >= input.len() {
            return Err(Error::CompressionError);
        }
        Ok(total)
    }
}

#[cfg(feature = "write")]
impl crate::compression::CompressionEncoder for CdFlacEncoder {}

#[cfg(all(test, feature = "write", feature = "want_subcode"))]
mod cd_flac_tests {
    use super::*;
    use crate::compression::codecs::CdFlacCodec;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    /// `CdFlacEncoder` → `CdFlacCodec` round-trips a CD hunk of smooth big-endian stereo audio
    /// sectors (so FLAC engages) plus pseudo-random subcode.
    #[test]
    fn cd_flac_roundtrips() {
        let frames = 8usize;
        let hs = frames as u32 * CD_FRAME_SIZE;
        let mut hunk = vec![0u8; hs as usize];
        let (mut l, mut r, mut dl, mut dr): (i32, i32, i32, i32) = (0, 0, 37, 53);
        let mut x = 0x9e37_79b9u32;
        for f in 0..frames {
            let frame = &mut hunk[f * CD_FRAME_SIZE as usize..][..CD_FRAME_SIZE as usize];
            let (sector, subcode) = frame.split_at_mut(CD_MAX_SECTOR_DATA as usize);
            for s in sector.chunks_exact_mut(4) {
                l += dl;
                if !(-20000..=20000).contains(&l) {
                    dl = -dl;
                    l += 2 * dl;
                }
                r += dr;
                if !(-18000..=18000).contains(&r) {
                    dr = -dr;
                    r += 2 * dr;
                }
                s[0..2].copy_from_slice(&(l as i16).to_be_bytes());
                s[2..4].copy_from_slice(&(r as i16).to_be_bytes());
            }
            for b in subcode.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = (x & 0xff) as u8;
            }
        }

        let mut enc = CdFlacEncoder::new(hs).unwrap();
        let mut comp = vec![0u8; hs as usize];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(n < hunk.len(), "expected cdfl to shrink the audio hunk");

        let mut dec = CdFlacCodec::new(hs).unwrap();
        let mut out = vec![0u8; hs as usize];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), hs as usize);
        assert_eq!(out, hunk, "cdfl round-trip mismatch");
    }
}
