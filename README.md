# chd-rs
[![Latest Version](https://img.shields.io/crates/v/chd.svg)](https://crates.io/crates/chd) [![Docs](https://docs.rs/chd/badge.svg)](https://docs.rs/chd) ![License](https://img.shields.io/crates/l/chd)
[![Minimum Supported Rust Version 1.59](https://img.shields.io/badge/rust-1.59%2B-orange.svg)](https://github.com/rust-lang/rust/blob/master/RELEASES.md#version-1590-2022-02-24)


Reimplementation of the CHD file format in pure Safe Rust, drop-in compatible with libchdr.

chd-rs aims to be a memory-safe, well documented, and clean from-scratch implementation of CHD, verifiable against 
[chd.cpp](https://github.com/mamedev/mame/blob/master/src/lib/util/chd.cpp) while being easier to read and use as
documentation to implement the format natively in other languages. It is standalone and can be built with just
a Rust compiler, without the need for a full C/C++ toolchain. 

Performance is competitive but a little slower than libchdr in benchmarks from using more immature (but fully correct) 
pure Rust implementations of compression codecs. Deflate (zlib) compression is backed by [flate2](https://crates.io/crates/flate2), 
LZMA is backed by [lzma-rs](https://crates.io/crates/lzma-rs), and FLAC decompression is backed by
[claxon](https://crates.io/crates/claxon). While performance is not ignored, the focus
is on readability and correctness.

## Usage
Open a `Chd` with `Chd::open`, then iterate hunks from 0 to `chd.header().hunk_count()` to
read hunks.

The size of the destination buffer must be exactly `chd.header().hunk_size()` to decompress with
`hunk.read_hunk_in`, which takes the output slice and a buffer to hold compressed data.

```rust
fn main() -> Result<()> {
    let mut f = BufReader::new(File::open("image.chd")?;
    let mut chd = Chd::open(&mut f, None)?;
    let hunk_count = chd.header().hunk_count();
    let hunk_size = chd.header().hunk_size();
    
    // buffer to store decompressed hunks
    let mut out_buf = chd.get_hunksized_buffer();
    
    // buffer for temporary compressed
    let mut temp_buf = Vec::new();
    for hunk_num in 0..hunk_count {
        let mut hunk = chd.hunk(hunk_num)?;
        hunk.read_hunk_in(&mut temp_buf, &mut out_buf)?;
    }
}
```
For more ergonomic but slower usage, [`chd::read`](https://github.com/SnowflakePowered/chd-rs/blob/master/chd-rs/src/read.rs) provides buffered adapters that implement `Read` and `Seek` at the
hunk level. A buffered adapter at the file level is also available.

### Lending Iterators
With `unstable_lending_iterators`, hunks and metadata can be slightly more ergonomically iterated over
albeit with a `while let` loop. This API is unstable until [Generic Associated Types](https://github.com/rust-lang/rust/pull/96709)
and the `LendingIterator` trait is stabilized.


```toml
[dependencies]
chd = { version = "0.2", features = ["unstable_lending_iterators"] }
```

Then hunks can be iterated like so.

```rust
fn main() -> Result<()> {
    let mut f = BufReader::new(File::open("image.chd")?);
    let mut chd = Chd::open(&mut f, None)?;
    
    // buffer to store decompressed hunks
    let mut out_buf = chd.get_hunksized_buffer();
    
    // buffer for temporary compressed
    let mut temp_buf = Vec::new();
    let mut hunk_iter = chd.hunks();
    while let Some(mut hunk) = hunk_iter.next() {
        hunk.read_hunk_in(&mut temp_buf, &mut out_buf)?;
    }
}
```

A similar API exists for metadata in `Chd::metadata`.


### Verifying Hunk Checksums
By default, chd-rs does not verify the checksums of decompressed hunks for performance. The feature `verify_block_crc` should be enabled 
to verify hunk checksums.

```toml
[dependencies]
chd = { version = "0.2", features = ["verify_block_crc"] }
```

## Writing CHDs
With the `write` feature, chd-rs can **create** CHDs whose output is **byte-for-byte identical to
chdman 0.288**, using pure-Rust bit-exact codec implementations (the encoders live in standalone
sibling crates). FLAC output is byte-identical against a glibc-built chdman and round-trip-correct
otherwise.

```toml
[dependencies]
chd = { version = "0.3", features = ["write"] }   # add "write-zstd" for zstd/cdzs
```

The create/extract surface mirrors chdman, as free functions in per-format modules:

| chdman | chd-rs |
| --- | --- |
| `createraw` / `extractraw` | `hd::create_raw_from_path` / `hd::extract_to_path` |
| `createhd` | `hd::create_from_path` (GDDD geometry + optional IDNT) |
| `createcd` / `extractcd` | `cd::create_from_cue`/`create_from_gdi`/`create_from_iso`; `cd::extract_to_cue`/`extract_to_gdi`/`extract_to_iso` |
| `createdvd` / `extractdvd` | `dvd::create_from_iso` / `dvd::extract_to_iso` |
| `copy` | `copy::copy` |
| `addmeta` / `delmeta` | `metadata::write_metadata` / `metadata::delete_metadata` |
| `createraw -op` (compressed child) | `hd::create_raw_from_path_with_parent` |

A runtime block device, `hd::HdImage`, supports per-sector `read_sector`/`write_sector` against an
uncompressed CHD — in place, or as an uncompressed **diff** over a compressed parent
(`open_with_diff`), exactly as MAME writes to a compressed image at runtime.

`Chd::verify()` (the `verify` feature, also enabled by `write`) recomputes the raw + overall
(metadata-inclusive) SHA-1 and checks them against the header.

See [docs/chdman-mapping.md](docs/chdman-mapping.md) for the full command mapping.

### Supported Codecs
chd-rs supports the following compression codecs, with wider coverage than libchdr. For implementation details,
see the [`chd::compression`](https://github.com/SnowflakePowered/chd-rs/tree/master/chd-rs/src/compression) module.

#### V1-4 Codecs
⚠️*V1-4 support has not been as rigorously tested as V5 support.* ⚠️
* None (`CHDCOMPRESSION_NONE`)
* Zlib (`CHDCOMPRESSION_ZLIB`)
* Zlib+ (`CHDCOMPRESSION_ZLIB`)

#### V5 Codecs
* None (`CHD_CODEC_NONE`)
* LZMA (`CHD_CODEC_LZMA`)
* Deflate (`CHD_CODEC_ZLIB`)
* FLAC (`CHD_CODEC_FLAC`)
* Huffman (`CHD_CODEC_HUFF`)
* Zstandard (`CHD_CODEC_ZSTD`)
* CD LZMA (`CHD_CODEC_CD_LZMA`)
* CD Deflate (`CHD_CODEC_CD_ZLIB`)
* CD FLAC (`CHD_CODEC_CD_FLAC`)
* CD Zstandard (`CHD_CODEC_CD_ZSTD`)
* AV Huffman (`CHD_CODEC_AVHUFF`)

#### Codecs and Huffman API 
By default, the codecs and static Huffman implementations are not exposed as part of the public API, 
but can be enabled with the `codec_api` and `huffman_api` features respectively. These APIs are subject
to change but should be considered mostly stable. 

In particular the type signature for [`HuffmanDecoder`](https://github.com/SnowflakePowered/chd-rs/blob/e03e093021f1705d46fe6aaa8b32593489e55467/chd-rs/src/huffman.rs#L110)
is subject to change once [`generic_const_exprs`](https://github.com/rust-lang/rust/issues/76560) is stabilized.

## Migrating

### From a previous chd-rs version (read-only 0.2 / 0.3)

**The read API is unchanged.** `Chd::open`, `chd.header()`, `chd.hunk(n)?.read_hunk_in(..)`,
`chd.metadata_refs()`, and the `read::ChdReader` / `read::HunkBufReader` adapters all behave exactly
as before — existing read code needs **no changes**.

Everything new is **additive and feature-gated**, so a default build still pulls in nothing extra:

| Want | Enable | Get |
| --- | --- | --- |
| Create / extract / copy / edit metadata | `write` | the `hd` / `cd` / `dvd` / `copy` modules, `metadata::write_metadata`/`delete_metadata`, `hd::HdImage`, `CompressionProgress` — all byte-identical to chdman 0.288 |
| zstd / cdzs encoding | `write-zstd` | the above + the Zstandard encoders |
| Integrity check | `verify` (also pulled in by `write`) | `Chd::verify() -> VerifyResult` |

Available with **no feature** (read builds included): the `codec` module + the crate-root
`CHD_CODEC_*` constants and `parse_codec_spec`/`codec_name`/`codec_exists`; `Chd::info() -> ChdInfo`;
and `Header::compression() -> [u32; 4]`.

**One source-breaking change:** `chd::Error` gained a `Cancelled` variant (used by the create API).
It is appended **last**, so the `#[repr(C)]` discriminants of the existing variants — and the libchdr
C ABI — are unchanged. Only an *exhaustive* `match` on `chd::Error` (no `_` arm) needs a new arm;
code using `?` / `Result` is unaffected.

The bundled `rchdman` CLI also gained `createraw`/`createhd`/`createcd`/`createdvd`/`copy`/`addmeta`/
`delmeta`/`extractcd`/`extractdvd`, and its `verify` now checks the full raw + metadata SHA-1.

### From libchdman-rs

chd-rs offers the same create/extract/copy/verify functionality as `libchdman-rs` (a MAME C++
wrapper) but keeps its own read idioms. The headline differences:

| libchdman-rs | chd-rs |
| --- | --- |
| an owned, **writeable** `Chd` handle (`Chd::open(path, writeable, parent)`) | a generic, **borrowed read-only** `Chd<F: Read + Seek>`; create/extract are free functions |
| `Chd::create*` / `Chd::write_metadata` / `Chd::write_bytes` (methods) | `hd::create_*` / `metadata::write_metadata` / `hd::HdImage::write_sector` (free fns / a dedicated type) |
| `ChdIo: Read + Write + Seek` trait | any `Read + Seek` — no trait needed |
| async `ChdCompressor` + `CompressStep` pull loop | synchronous create fns taking `progress` / `cancel` callbacks |

```rust
// libchdman-rs:  let chd = Chd::open(path, /*writeable=*/ false, /*parent=*/ None)?;
// chd-rs — a Read+Seek, no `writeable`:
let chd = chd::Chd::open(std::io::BufReader::new(std::fs::File::open(path)?), None)?;

// libchdman-rs:  chd.create(..) driving ChdCompressor
// chd-rs — a free function with progress/cancel callbacks:
use chd::hd::{self, HdCreateOptions};
hd::create_from_path(in_path, out_path, HdCreateOptions::default(), &mut |_p| {}, &|| false)?;
```

The owned-handle runtime read/write surface maps to `hd::HdImage` (`open` / `open_with_diff` /
`read_sector` / `write_sector`); `CompressionProgress` matches libchdman-rs field-for-field.

For the exhaustive "translate my libchdman-rs code" guide (every accessor and diverging item) see
[docs/libchdman-differences.md](docs/libchdman-differences.md), and the chdman-command mapping in
[docs/chdman-mapping.md](docs/chdman-mapping.md).

## `rchdman` command line tool
chd-rs ships `rchdman`, a chdman-style CLI covering read **and** create operations:

* `info`, `verify` (full raw **and** metadata-inclusive SHA-1), `benchmark`, `dumpmeta`
* `extractraw`, `extractcd`, `extractdvd`
* `createraw` (with `--outputparent` for a compressed child), `createhd`, `createcd`, `createdvd`, `copy`
* `addmeta`, `delmeta`

Created CHDs are byte-identical to chdman. rchdman is single-threaded, so it is generally slower than
chdman, but the output is the same.

## Performance
By default, chd-rs uses pure Rust codecs but if maximum performance is needed, `max_perf` can be enabled. This enables the zlib-ng backend of [flate2](https://crates.io/crates/flate2)
as well as using experimental APIs in a custom [lzma-rs fork](https://github.com/SnowflakePowered/lzma-rs/tree/feature-perf-experiments) for some improvements in
performance at the expense of higher memory usage. Combined with `codegen-units=1` and [Profile Guided Optimization](https://github.com/vadimcn/cargo-pgo), chd-rs is within 1% of libchdr performance.

Without `max_perf`, chd-rs is already within 15% of libchdr without needing to link with C libraries like zlib-ng.

## `libchdr` API
⚠️*The C API has not been heavily tested. Use at your own risk.* ⚠️

chd-rs provides a C API compatible with [chd.h](https://github.com/rtissera/libchdr/blob/6eeb6abc4adc094d489c8ba8cafdcff9ff61251b/include/libchdr/chd.h). 
ABI compatibility is detailed below but is untested when compiling as a dynamic library. See [/chd-rs-capi](https://github.com/SnowflakePowered/chd-rs/tree/master/chd-rs-capi) for more details.
