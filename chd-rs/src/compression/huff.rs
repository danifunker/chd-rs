use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::error::Result;
use crate::header::CodecType;
use crate::huffman::Huffman8BitDecoder;
use bitreader::BitReader;

/// MAME 8-bit Huffman (huff) decompression codec.
///
/// ## Format Details
/// The Huffman codec uses a Huffman-encoded Huffman tree with the
/// the default Huffman settings of
/// * `NUM_CODES`: 256
/// * `MAX_BITS`: 16
///
/// The last decoded code from the input buffer may not contain enough bits for a full
/// byte is reconstructed by shifting zero-bits in from the right. See the source code for
/// [huffman.rs](https://github.com/SnowflakePowered/chd-rs/blob/575cc2330b0c6eb444e8773068295510147ffa6b/chd-rs/src/huffman.rs#L242)
/// for more details.
/// ## Buffer Restrictions
/// Each compressed Huffman hunk decompresses to a hunk-sized chunk.
/// The input buffer must contain exactly enough data to fill the output buffer
/// when decompressed.
pub struct HuffmanCodec;
impl CodecImplementation for HuffmanCodec {
    fn new(_: u32) -> Result<Self> {
        Ok(HuffmanCodec)
    }

    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult> {
        let mut bit_reader = BitReader::new(input);
        let decoder = Huffman8BitDecoder::from_huffman_tree(&mut bit_reader)?;

        for i in output.iter_mut() {
            *i = decoder.decode_one(&mut bit_reader)? as u8;
        }

        Ok(DecompressResult::new(
            output.len(),
            ((input.len() * 8) - bit_reader.remaining() as usize) / 8,
        ))
    }
}

impl CompressionCodecType for HuffmanCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::HuffV5
    }
}

impl CompressionCodec for HuffmanCodec {}

/// MAME 8-bit Huffman (huff) compression codec.
///
/// Encodes via [`crate::huffman_encode::encode_8bit`], a faithful port of MAME's
/// `huffman_8bit_encoder` (256 codes, 16-bit max) — byte-identical to chdman for a given hunk.
#[cfg(feature = "write")]
pub struct HuffmanEncoder;

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for HuffmanEncoder {
    fn new(_: u32) -> Result<Self> {
        Ok(HuffmanEncoder)
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let out = crate::huffman_encode::encode_8bit(input)
            .map_err(|_| crate::error::Error::CompressionError)?;
        if out.len() > output.len() {
            return Err(crate::error::Error::CompressionError);
        }
        output[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }
}

#[cfg(feature = "write")]
impl crate::compression::CompressionEncoder for HuffmanEncoder {}

#[cfg(all(test, feature = "write"))]
mod tests {
    use super::*;
    use crate::compression::{CodecEncodeImplementation, CodecImplementation};

    #[test]
    fn huff_encode_roundtrips_through_decoder() {
        // Skewed distribution so Huffman shrinks it (result fits a hunk-sized buffer) and the
        // tree is non-trivial.
        let hunk: Vec<u8> = (0..4096u32)
            .map(|i| if i % 3 == 0 { (i % 11) as u8 } else { 0 })
            .collect();

        let mut enc = HuffmanEncoder::new(4096).unwrap();
        let mut comp = vec![0u8; 4096];
        let n = enc.compress(&hunk, &mut comp).unwrap();
        assert!(n < hunk.len(), "expected Huffman to shrink the skewed hunk");

        let mut dec = HuffmanCodec::new(4096).unwrap();
        let mut out = vec![0u8; 4096];
        let res = dec.decompress(&comp[..n], &mut out).unwrap();
        assert_eq!(res.total_out(), 4096);
        assert_eq!(out, hunk);
    }
}
