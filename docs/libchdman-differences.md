# Porting from libchdman-rs to chd-rs

chd-rs offers the same CHD **create/extract** functionality as
[`libchdman-rs`](../../libchdman-rs) (a C++/MAME wrapper), but keeps chd-rs's own idioms on the
read side. This doc is the authoritative "how do I translate my libchdman-rs code" guide: every
intentional divergence, and the mechanical substitution for it. The full API map (which item maps
to which) is in [libchdman-parity.md](libchdman-parity.md); this doc is the *why it differs / what
to write instead*.

The headline decision (locked): **keep chd-rs's generic, borrowed reader; add the create/extract
surface as free functions; do not graft libchdman-rs's owned, mutable `Chd` handle.**

---

## 1. The `Chd` handle: borrowed reader vs owned read/write handle

| | libchdman-rs | chd-rs |
| --- | --- | --- |
| Type | `Chd` — an **owned** handle over a path/`ChdIo`, with a `writeable` flag | `Chd<F: Read + Seek>` — a **generic, borrowed** reader |
| Mutability | one handle **reads *and* writes** at runtime | **reads only**; creation is via free functions; runtime block writes via a future `HdImage` |
| Custom I/O | the `ChdIo: Read + Write + Seek` trait | **any `Read + Seek`** — no trait needed |
| Compression | async `ChdCompressor` + `ChdDataHandler` pull model | **synchronous** create functions with `progress`/`cancel` callbacks |

### Opening

```rust
// libchdman-rs
let chd = Chd::open(path, /*writeable=*/false, /*parent=*/None)?;

// chd-rs — takes a Read+Seek, no `writeable` (reads only). Path helper:
use std::io::BufReader; use std::fs::File;
let chd = chd::Chd::open(BufReader::new(File::open(path)?), None)?;
```

There is **no `writeable` open**. Reading and creating are different operations on different types.
`Chd::open_custom(io, ..)` collapses to plain `Chd::open(io, ..)` — any `Read + Seek` *is* "custom
I/O", so `ChdIo` is unnecessary.

### Header / hunk / metadata accessors (the 🟡 rows)

These exist but live on sub-objects, not as methods on `Chd`:

| libchdman-rs | chd-rs |
| --- | --- |
| `chd.version()/hunk_bytes()/hunk_count()/unit_bytes()/logical_bytes()` | `chd.header().{version,hunk_size,hunk_count,unit_bytes,logical_bytes}()` |
| `chd.sha1()/raw_sha1()/parent_sha1()` | `chd.header().{sha1,raw_sha1,parent_sha1}()` |
| `chd.hunk_info(n)` | `chd.map().get_entry(n)` → `MapEntry` (`hunk_type()` + `block_size()`) |
| `chd.read_hunk(n, buf)` | `chd.hunk(n)?.read_hunk_in(&mut scratch, buf)` (needs a scratch buffer) |
| `chd.read_bytes(off, buf)` | wrap in `read::ChdReader` (`Read + Seek`) and `seek` + `read_exact` |
| `chd.read_metadata(tag, index)` | `chd.metadata_refs()` / `chd.metadata()` then filter by `metatag` (see `hd::read_geometry`) |

---

## 2. Creating CHDs

libchdman-rs creates via `Chd::create(..)` / the format modules' `create_from_*` (which internally
drive `ChdCompressor`). chd-rs has **free functions in the format modules** instead.

### Hard disk — raw (`createraw`)

```rust
// chd-rs: byte-identical to `chdman createraw`
use chd::hd::{self, HdCreateOptions};
use chd::{CompressionProgress, CHD_CODEC_LZMA, CHD_CODEC_ZLIB};

let opts = HdCreateOptions {
    hunk_size: 4096,
    unit_size: 512,
    codecs: [CHD_CODEC_LZMA, CHD_CODEC_ZLIB, 0, 0], // per-hunk best-of, like chdman -c lzma,zlib
    ..Default::default()
};
hd::create_raw_from_path(
    in_path, out_path, opts,
    &mut |p: CompressionProgress| eprintln!("{}/{} ({:.1}%)", p.bytes_done, p.bytes_total, p.ratio * 100.0),
    &|| false, // cancel
)?;
```

