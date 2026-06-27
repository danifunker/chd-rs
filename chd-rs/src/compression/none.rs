// None (copy) codec
use crate::compression::{
    CodecImplementation, CompressionCodec, CompressionCodecType, DecompressResult,
};
use crate::error::Result;
use crate::header::CodecType;
use std::io::Write;

/// None/copy codec that does a byte-for-byte copy of the input buffer.
///
/// ## Buffer Restrictions
/// The input buffer must be exactly the same length as the output buffer.
pub struct NoneCodec;
impl CodecImplementation for NoneCodec {
    fn new(_: u32) -> Result<Self> {
        Ok(NoneCodec)
    }

    fn decompress(&mut self, input: &[u8], mut output: &mut [u8]) -> Result<DecompressResult> {
        Ok(DecompressResult::new(output.write(input)?, input.len()))
    }
}

impl CompressionCodecType for NoneCodec {
    fn codec_type(&self) -> CodecType {
        CodecType::None
    }
}

impl CompressionCodec for NoneCodec {}

/// None/copy encoder: a byte-for-byte copy of the input.
///
/// "None" never shrinks a hunk, so the writer normally records uncompressed hunks at the
/// map level rather than through this encoder; it exists for symmetry with [`NoneCodec`].
#[cfg(feature = "write")]
pub struct NoneEncoder;

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for NoneEncoder {
    fn new(_: u32) -> Result<Self> {
        Ok(NoneEncoder)
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        if input.len() > output.len() {
            return Err(crate::error::Error::CompressionError);
        }
        output[..input.len()].copy_from_slice(input);
        Ok(input.len())
    }
}

#[cfg(feature = "write")]
impl crate::compression::CompressionEncoder for NoneEncoder {}
