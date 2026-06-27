use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::header::CodecType;
use crate::Error;

/// Zstandard (zstd) decompression codec.
///
/// ## Format Details
/// CHD compresses Zstandard hunks with the streaming compressor.
///
/// ## Buffer Restrictions
/// Each compressed Zstandard hunk decompresses to a hunk-sized chunk.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
#[cfg(not(feature = "fast_zstd"))]
pub struct ZstdCodec {
    decoder: ruzstd::decoding::FrameDecoder,
}

/// Zstandard (zstd) decompression codec.
///
/// ## Format Details
/// CHD compresses Zstandard hunks with the streaming compressor.
///
/// ## Buffer Restrictions
/// Each compressed Zstandard hunk decompresses to a hunk-sized chunk.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
#[cfg(feature = "fast_zstd")]
pub struct ZstdCodec {
    zstd_context: zstd_safe::DCtx<'static>,
}

#[cfg(not(feature = "fast_zstd"))]
impl CodecImplementation for ZstdCodec {
    fn new(_hunk_size: u32) -> crate::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self {
            decoder: ruzstd::decoding::FrameDecoder::new(),
        })
    }

    fn decompress(
        &mut self,
        mut input: &[u8],
        output: &mut [u8],
    ) -> crate::Result<DecompressResult> {
        let bytes_out = self
            .decoder
            .decode_all(&mut input, output)
            .map_err(|e| Error::DecompressionError)?;

        // If each chunk doesn't output to exactly the same then it's an error
        if bytes_out != output.len() {
            return Err(Error::DecompressionError);
        }

        Ok(DecompressResult {
            bytes_out,
            // The "read" value returned by decode_from_to() would be incorrect here,
            // since reset() modifies the slice length.
            // bytes_read_from_source() appears to return the whole block length.
            bytes_read: self.decoder.bytes_read_from_source() as usize,
        })
    }
}

#[cfg(feature = "fast_zstd")]
impl CodecImplementation for ZstdCodec {
    fn new(_hunk_size: u32) -> crate::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self {
            zstd_context: zstd_safe::DCtx::try_create().ok_or(crate::Error::CodecError)?,
        })
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> crate::Result<DecompressResult> {
        self.zstd_context
            .reset(zstd_safe::ResetDirective::SessionAndParameters)
            .map_err(|_| Error::DecompressionError)?;

        // If each chunk doesn't output to exactly the same then it's an error
        let bytes_out = self
            .zstd_context
            .decompress(output, input)
            .map_err(|_| Error::DecompressionError)?;

        if bytes_out != output.len() {
            return Err(Error::DecompressionError);
        }

        Ok(DecompressResult {
            bytes_out: output.len(),
            // ZSTD_decompress() takes the exact size of a number of frames, so it
            // should've returned an error if it hasn't used the entire input slice.
            bytes_read: input.len(),
        })
    }
}

impl CompressionCodecType for ZstdCodec {
    fn codec_type(&self) -> CodecType
    where
        Self: Sized,
    {
        CodecType::ZstdV5
    }
}

impl CompressionCodec for ZstdCodec {}

/// Zstandard (zstd) compression codec.
///
/// Backed by [`libzstd-bitexact-rs`](https://crates.io/crates/libzstd-bitexact-rs) `=0.155`, a
/// bit-exact pure-Rust port of libzstd **1.5.5** (chdman's version). Reproduces chdman's
/// per-hunk path exactly: level 22 (`ZSTD_maxCLevel`), **unknown pledged size** (so the frame
/// keeps `windowLog` 27 and long-distance matching), no dictionary — i.e.
/// `ZSTD_initCStream(22)` + `ZSTD_compressStream2(.., ZSTD_e_end)`.
#[cfg(feature = "write-zstd")]
pub struct ZstdEncoder;

#[cfg(feature = "write-zstd")]
impl crate::compression::CodecEncodeImplementation for ZstdEncoder {
    fn new(_: u32) -> crate::Result<Self> {
        Ok(ZstdEncoder)
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> crate::Result<usize> {
        // `StreamEncoder::new(22)` = unknown pledged size (do NOT pledge the hunk size — that
        // downsizes windowLog and changes the bytes); `finish` = ZSTD_e_end.
        let mut out = Vec::new();
        libzstd_bitexact_rs::StreamEncoder::new(22)
            .finish(input, &mut out)
            .map_err(|_| Error::CompressionError)?;
        if out.len() > output.len() {
            return Err(Error::CompressionError);
        }
        output[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }
}

#[cfg(feature = "write-zstd")]
impl crate::compression::CompressionEncoder for ZstdEncoder {}

#[cfg(all(test, feature = "write-zstd"))]
mod tests {
    use super::*;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    /// Round-trip proves the integration is **valid**; byte-exactness vs zstd 1.5.5 is
    /// guaranteed by libzstd-bitexact-rs's own differential suite. Because zstd decode is
    /// format-stable, a round-trip cannot catch encoder drift — to guard byte-identity,
    /// compare *compressed* bytes against a 1.5.5 golden (a chd-rs-side differential test).
    #[test]
    fn zstd_encode_roundtrips_through_decoder() {
        let hunk: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

        let mut enc = ZstdEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(n < hunk.len(), "expected compression to shrink the hunk");

        let mut dec = ZstdCodec::new(4096).unwrap();
        let mut out = vec![0u8; 4096];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), 4096);
        assert_eq!(out, hunk);
    }
}