- **`codecs` is a `[u32; 4]` of `CHD_CODEC_*` FourCCs** (use `chd::parse_codec_spec("lzma,zlib")`
  for chdman's `-c` syntax). The list must be contiguous from slot 0 (a `0` ends it); all-zero
  means uncompressed. Per hunk, chd-rs reproduces MAME's `find_best_compressor` exactly.
- The whole input is read into memory (a streaming writer is future work).
- `progress` / `cancel` replace libchdman-rs's async pull model — see §3.

### Hard disk — full `createhd` (with `GDDD`/`IDNT` geometry metadata)

```rust
use chd::hd::{self, HdCreateOptions, HdGeometry};
let opts = HdCreateOptions {
    codecs: [chd::CHD_CODEC_ZLIB, 0, 0, 0],
    geometry: None,            // None → derived via compute_chs (like chdman)
    ident: None,               // Some(blob) → also writes an IDNT record
    ..Default::default()
};
hd::create_from_path(in_path, out_path, opts, &mut |_p| {}, &|| false)?;
```

`create_from_*` is byte-identical to `chdman createhd`: it writes the `GDDD` geometry record (and,
if `opts.ident` is set, an `IDNT` record), and computes the metadata-inclusive overall SHA-1 for
compressed CHDs (uncompressed CHDs leave the SHA-1 fields zero, as chdman does). Use
`create_raw_*` (which writes **no** metadata) for the `createraw` equivalent — it rejects
`geometry`/`ident` with `Error::UnsupportedFormat` so you can't accidentally lose them.

### Extract (`extractraw` / `extracthd`)

```rust
hd::extract_to_path(chd_path, out_path, &mut |bytes_done| { /* progress */ })?;
```

Works for any CHD chd-rs can decode (any codec/version); the output is the exact logical image
(the last hunk's zero-padding is truncated).

### Copy / re-compress (`copy`)

```rust
use chd::copy::{self, CopyOptions};
let opts = CopyOptions {
    hunk_size: None,                       // None → keep the source's hunk size
    codecs: [chd::CHD_CODEC_LZMA, 0, 0, 0],// [0;4] → uncompressed
};
copy::copy(src_path, dst_path, opts, &mut |_p| {}, &|| false)?;
```

Byte-identical to `chdman copy`: re-compresses the source's logical bytes into the new codec
list/hunk size and clones every metadata record verbatim, preserving the unit size and `raw_sha1`.
(Legacy CD/GD metadata re-do is not yet implemented — that lands with the `cd` module.)

---

## 3. Progress & cancellation (replaces `ChdCompressor`/`ChdDataHandler`/`CompressStep`)

chd-rs's create functions are **synchronous** and take two callbacks:

- `progress: &mut dyn FnMut(CompressionProgress)` — invoked per hunk. `CompressionProgress` matches
  libchdman-rs field-for-field: `{ bytes_done: u64, bytes_total: u64, ratio: f64 }`.
- `cancel: &dyn Fn() -> bool` — polled before each hunk. Returning `true` aborts with
  `Error::Cancelled`. Because the output is assembled in memory and flushed only on success,
  **a cancelled write leaves the output untouched** (and `create_raw_from_path` removes the partial
  file).

There is no `ChdCompressor`, `ChdDataHandler`, `ChdCompressor::compress_continue`, or `CompressStep`
loop to drive — just pass the callbacks.

```rust
// libchdman-rs: drive the pull loop yourself
// loop { match compressor.compress_continue()? { CompressStep::Continue(p) => .., Done(p) => break } }

// chd-rs: hand the callbacks to the create function; it runs the loop internally.
```

---

## 4. Errors

chd-rs uses its own `chd::Error` (`Result<T> = Result<T, chd::Error>`), the same enum the read API
returns. The create surface adds one variant, **`Error::Cancelled`** (no libchdr equivalent;
appended after `Error::Unknown` so the existing libchdr-ABI discriminants are unchanged).

---

## 5. Not ported (and the chd-rs equivalent)

| libchdman-rs | chd-rs equivalent |
| --- | --- |
| `ChdIo` trait | any `Read + Seek` (generic) |
| `ChdCompressor` / `ChdDataHandler` / `CompressStep` | synchronous create fns + `progress`/`cancel` |
| `Chd::create` / `Chd::create_with_parent` | `hd::create_*` / `hd::create_raw_*` (parent/diff in a later phase) |
| `copy::copy` / `CopyOptions` | `copy::copy` / `copy::CopyOptions` (same shape) |
| `Chd::write_hunk` / `write_bytes` / `write_metadata` | runtime writes via a future `HdImage`; metadata writer in a later phase |
| `Chd::info()` / `verify()` / `ChdInfo` | Phase G |
| `make_tag(a,b,c,d)` (4-arg) | crate-internal `make_tag(&[u8;4])` |

`HunkIter`/`MetadataIter`/`ChdReader`/`HunkReader` and the metadata tag constants exist under
different names — see [libchdman-parity.md](libchdman-parity.md) §3.7.

---

## 6. FLAC byte-identity caveat

The raw `flac` codec (and, later, `cdfl` / the DVD default) is backed by
[`libflac-rs`](../../libflac-rs), whose float parity is validated against **glibc** libm. Output is
byte-identical to a **glibc-built** chdman, but may differ by a few bytes from an **MSVC/Windows**
chdman. It is always **round-trip-correct** (decodes back to the original PCM, and chdman extracts
a chd-rs-written flac CHD losslessly). Treat flac as round-trip-verified except against a confirmed
glibc chdman. All other codecs (`none`/`zlib`/`huff`/`lzma`/`zstd`) are byte-identical to chdman
0.288 unconditionally.
