use crate::error::Result;
use crate::header::CodecType;
use std::ops::{Add, AddAssign};

mod avhuff;
mod cdrom;
mod ecc;
mod flac;
mod huff;
mod lzma;
mod none;
mod zlib;
mod zstd;

pub mod codecs {
    pub use crate::compression::avhuff::AVHuffCodec;
    pub use crate::compression::cdrom::CdLzmaCodec;
    pub use crate::compression::cdrom::CdZlibCodec;
    pub use crate::compression::cdrom::CdZstdCodec;
    pub use crate::compression::flac::CdFlacCodec;
    pub use crate::compression::flac::RawFlacCodec;
    pub use crate::compression::huff::HuffmanCodec;
    pub use crate::compression::lzma::LzmaCodec;
    pub use crate::compression::none::NoneCodec;
    pub use crate::compression::zlib::ZlibCodec;
    pub use crate::compression::zstd::ZstdCodec;

    // Encoders (write feature). Added per-codec as milestones land.
    #[cfg(feature = "write")]
    pub use crate::compression::flac::RawFlacEncoder;
    #[cfg(feature = "write")]
    pub use crate::compression::huff::HuffmanEncoder;
    #[cfg(feature = "write")]
    pub use crate::compression::lzma::LzmaEncoder;
    #[cfg(feature = "write")]
    pub use crate::compression::none::NoneEncoder;
    #[cfg(feature = "write")]
    pub use crate::compression::zlib::ZlibEncoder;
    #[cfg(feature = "write-zstd")]
    pub use crate::compression::zstd::ZstdEncoder;
}

// unstable(trait_alias)
/// Marker trait for a codec that can be used to decompress a compressed hunk.
pub trait CompressionCodec: CodecImplementation + CompressionCodecType + Send + Sync {}

/// Trait for a codec that implements a known CHD codec type.
pub trait CompressionCodecType {
    /// Returns the known [`CodecType`](crate::header::CodecType) that this
    /// codec implements.
    fn codec_type(&self) -> CodecType
    where
        Self: Sized;
}

/// Trait for a CHD decompression codec implementation.
pub trait CodecImplementation {
    /// Creates a new instance of this codec for the provided hunk size.
    fn new(hunk_size: u32) -> Result<Self>
    where
        Self: Sized;

    /// Decompress compressed bytes from the input buffer into the
    /// output buffer.
    ///
    /// Usually the output buffer must have the exact
    /// length as `hunk_size`, but this may be dependent on the codec
    /// implementation.
    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult>;
}

/// Marker trait for a codec that can be used to compress (encode) a hunk.
///
/// The encode mirror of [`CompressionCodec`]. Available with the `write` feature.
#[cfg(feature = "write")]
pub trait CompressionEncoder: CodecEncodeImplementation + Send + Sync {}

/// Trait for a CHD compression (encode) codec implementation.
///
/// The encode mirror of [`CodecImplementation`]. Available with the `write` feature.
#[cfg(feature = "write")]
pub trait CodecEncodeImplementation {
    /// Creates a new encoder for the provided hunk size.
    fn new(hunk_size: u32) -> Result<Self>
    where
        Self: Sized;

    /// Compress `input` (one hunk) into `output`, returning the number of compressed
    /// bytes written.
    ///
    /// `output` is sized to at least `hunk_size`. A codec that cannot produce output
    /// that fits within `output` (i.e. it would expand the hunk) returns
    /// [`Error::CompressionError`](crate::error::Error::CompressionError) so the
    /// writer can fall back to a smaller codec or store the hunk uncompressed.
    ///
    /// Implementations must be **deterministic**: identical input must yield identical
    /// output (the bit-for-bit parity goal depends on it).
    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize>;
}

/// The result of a chunk decompression operation.
#[derive(Copy, Clone, Default)]
pub struct DecompressResult {
    bytes_out: usize,
    bytes_read: usize,
}

impl Add for DecompressResult {
    type Output = DecompressResult;

    fn add(self, rhs: Self) -> Self::Output {
        DecompressResult {
            bytes_out: self.total_out() + rhs.total_out(),
            bytes_read: self.total_in() + rhs.total_in(),
        }
    }
}

impl AddAssign for DecompressResult {
    fn add_assign(&mut self, rhs: Self) {
        self.bytes_read += rhs.bytes_read;
        self.bytes_out += rhs.bytes_out;
    }
}

impl DecompressResult {
    pub(crate) fn new(out: usize, read: usize) -> Self {
        DecompressResult {
            bytes_out: out,
            bytes_read: read,
        }
    }

    /// Returns the total number of decompressed bytes written to the output buffer.
    pub fn total_out(&self) -> usize {
        self.bytes_out
    }

    /// Returns the total number of bytes read from the compressed input buffer.
    pub fn total_in(&self) -> usize {
        self.bytes_read
    }
}
