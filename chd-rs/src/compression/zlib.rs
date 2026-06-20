use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::error::{Error, Result};
use crate::header::CodecType;
use flate2::{Decompress, FlushDecompress};

/// Deflate (zlib) decompression codec.
///
/// ## Format Details
/// CHD compresses Deflate hunks without a zlib header.
///
/// ## Buffer Restrictions
/// Each compressed Deflate hunk decompresses to a hunk-sized chunk.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
pub struct ZlibCodec {
    engine: Decompress,
}

impl CodecImplementation for ZlibCodec {
    fn new(_: u32) -> Result<Self> {
        Ok(ZlibCodec {
            engine: Decompress::new(false),
        })
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult> {
        self.engine.reset(false);
        let status = self
            .engine
            .decompress(input, output, FlushDecompress::Finish)
            .map_err(|_| Error::DecompressionError)?;

        if status == flate2::Status::BufError {
            return Err(Error::DecompressionError);
        }

        let total_out = self.engine.total_out();
        if self.engine.total_out() != output.len() as u64 {
            return Err(Error::DecompressionError);
        }

        Ok(DecompressResult::new(
            total_out as usize,
            self.engine.total_in() as usize,
        ))
    }
}

impl CompressionCodecType for ZlibCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::Zlib
    }
}

impl CompressionCodec for ZlibCodec {}

/// Deflate (zlib) compression codec.
///
/// Backed by [`zlib-bitexact-rs`](https://crates.io/crates/zlib-bitexact-rs), a bit-exact
/// pure-Rust port of stock **zlib 1.3.1**'s deflate. Produces a raw DEFLATE stream (no zlib
/// header/trailer) byte-identical to MAME's
/// `deflateInit2(Z_BEST_COMPRESSION, Z_DEFLATED, -MAX_WBITS, 8, Z_DEFAULT_STRATEGY)` +
/// `deflate(Z_FINISH)`. The decoder ([`ZlibCodec`]) keeps using `flate2` — inflate is
/// unambiguous, so only the encoder needs byte-exactness.
#[cfg(feature = "write")]
pub struct ZlibEncoder;

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for ZlibEncoder {
    fn new(_: u32) -> Result<Self> {
        Ok(ZlibEncoder)
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let compressed = zlib_bitexact_rs::deflate_raw(input);
        // `deflate_raw` always consumes the whole input; the only failure mode is that the raw
        // DEFLATE stream doesn't fit the hunk-sized output buffer (it would expand), in which
        // case the codec "loses" and the writer stores the hunk uncompressed (NONE).
        if compressed.len() > output.len() {
            return Err(Error::CompressionError);
        }
        output[..compressed.len()].copy_from_slice(&compressed);
        Ok(compressed.len())
    }
}

#[cfg(feature = "write")]
impl crate::compression::CompressionEncoder for ZlibEncoder {}

#[cfg(all(test, feature = "write"))]
mod tests {
    use super::*;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    #[test]
    fn zlib_encode_roundtrips_through_decoder() {
        // A compressible pattern so the encoder produces something smaller than the hunk.
        let hunk: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

        let mut enc = ZlibEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(n < hunk.len(), "expected compression to shrink the hunk");

        let mut dec = ZlibCodec::new(4096).unwrap();
        let mut out = vec![0u8; 4096];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), 4096);
        assert_eq!(out, hunk);
    }

    #[test]
    fn zlib_encode_rejects_incompressible_into_tight_buffer() {
        // Pseudo-random (incompressible) data; deflate will need >= input bytes, so a
        // hunk-sized output buffer can't hold it and the encoder must reject.
        let mut x = 0x1234_5678u32;
        let hunk: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xff) as u8
            })
            .collect();

        let mut enc = ZlibEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        assert!(enc.compress(&hunk, &mut comp).is_err());
    }
}
