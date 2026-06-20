# chd-rs encode integration spec

How the three bit-exact codec crates (`lzma-sdk-rs`, `libflac-rs`,
`libzstd-bitexact-rs`) plus in-tree `zlib`/`huff` plug into chd-rs's existing codec
layer to enable **writing** CHDs. This is the design that precedes implementation
(PARITY_PLAN M0–M2). It mirrors the decode architecture exactly — every decoder has an
inverse encoder constrained by the same `hunk_size` and property derivation.

Scope: the **codec encode layer** only. The writer core that drives these (V5 header +
compressed-map writer, SHA-1, the per-hunk codec-selection driver) is PARITY_PLAN M1 and is
summarized in §6 for the contract the codecs must satisfy.

## 1. Encode trait — mirror of `CodecImplementation`

Decode side (`compression/mod.rs:43`):
```rust
pub trait CodecImplementation {
    fn new(hunk_size: u32) -> Result<Self> where Self: Sized;
    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<DecompressResult>;
}
```

Add a parallel trait (gated behind a new `write` feature):
```rust
pub trait CodecEncode {
    fn new(hunk_size: u32) -> Result<Self> where Self: Sized;
    /// Compress `input` (exactly hunk_size bytes) into `output`. Returns the number of
    /// compressed bytes written. `output` is sized to hunk_size; a codec that cannot
    /// beat that (would expand) returns Err(CompressionError) so the driver falls back
    /// to a smaller codec or stores the hunk uncompressed. MUST be deterministic.
    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize>;
}
```

`CompressResult` is unnecessary — encoders consume the whole hunk and report one length.
Reuse the existing `Error::CompressionError` variant (already defined, currently unused).

**Struct layout:** add a parallel encoder struct per codec under `compression/` (e.g.
`ZlibEncoder`, `LzmaEncoder`, `HuffmanEncoder`, `RawFlacEncoder`, `CdEncoder<E, S>`), keeping
decode and encode types separate (the new crates' encoders are distinct objects from the
decoders chd-rs already uses). Mirror `CodecType::init` with:
```rust
impl CodecType {
    #[cfg(feature = "write")]
    pub fn init_encoder(&self, hunk_size: u32) -> Result<Box<dyn CodecEncode>> { /* match … */ }
}
```
and a `CodecsEncode` enum mirroring `Codecs::{Single, Four}` (`chdfile.rs:434`).

## 2. Per-codec mapping (decode internal → encode inverse)

| CHD codec | FourCC | Encode source | Notes |
| --- | --- | --- | --- |
| `none` | 0 | in-tree | `output.copy_from_slice(input)`; only "wins" when nothing else fits (driver uses NONE map type, not this codec, but keep symmetric) |
| `zlib` | `zlib` | `flate2` (in-tree) | raw deflate, **see §2.1** |
| `lzma` | `lzma` | **`lzma-sdk-rs`** | **see §2.2** |
| `zstd` | `zstd` | **`libzstd-bitexact-rs`** | `compress(input, 22)`; ⚠️ version drift (§2.3) |
| `huff` | `huff` | in-tree | port MAME static-Huffman encoder; **see §2.4** |
| `flac` | `flac` | **`libflac-rs`** | endian trial + `'L'/'B'`; **see §2.5** |
| `cdzl` | `cdzl` | `CdEncoder<Zlib, Zlib>` | **see §2.6** |
| `cdlz` | `cdlz` | `CdEncoder<Lzma, Zlib>` | sector=lzma, subcode=zlib |
| `cdzs` | `cdzs` | `CdEncoder<Zstd, Zstd>` | ⚠️ inherits zstd drift |
| `cdfl` | `cdfl` | `CdFlacEncoder` | sector=FLAC(BE), subcode=zlib; no header byte; **see §2.6** |
| `avhuff` | `avhu` | — | **out of scope** (AV deferred) |

### 2.1 `zlib` — flate2 raw deflate

