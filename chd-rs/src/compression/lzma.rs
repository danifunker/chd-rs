use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::error::{Error, Result};
use crate::header::CodecType;
use lzma_rs::decompress::raw::{LzmaDecoder, LzmaParams, LzmaProperties};
use std::io::Cursor;
/// LZMA (lzma) decompression codec.
///
/// ## Format Details
/// CHD compresses LZMA hunks without an xz stream header using
/// the default compression settings for LZMA 19.0. These settings are
///
/// * Literal Context Bits (`lc`): 3
/// * Literal Position Bits (`lp`): 0
/// * Position Bits (`pb`): 2
///
/// The dictionary size is determined via the following algorithm with a level of 9, and a
/// reduction size of hunk size.
///
/// ```rust
/// fn get_lzma_dict_size(level: u32, reduce_size: u32) -> u32 {
///     let mut dict_size = if level <= 5 {
///         1 << (level * 2 + 14)
///     } else if level <= 7 {
///         1 << 25
///     } else {
///        1 << 26
///     };
///
///     // This does the same thing as LzmaEnc.c when determining dict_size
///     if dict_size > reduce_size {
///         for i in 11..=30 {
///             if reduce_size <= (2u32 << i) {
///                 dict_size = 2u32 << i;
///                 break;
///             }
///             if reduce_size <= (3u32 << i) {
///                 dict_size = 3u32 << i;
///                 break;
///             }
///         }
///     }
///     dict_size
/// }
/// ```
///
/// ## Buffer Restrictions
/// Each compressed LZMA hunk decompresses to a hunk-sized chunk.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
pub struct LzmaCodec {
    // The LZMA codec for CHD uses raw LZMA chunks without a stream header. The result
    // is that the chunks are encoded with the defaults used in LZMA 19.0.
    // These defaults are lc = 3, lp = 0, pb = 2.
    engine: LzmaDecoder,
}

impl CompressionCodec for LzmaCodec {}

/// MAME/libchdr uses an ancient LZMA 19.00.
///
/// To match the proper dictionary size, we copy the algorithm from
/// [`LzmaEnc::LzmaEncProps_Normalize`](https://github.com/rtissera/libchdr/blob/cdcb714235b9ff7d207b703260706a364282b063/deps/lzma-19.00/src/LzmaEnc.c#L52).
fn get_lzma_dict_size(level: u32, reduce_size: u32) -> u32 {
    let mut dict_size = if level <= 5 {
        1 << (level * 2 + 14)
    } else if level <= 7 {
        1 << 25
    } else {
        1 << 26
    };

    // this does the same thing as LzmaEnc.c when determining dict_size
    if dict_size > reduce_size {
        // might be worth converting this to a while loop for const
        // depends if we can const-propagate hunk_size.
        // todo: #[feature(const_inherent_unchecked_arith)
        for i in 11..=30 {
            if reduce_size <= (2u32 << i) {
                dict_size = 2u32 << i;
                break;
            }
            if reduce_size <= (3u32 << i) {
                dict_size = 3u32 << i;
                break;
            }
        }
    }

    dict_size
}

impl CompressionCodecType for LzmaCodec {
    fn codec_type(&self) -> CodecType
    where
        Self: Sized,
    {
        CodecType::LzmaV5
    }
}

#[cfg(not(feature = "fast_lzma"))]
impl CodecImplementation for LzmaCodec {
    fn new(hunk_size: u32) -> Result<Self> {
        Ok(LzmaCodec {
            engine: LzmaDecoder::new(
                LzmaParams::new(
                    LzmaProperties {
                        lc: 3,
                        lp: 0,
                        pb: 2,
                    },
                    get_lzma_dict_size(9, hunk_size),
                    None,
                ),
                None,
            )
            .map_err(|_| Error::DecompressionError)?,
        })
    }

    fn decompress(&mut self, input: &[u8], mut output: &mut [u8]) -> Result<DecompressResult> {
        let mut read = Cursor::new(input);
        let len = output.len();
        self.engine.reset(Some(Some(len as u64)));
        self.engine
            .decompress(&mut read, &mut output)
            .map_err(|_| Error::DecompressionError)?;
        Ok(DecompressResult::new(len, read.position() as usize))
    }
}

#[cfg(feature = "fast_lzma")]
impl CodecImplementation for LzmaCodec {
    fn new(hunk_size: u32) -> Result<Self> {
        let dict_size = get_lzma_dict_size(9, hunk_size);
        Ok(LzmaCodec {
            engine: LzmaDecoder::new_with_buffer(
                LzmaParams::new(
                    LzmaProperties {
                        lc: 3,
                        lp: 0,
                        pb: 2,
                    },
                    dict_size,
                    None,
                ),
                None,
                vec![0; dict_size as usize],
            )
            .map_err(|_| Error::CodecError)?,
        })
    }

    fn decompress(&mut self, input: &[u8], mut output: &mut [u8]) -> Result<DecompressResult> {
        use lzma_rs::decompress::raw::LzAccumBuffer;
        let mut read = Cursor::new(input);
        let len = output.len();
        self.engine.reset(Some(Some(len as u64)));
        self.engine
            .decompress_with_buffer::<LzAccumBuffer<_>, _, _>(&mut read, &mut output)
            .map_err(|_| Error::DecompressionError)?;
        Ok(DecompressResult::new(len, read.position() as usize))
    }
}

/// LZMA (lzma) compression codec.
///
/// Backed by [`lzma-sdk-rs`](https://docs.rs/lzma-sdk-rs), a bit-exact pure-Rust port of the
/// 7-zip LZMA SDK 23.01 encoder. Emits a raw LZMA stream (no header, no end marker) using the
/// CHD props (lc=3/lp=0/pb=2, dictionary reduced to the hunk size) — byte-identical to MAME's
/// `LzmaEnc_MemEncode`. `LzmaProps::chd_for_hunk` reproduces the same dictionary derivation as
/// [`get_lzma_dict_size`] above.
#[cfg(feature = "write")]
pub struct LzmaEncoder {
    props: lzma_sdk_rs::LzmaProps,
}

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for LzmaEncoder {
    fn new(hunk_size: u32) -> Result<Self> {
        Ok(LzmaEncoder {
            props: lzma_sdk_rs::LzmaProps::chd_for_hunk(hunk_size),
        })
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let out = lzma_sdk_rs::encode(input, &self.props);
        if out.len() > output.len() {
            return Err(Error::CompressionError);
        }
        output[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }
}

#[cfg(feature = "write")]
impl crate::compression::CompressionEncoder for LzmaEncoder {}

#[cfg(all(test, feature = "write"))]
mod tests {
    use super::*;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    /// Proves the cross-crate integration: lzma-sdk-rs's encoder output is decodable by
    /// chd-rs's existing LZMA decoder with the CHD dictionary params, and round-trips.
    #[test]
    fn lzma_encode_roundtrips_through_decoder() {
        let hunk: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

        let mut enc = LzmaEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(n < hunk.len(), "expected compression to shrink the hunk");

        let mut dec = LzmaCodec::new(4096).unwrap();
        let mut out = vec![0u8; 4096];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), 4096);
        assert_eq!(out, hunk);
    }
}