Decode uses `flate2::Decompress::new(false)` (raw, no zlib header). Encode:
```rust
let mut c = flate2::Compress::new(flate2::Compression::best(), false); // level 9, raw
c.compress(input, output, flate2::FlushCompress::Finish)?;
let n = c.total_out() as usize;
```
MAME uses `deflateInit2(Z_BEST_COMPRESSION, Z_DEFLATED, -MAX_WBITS, /*memLevel=*/8, Z_DEFAULT_STRATEGY)`.
flate2 → `Compression::best()` = level 9, `zlib_header=false` = `-MAX_WBITS`, and zlib's default
memLevel is 8 (flate2 doesn't override it). **Bit-exact hinges on the `zlib-rs` backend
(already chd-rs's dep) producing byte-identical deflate to stock zlib 1.3.1.** This is the
zlib validation task from PARITY_PLAN — add a differential test (deflate a corpus, compare to
zlib 1.3.1). If `zlib-rs` diverges, pin/patch it; do not silently accept.

### 2.2 `lzma` — lzma-sdk-rs

Decode derives the dict size from hunk size (`get_lzma_dict_size(9, hunk_size)`, props
lc=3/lp=0/pb=2). The crate's `LzmaProps::chd_for_hunk(hunk_bytes)` reproduces exactly that.
Encode:
```rust
let props = lzma_sdk_rs::LzmaProps::chd_for_hunk(hunk_size);
let out = lzma_sdk_rs::encode(input, &props);   // raw stream, no header, no end marker
```
`encode` is byte-exact with `LzmaEnc_MemEncode(..., writeEndMark=0)` — the exact bytes the V5
map stores. Copy `out` into `output`, return its length (Err if `out.len() >= output.len()`).
The 5 decoder-props bytes are **not** stored (chd-rs reconstructs them from hunk size on
decode), so `decoder_props` is unused here.

### 2.3 `zstd` — libzstd-bitexact-rs (IMPLEMENTED, gated `write-zstd`)

`libzstd-bitexact-rs` **0.155** is byte-exact with zstd **1.5.5** (chdman's version). chdman's
per-hunk path is level 22 with an **unknown pledged size**, so:
```rust
let mut out = Vec::new();
libzstd_bitexact_rs::StreamEncoder::new(22)   // unknown pledged size => windowLog 27 + LDM
    .finish(input, &mut out)?;                // == ZSTD_compressStream2(.., ZSTD_e_end)
```
Do **not** use `with_pledged_src_size` — pledging the hunk size downsizes `windowLog` and
changes the output bytes. `compress(input, 22)` is byte-identical for a single `e_end`. For
`cdzs`, the same encoder applies to each sub-stream. Implemented as `ZstdEncoder` in
`compression/zstd.rs` behind the `write-zstd` feature (the zstd crate is large and `zstd`/`cdzs`
aren't default codecs). ⚠️ A round-trip test proves validity only; to guard byte-identity,
compare *compressed* bytes to a zstd-1.5.5 golden (decode is format-stable).

### 2.4 `huff` — in-tree static-Huffman encoder

Decode: `Huffman8BitDecoder::from_huffman_tree(reader)` then `decode_one` per byte
(`huff.rs`). No standalone crate — implement the **encoder** in-tree alongside the decoder,
porting MAME `huffman.cpp`'s `import_tree_rle`/`compute_tree_from_histogram`/encode path
(BSD-3). The `huff_write` feature scaffolding in `huffman.rs` (the `parent`/`count`/`histogram`
node fields) is the starting point. Output: serialize the tree (RLE bitstream) then Huffman-
code each byte. This same encoder is also needed by the **V5 compressed-map writer** (the map
uses a 16-symbol Huffman), so build it as a reusable `huffman` encode primitive.

### 2.5 `flac` (raw) — libflac-rs with endian trial

Decode: first byte `'L'`/`'B'` selects endianness, remainder is raw FLAC frames decoded to 2ch
i16 PCM. Encode wrapper (chd-rs owns the endian trial, per libflac-rs's design note):
```rust
let block_size = blocksize(hunk_size);           // chd-rs's existing helper: /4, halve while >2048
let enc = libflac_rs::Encoder::new(libflac_rs::EncoderConfig::chd(block_size));
// interpret the hunk as i16 samples, two ways:
let le_i32: Vec<i32> = samples_as_i32::<LittleEndian>(input);
let be_i32: Vec<i32> = samples_as_i32::<BigEndian>(input);
let le = enc.encode_frames(&le_i32);             // raw frames, no STREAMINFO
let be = enc.encode_frames(&be_i32);
// prepend marker, keep the smaller — exactly MAME's chd_flac_compressor
let (marker, body) = if le.len() <= be.len() { (b'L', le) } else { (b'B', be) };
```
`EncoderConfig::chd(block_size)` bakes in level 8 / 2ch / 16-bit. **`encode_frames` takes
`&[i32]` interleaved**, so convert the hunk's i16 PCM to i32. ⚠️ libflac-rs float parity is
validated vs **glibc** libm — confirm the reference chdman is a glibc build (FLAC encode
decisions are libm-dependent on both sides; PARITY_PLAN §11 secondary note).

### 2.6 CD wrappers — `CdEncoder<Engine, Sub>` and `CdFlacEncoder`

Each 2448-byte CD frame = **2048 sector data + 96 subcode** (`CD_MAX_SECTOR_DATA` +
`CD_MAX_SUBCODE_DATA`; `CD_FRAME_SIZE=2448`). Decode (cdrom.rs) splits compressed input as
`[ECC-flag bytes][sector complen 2|3 BE][sector stream][subcode stream]`, decompresses each
sub-stream as one blob across all frames, de-interleaves to `[sector|subcode]×frames`, and for
flagged frames writes the sync header + `generate_ecc()`. **Encode inverts this:**

1. For each frame, split `[2048 sector][96 subcode]`.
2. **ECC strip:** if a sector is a raw MODE1/MODE2 sector with a valid sync header and ECC
   (detect with the existing `ecc.rs`: sync == `CD_SYNC_HEADER` and `verify_ecc()` true), set
   its bit in the ECC-flag header and **omit** the ECC/sync from the data fed to the
   compressor (MAME stores the sector without redundant ECC, regenerating on decode). Use
   `clear_ecc()`/the known offsets to elide. Otherwise leave the sector intact, flag bit 0.
3. Concatenate all (possibly ECC-stripped) sector data → compress with `Engine`
   (`ZlibEncoder`/`LzmaEncoder`/`ZstdEncoder`); concatenate all subcode → compress with `Sub`.
4. Emit header: `ceil(frames/8)` ECC-flag bytes, then the sector stream's compressed length as
   **2 bytes** (hunk < 64 KiB) or **3 bytes** big-endian, then sector stream, then subcode
   stream. (`CdFlacEncoder`: no ECC-flag/complen header byte layout differs — sector→FLAC(BE)
   via §2.5 without the `'L'/'B'` trial since CD FLAC is always big-endian, subcode→zlib;
   mirror `CdFlacCodec::decompress` exactly.)

`ecc.rs`'s `ErrorCorrectedSector` trait already provides `generate_ecc`/`verify_ecc`/`clear_ecc`
— the forward path needed here exists; no new ECC math.

## 3. Dependency wiring

The three crates are sibling repos outside the chd-rs workspace. In `chd-rs/chd-rs/Cargo.toml`:
```toml
[features]
write = ["dep:lzma-sdk-rs", "dep:libflac-rs"]   # core write path (none/zlib/huff/lzma/flac)
write-zstd = ["write", "dep:libzstd-bitexact-rs"] # gated until the crate targets zstd 1.5.5

[dependencies]
lzma-sdk-rs       = { path = "../../lzma-sdk-rs", optional = true }
libflac-rs        = { path = "../../libflac-rs", optional = true }
libzstd-bitexact-rs = { path = "../../libzstd-bitexact-rs", optional = true }
```
Path deps for local dev; switch to crates.io version deps once published. Keep `write` off by
default so read-only consumers pull in nothing. All encode code is `#[cfg(feature = "write")]`.

## 4. Sizing & invariants

- `output` buffers handed to `compress` are sized to `hunk_size`; a codec that would meet or
  exceed that returns `Err(CompressionError)` (the driver then tries the next slot / NONE).
- FLAC requires `hunk_size % (channels * 2) == 0`; CD codecs require `hunk_size % 2448 == 0`
  (already validated on the decode `new`; reuse the same checks).
- lzma/flac encoders may allocate a `Vec` and copy into `output`; that's fine (matches the
  decode side's buffering). Keep a reusable scratch `Vec` per encoder to avoid per-hunk allocs.

## 5. Verification (per PARITY_PLAN §7)

1. **Round-trip:** `encode` → existing chd-rs `decompress` → assert equals the original hunk.
   Already have the decoders; this is the first gate for every codec.
2. **Codec byte-exactness:** the three crates are self-verified vs their C oracle, so the
   remaining risk is chd-rs's *wrappers* (endian trial, CD split/ECC, props derivation). Add a
   thin differential: for a corpus of hunks, compare each chd-rs encoder's output to the
   corresponding MAME codec output (via libchdman-rs / chdman), byte-for-byte. zlib is the
   one in-tree codec whose bytes depend on `zlib-rs` — test it explicitly vs zlib 1.3.1.
3. **End-to-end (M1+):** a full CHD written by chd-rs must be byte-identical to chdman's for
   the same input + codec set, and pass `chdman verify`. That exercises codec selection order,
   map encoding, and SHA-1 together (gated `chdman_compat_tests`, dev-local).

## 6. Contract for the codec-selection driver (M1, summary)

The writer core compresses each hunk by trying the configured codec slots and picking the
output the same way MAME does — **smallest wins; tie broken by lowest slot index** — else
self/parent-ref, else NONE. For bit-for-bit parity the driver must reproduce MAME's
`chd.cpp` per-hunk decision order exactly (which codecs are attempted, in what order, and the
tie rule), because the winning codec index is written into the V5 map. The codec layer above
just needs to be **deterministic** and to **reject (Err) rather than expand**, so the driver
can compare lengths. Map encoding, header, and SHA-1 ordering are specified in PARITY_PLAN M1.
