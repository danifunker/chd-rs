# chd-rs → libchdman-rs Feature Parity Plan

Status: **DRAFT — one decision remaining: the bit-for-bit codec path (see [§11](#11-bit-for-bit-feasibility))**
Last updated: 2026-06-09

**Decisions locked 2026-06-09:** D1 = **bit-for-bit identical to chdman**; D3 = module/function-level
parity (keep generic `Chd<F>`); D4 = full `hd`+`cd`+`dvd`+`copy` parity in one push. D2 (encoder
strategy) is reframed by the bit-for-bit choice — see [§11](#11-bit-for-bit-feasibility).

## 1. Goal

Bring this crate (`chd-rs`, a **pure-Rust, read-only** CHD implementation) up to
feature parity with `../libchdman-rs` (a wrapper around MAME's C++ `chd.cpp` with
full `chdman` create/extract parity), **without copying GPL code** and keeping the
result **BSD-3-Clause**.

Where practical, mirror libchdman-rs's **Rust names** (modules, functions, option
structs) so downstream code can move between the two crates with minimal churn.

### What "parity" means here

| Capability | chd-rs today | libchdman-rs | Target |
| --- | --- | --- | --- |
| Read / decompress (V1–V5) | ✅ | ✅ | keep |
| Create HD (`createhd`/`createraw`) | ❌ | ✅ | **add** |
| Create CD (`createcd`) | ❌ | ✅ | **add** |
| Create DVD (`createdvd`) | ❌ | ✅ | **add** |
| `copy` (recompress) | ❌ | ✅ | **add** |
| Extract HD/CD/DVD | partial (`extractraw`) | ✅ | **add** |
| Metadata write/delete | ❌ | ✅ | **add** |
| Parent/diff CHDs (write) | read-only | ✅ | **add** |
| Runtime block writes (`HdImage`) | ❌ | ✅ | **add** |
| GD-ROM / Laserdisc / AV create | ❌ | deferred | **out of scope** |

## 2. Licensing position (important — premise correction)

The working assumption was "don't copy from MAME because it's GPL-3." The audit shows
the relevant MAME files are **NOT GPL** — the entire CHD core is **BSD-3-Clause**
(`// license:BSD-3-Clause copyright-holders:Aaron Giles` headers). Verified files in
`../mame/src/lib/util/`:

| File | License |
| --- | --- |
| `chd.cpp` / `chd.h` | BSD-3-Clause (Aaron Giles) |
| `chdcodec.cpp` / `chdcodec.h` | BSD-3-Clause (Aaron Giles) |
| `cdrom.cpp` / `cdrom.h` | BSD-3-Clause (Aaron Giles, R. Belmont) |
| `huffman.cpp` / `huffman.h` | BSD-3-Clause (Aaron Giles) |
| `hashing.cpp` / `hashing.h` | BSD-3-Clause (Aaron Giles, Vas Crabb) |
| `flac.cpp` / `avhuff.cpp` | BSD-3-Clause (Aaron Giles) |
| `../mame/src/tools/chdman.cpp` | BSD-3-Clause (Aaron Giles) |

**Implication:** We may legally *reference these algorithms as documentation and even
port them closely*, provided we preserve the BSD-3-Clause notice and Aaron Giles'
copyright in our headers. BSD-3 → BSD-3 is fully compatible. (MAME *as a whole project*
is GPL-2.0+, but that license attaches to the emulator/driver code we are not touching.)

**Policy adopted by this plan:**
- Reference only the BSD-3-Clause util files above. Do **not** read or port anything
  outside `src/lib/util/` + `src/tools/chdman.cpp`.
- Stay pure-Rust by *re-deriving* algorithms in idiomatic Rust (chd-rs's existing
  house style), not transliterating C++. Where a file is a close functional port
  (e.g. ECC tables, `guess_chs`), add an SPDX/attribution comment:
  `// Algorithm ported from MAME src/lib/util/<file> (BSD-3-Clause, © Aaron Giles).`
- Keep the existing `LICENSE.md` (BSD-3, © 2022 Ronny Chan); add a `NOTICE`/THIRD-PARTY
  section crediting MAME's CHD authors for the referenced algorithms.

## 3. Gap analysis

chd-rs is cleanly layered (`header`, `map`, `metadata`, `compression/*`, `chdfile`,
`read`) but **every path is decode-only**:

- **Codecs are all decode-only.** `CodecImplementation` (`chd-rs/src/compression/mod.rs:43`)
  has `decompress()` and no `compress()`. All 11 codecs implement decode only.
- **No map *writing*.** `map.rs` parses V5 compressed maps + legacy maps; there is no
  encoder (the inverse of `compress_v5_map`).
- **No header writing**, no metadata writing, no SHA-1 *generation* (only parent SHA-1
  *validation* at `chdfile.rs:54`).
- **`Chd<F>` requires `F: Read + Seek`** — read-only trait bounds throughout.

### Reusable assets already in-tree (big head start)

- **CD ECC tables already present**: `chd-rs/src/compression/ecc.rs` has `ECC_LOW`,
  `ECC_HIGH`, `ECC_P_OFF`, `ECC_Q_OFF` and the verify path. CD MODE1 ECC/EDC
  *generation* is the same tables run "forwards" — low risk.
- **Huffman decoder** + gated `huff_write` field scaffolding (`huffman.rs`) — the
  static-Huffman *encoder* (needed for V5 map + `huff` codec) can sit alongside it.
- **Deflate encode is free**: `flate2` (already a dep) does raw-deflate *encoding*, not
  just decoding.
- Full, correct parsers for every header/map/metadata structure → exact byte layouts to
  invert for writing.

### Codec encoder availability (the central risk — see also §8)

| CHD codec | Decode dep (today) | Encode path | Risk |
| --- | --- | --- | --- |
| `none` | trivial | trivial | none |
| `zlib`/Deflate | flate2 | **flate2 encodes** (raw deflate) | low |
| `huff` | in-tree decoder | port MAME static Huffman encoder (BSD-3) | low–med |
| `lzma` | `lzma-rs-perf-exp` (decoder fork) | needs **raw** LZMA encode (lc3/lp0/pb2, no end-marker) | **med–high** |
| `zstd` / `cdzs` | `ruzstd` 0.8 (decoder) | `ruzstd` encoder support is immature; may need C `zstd` behind a feature | **med** |
| `flac` / `cdfl` | `claxon` (decode-only) | **no pure-Rust FLAC encoder in deps** → `flacenc` crate or custom raw-FLAC | **high** |
| `cdlz`/`cdzl` | in-tree | wrap base encoders + ECC/subcode split | med |
| `avhuff` | in-tree | defer (AV out of scope) | n/a |

## 4. Architecture / approach

> **Encode-layer design is specified in [`docs/encode-integration.md`](docs/encode-integration.md)** —
> how `lzma-sdk-rs` / `libflac-rs` / `libzstd-bitexact-rs` + in-tree `zlib`/`huff` plug into the
> existing codec layer (the `CodecEncode` trait, per-codec mapping, CD ECC reuse, cargo wiring).

Additive, non-breaking. Keep `Chd<F: Read + Seek>` for reading; add a parallel
**write layer** and a libchdman-rs-shaped **facade**.

1. **Encoder trait.** Extend the codec layer with an opt-in encode capability, e.g.
   ```rust
   pub trait CompressionCodecEncode {
       fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize>; // Err if expands
   }
   ```
   Implement per codec, gated initially behind a `write` feature so read-only consumers
   pay nothing.
2. **Writer core (`chd-rs/src/write/`).** A `ChdWriter<W: Write + Seek>` that owns:
   header serialization, the hunk-write pipeline (try configured codecs → pick smallest,
   else self/parent/uncompressed), V5 compressed-map encoding, metadata linked-list
   writing, and SHA-1 accumulation (raw + overall, matching MAME's ordering so
   `verify` passes against chdman output).
3. **Compressor driver.** Pure-Rust equivalent of MAME's `chd_file_compressor`:
   pull hunks from a source, compress, dedup self/parent hunks, emit map. Single-threaded
   first; optional `rayon` parallelism later.
4. **Format facade modules** mirroring libchdman-rs names: `hd`, `cd`, `dvd`, `copy`,
   plus `codec` (FourCC constants + `parse_codec_spec`/`codec_name`/`codec_exists`).
5. **rchdman** gains `createhd`/`createcd`/`createdvd`/`copy`/`addmeta`/`delmeta`
   subcommands.

## 5. API parity mapping

Module/function/option **names match libchdman-rs**. The one unavoidable divergence is
the `Chd` *type itself*: libchdman-rs's `Chd::open(path, writeable, parent)` is an owned
FFI handle, whereas chd-rs's `Chd<F>` is generic over a borrowed reader. **Recommended
(Decision D3):** keep chd-rs's generic `Chd<F>` for reads, and match parity at the
module/free-function level, which is where the porting value is.

| libchdman-rs | chd-rs target |
| --- | --- |
| `hd::create_from_path` / `create_from_reader` | same names, `write/hd.rs` |
| `hd::extract_to_path` / `extract_to_writer` | same |
| `hd::{HdCreateOptions, HdGeometry, compute_chs, format_gddd, read_geometry}` | same |
| `hd::HdImage` (+ `open_with_diff`, `reopen_diff`, `read_sector`/`write_sector`) | same |
| `cd::{create_from_cue, create_from_iso, list_tracks, extract_to_cue/iso/gdi}` | same |
| `cd::{CdCreateOptions, TrackInfo, TrackType, SubcodeType, CdCookedReader}` | same |
| `dvd::{create_from_iso, create_from_reader, extract_to_iso, DvdCreateOptions}` | same |
| `copy::{copy, CopyOptions}` | same |
| `codec::{parse_codec_spec, codec_name, codec_exists, CHD_CODEC_*}` | same |
| `CompressionProgress { bytes_done, bytes_total, ratio }` | same |
| `Chd::open(path, writeable, parent)` | **diverges** — see D3 |

## 6. Milestones

Each milestone is independently shippable, gated behind the `write` feature, and lands
with tests that round-trip through the existing decoder **and** verify against chdman
where available (D1).

- **M0 — Foundations.** `write` feature; `CompressionCodecEncode` trait; SHA-1
  generation (`sha1` dep) matching MAME's raw+overall ordering; round-trip test harness;
  `NOTICE` attribution. Ref: `chd.cpp` `compute_overall_sha1`, `metadata_hash`.
- **M1 — V5 writer core + `none`/`zlib`.** Header V5 write, `compress_v5_map`
  (RLE+Huffman) encoder, uncompressed/self-hunk dedup, write an uncompressed and a
  zlib-only CHD that chdman reads. Ref: `chd.cpp:2071` `compress_v5_map`.
- **M2 — Codec encoders.** `huff` (port `huffman.cpp` encoder), `lzma` (raw),
  `zstd`. Each: compress→our-decoder→chdman round-trip. (FLAC tracked separately, D2.)
- **M3 — `hd` module + `codec` module.** `compute_chs` (port `guess_chs`,
  `chdman.cpp:1115`), GDDD/IDNT metadata, create/extract, `parse_codec_spec`.
- **M4 — Metadata write/delete + `copy`.** Metadata linked-list writer; `copy` clones
  metadata + recompresses. Ref: `chd.cpp` `write_metadata`/`delete_metadata`.
- **M5 — `dvd` module.** Flat 2048 sectors + empty `DVD ` record (note the 1-NUL-byte
  quirk libchdman-rs documents). Defaults `[lzma,zlib,huff,flac]` (needs M2/FLAC).
- **M6 — `cd` module.** CUE/GDI/ISO/Nero TOC parser (pure Rust), CD codec *encoders*
  (sector+subcode split, ECC/EDC synthesis via `ecc.rs` tables run forward), CHT2
  metadata, `list_tracks`, extract to cue/iso/gdi. Ref: `cdrom.cpp` `ecc_generate:1400`.
- **M7 — Parent/diff + `HdImage` runtime writes.** Uncompressed diff children against a
  compressed parent; block-device `read_sector`/`write_sector`.
- **M8 — rchdman subcommands + docs.** `create*`/`copy`/`addmeta`/`delmeta`; port
  libchdman-rs's `docs/format-modules.md` + `docs/chdman-mapping.md`; README rewrite.

## 7. Testing strategy

- **Round-trip (always):** create → read back with chd-rs's own decoder → assert logical
  bytes + `raw_sha1` match the source.
- **chdman cross-check (D1):** dev-local feature `chdman_compat_tests` that shells out to
  a real `chdman` (and/or uses `../mame` build) to (a) `chdman verify` our output and
  (b) confirm `info` SHA-1s match. Mirrors libchdman-rs's gating so CI needs no chdman.
- **Fixtures:** reuse libchdman-rs's checked-in ISO/BIN/CUE fixtures (BSD-3) under
  `chd-rs/tests/`.
- **Fidelity expectation:** we target **logical + SHA-1 parity and chdman-readability**,
  *not* byte-identical files (see D1 / §8).

## 8. Pitfalls

1. **Byte-for-byte parity is effectively unattainable in pure Rust.** libchdman-rs is
   byte-exact only because it *is* MAME's compressors. Different deflate/LZMA/FLAC encoder
   implementations emit different (valid) bitstreams, and MAME's per-hunk codec-selection
   order would have to be matched exactly. Realistic target: files that **decompress to
   identical content (matching `sha1`/`raw_sha1`) and pass `chdman verify`**. Set this
   expectation up front (Decision D1).
2. **FLAC encoding is the hardest gap.** `claxon` is decode-only; CHD FLAC uses a specific
   raw framing (no seektable, block size tied to hunk frames, fixed STREAMINFO). Options:
   `flacenc` crate (pure Rust, permissive) wrapped to emit raw frames, or a custom
   minimal encoder. Until solved, the **DVD default `[lzma,zlib,huff,flac]` and CD `cdfl`
   can't be produced** — M5/M6 partially blocked (Decision D2).
3. **LZMA must be *raw*.** CHD uses raw LZMA (props lc=3/lp=0/pb=2, no end marker, dict
   size derived from hunk size). The decoder dep is a fork; its *encoder* path (if any)
   must produce that exact framing or chdman won't decode it.
4. **zstd encoder maturity.** Pure-Rust zstd encoding (`ruzstd`) is young; ratios/ível may
   be poor or incomplete. May need to gate a C-backed `zstd` encoder behind a feature,
   which dents the "no C toolchain" promise for that codec only (Decision D2).
5. **SHA-1 ordering must match MAME exactly** or `verify`/`info` SHA-1s won't line up.
   The overall SHA-1 folds in metadata in a defined order with the `CHD_MDFLAGS_CHECKSUM`
   filter; get this wrong and round-trips pass but cross-checks fail. Port carefully from
   `chd.cpp`.
6. **V5 compressed-map encoder is intricate** (RLE + self/parent compaction +
   Huffman-coded lengths + `lengthbits`/`selfbits`/`parentbits` + CRC-16). Mirror
   `compress_v5_map` (`chd.cpp:2071`) closely; easy to get subtly wrong.
7. **CUE/GDI/Nero parsing** is reimplemented from scratch (libchdman-rs delegated to
   MAME's `parse_toc`). Edge cases: pregap/postgap, INDEX 00/01, multi-FILE cues, split
   bins, GDI high-density area. Scope risk for M6.
8. **`Chd` API shape clash.** Existing generic `Chd<F>` vs libchdman-rs's owned handle —
   100% drop-in source compatibility at the `Chd` level isn't possible without a breaking
   change to the existing read API (Decision D3).
9. **Self/parent hunk dedup correctness.** The writer must reproduce CHD's self-ref and
   parent-ref hunk types and CRC bookkeeping, or produce larger-but-valid files. Start
   conservative (no dedup), add dedup once verify is green.
10. **chd-rs MSRV / `no_std` posture.** Adding `sha1`, possibly `flacenc`/`rayon` must
    respect the existing MSRV (1.59 advertised) and `std`/`no_std` feature split. Keep all
    write code behind `write` + `std`.

## 9. Open Decisions

These gate the plan and are being asked now.

- **D1 — Fidelity target.** Byte-for-byte vs logical+SHA-1+chdman-readable.
  *Recommendation: logical + SHA-1 + `chdman verify` clean.*
- **D2 — Encoder purity.** Strict pure-Rust (accept FLAC/zstd gaps/effort) vs allow
  optional C-backed encoders behind features vs limit initial codec scope to
  `none/zlib/lzma/huff` and defer FLAC/zstd encode.
- **D3 — API shape.** Module/function-level parity (keep generic `Chd<F>`) vs introduce a
  separate owned writeable handle mirroring libchdman-rs's `Chd` exactly.
- **D4 — Format scope/priority.** Full `hd`+`cd`+`dvd`+`copy` parity, or HD-first
  (smallest path to a usable writer for the MiSTer/rusty-backup use case) with CD/DVD
  later.

## 11. Bit-for-bit feasibility

D1 was chosen as **byte-for-byte identical to chdman**. This is the demanding option and
it reshapes everything, so the analysis is recorded here.

### Why bit-for-bit forces specific encoders

A CHD's exact bytes are determined hunk-by-hunk by two things:
1. **Each codec's exact compressed bitstream**, and
2. **Which codec "wins" each hunk** — MAME tries every configured codec and keeps the
   smallest output. A **1-byte** difference in any codec flips the winner for that hunk,
   which changes the map, the self/parent dedup, and cascades into a *completely*
   different file.

So *bit-for-bit ⟺ every codec encoder emits byte-identical output to the exact C library
MAME links, at the exact version and settings.* Verified MAME settings:

| Codec | MAME library | Settings (`chdcodec.cpp` / `flac.cpp`) | Bit-exact in pure Rust today? | Prerequisite |
| --- | --- | --- | --- | --- |
| `none` | memcpy | — | ✅ yes | — |
| `huff` | MAME static Huffman (`huffman.cpp`, BSD-3) | fully specified in-repo | ✅ yes (faithful port) | in-scope, small |
| `zlib` | **stock zlib** (confirmed; *not* zlib-ng) | `deflateInit2(L9, -MAX_WBITS, memLevel 8, default strategy)` | ⚠️ *maybe* via `zlib-rs` (aims for byte-identical-to-zlib) | validate `zlib-rs` output |
| `lzma` | LZMA SDK (7-zip) `LzmaEnc` | `level=8`, dict normalized from hunk size, BT4 | ❌ no Rust port is bit-exact | **bit-exact `LzmaEnc.c` port** |
| `zstd` | libzstd | `ZSTD_maxCLevel()` = 22 | ❌ no; output also drifts across zstd versions | **bit-exact libzstd encoder** |
| `flac` | libFLAC | `level=8`, subset off, fixed blocksize, verify/md5 off | ❌ no (`flacenc` ≠ libFLAC bytes) | **bit-exact libFLAC encoder** |

`none` + `huff` are free / faithful ports. `zlib` is one validated dependency away
(maybe). **`lzma`, `zstd`, `flac` each require reproducing a specific C library's bitstream
byte-for-byte — there is no pure-Rust implementation that does this for any of the three.**
They also pin output to the *exact* library versions MAME 0.288 bundles (zstd especially is
version-sensitive).

### The three-way tension

`pure Rust` + `bit-for-bit` + `no C` **cannot all hold** for lzma/zstd/flac with what exists
today. Pick the reconciling path:

- **Path A — bit-for-bit via shared C codec libs.** Write the CHD orchestration (header,
  map, metadata, SHA-1, compressor driver, dedup) in Rust, but link the *same* C encoders
  MAME uses (zlib, LZMA SDK, libFLAC, libzstd) at matching versions/settings. The only
  realistic route to true bit-for-bit. Drops "pure-Rust codecs" but avoids MAME's C++ core.
- **Path B — bit-exact pure-Rust encoder crates first.** Build `lzma-sdk`-exact,
  `libFLAC`-exact, and `libzstd`-exact encoders in Rust as standalone projects, then the
  writer. Satisfies pure-Rust + bit-for-bit. Research-grade, multi-month per codec, and
  brittle against upstream version drift.
- **Path C — drop bit-for-bit; pure-Rust + `chdman verify` parity.** Any correct encoders;
  validate by SHA-1 + `chdman verify`. Achievable now, keeps chd-rs's identity, but files
  are not byte-identical (this was the D1 option *not* chosen).
- **Path D — incremental (recommended first move).** Start now on the writer core +
  `none`/`huff`/`zlib`, which *are* bit-exact-feasible, and prove the entire pipeline
  (header, map encoder, metadata, SHA-1, dedup) byte-identical on uncompressed / huff / zlib
  CHDs. Schedule `lzma`/`zstd`/`flac` as separate encoder subprojects, choosing Path A vs B
  **per codec** when reached. Unblocks immediate progress without first solving the hardest
  codecs.

**Recommendation:** Path D to start, with Path A as the default for the three hard codecs
unless a strict no-C mandate forces Path B. Answer to "do I need to start other projects
first?": **for `none`/`zlib`/`huff`, no — start now; for `lzma`/`zstd`/`flac`, yes — each
is a separate bit-exact encoder project (Path A links the C lib; Path B reimplements it).**

### Encoder sourcing — C upstreams, versions, licenses (all permissive)

To be bit-for-bit, we must match the **exact** library versions MAME 0.288 bundles
(verified from `../mame/3rdparty/`). Good news: **none are GPL-only** — every one is
public-domain, zlib, or BSD-3, so we may link *or* port and relicense the port BSD-3.

| Codec | Upstream C project | Version in MAME | License | Existing Rust crate | Recommended route |
| --- | --- | --- | --- | --- | --- |
| `zlib` | zlib (madler) | **1.3.1** | zlib license | `zlib-rs` (byte-identical port; **already used** via flate2) | **Port (done) — validate byte-identity to 1.3.1** |
| `huff` | MAME `huffman.cpp` | (in MAME) | BSD-3 | in-tree decoder | **Port encoder** (small) |
| `lzma` | LZMA SDK (Igor Pavlov / 7-zip) | **23.01** | **public domain** | none bit-exact (`xz2`/`lzma-sys` wrap *liblzma*, different bytes) | **Port `LzmaEnc.c` + `LzFind.c`** (PD → BSD-3; self-contained, deterministic) |
| `zstd` | libzstd (Meta) | **1.5.5** | BSD-3 (dual GPLv2) | `zstd-sys`/`zstd-safe` (**already optional dep**) | **Link, pinned to 1.5.5** (porting impractical) |
| `flac` | libFLAC (Xiph.Org) | **1.4.3** | BSD-3 (lib only; tools are GPL/LGPL — not used) | `libflac-sys`/`flac-sys` | **Link 1.4.3 first; optional port later** |

Notes:
- **zlib is essentially solved**: chd-rs already decodes via `zlib-rs`, which is an
  explicit byte-compatible reimplementation of zlib. We only need to confirm its *deflate*
  output equals zlib 1.3.1 at `(level 9, -MAX_WBITS, memLevel 8, default strategy)`.
- **LZMA is the flagship port**: the 7-zip SDK encoder is public domain and self-contained
  (`LzmaEnc.c` + `LzFind.c` match finder), so a faithful Rust port can be bit-exact and is
  legally unencumbered. *Do not* use `liblzma`/`xz` — different encoder, different bytes.
- **zstd should be linked, not ported**: libzstd at level 22 uses an enormous, version-
  sensitive optimal parser; a bit-exact pure-Rust port is not realistic. The `zstd` crate
  already exists and is BSD-3. Pin it to a build that reports `1.5.5`.
- **FLAC**: link libFLAC 1.4.3 (BSD-3) to get bit-for-bit quickly; a Rust port of
  `stream_encoder.c` (LPC/apodization/Rice search must match exactly) is feasible later if
  a fully-pure-Rust build is wanted. `flacenc` does **not** match libFLAC's bytes.

Net: with this routing, only **zstd** (and, until ported, **flac**) require a C toolchain;
`none`/`zlib`/`huff`/`lzma` are pure Rust. All five are permissively licensed.

### D2 locked (2026-06-09): Path B — maximal pure Rust, as standalone crates

Decision: **bit-exact pure-Rust ports of every codec**, built as **separate reusable crates**
so other projects can consume them. chd-rs depends on them; the codec work is not buried
inside chd-rs.

**Status 2026-06-18 — all three crates are BUILT and bit-exact-verified** (sibling repos
under `../`). Each has a handoff doc under [`docs/codec-ports/`](docs/codec-ports/).

| Crate | Ports | Target version | Parity target needs | Aligned? |
| --- | --- | --- | --- | --- |
| *(existing)* `zlib-rs` | deflate (validate only) | zlib 1.3.1 | zlib 1.3.1 | ✅ (validate) |
| *(in-tree)* MAME static Huffman | V5 map + `huff` codec | `huffman.cpp` | — | ✅ |
| `lzma-sdk-rs` v0.2301.0 | raw LZMA encode (+dec) | LZMA SDK 23.01 | 23.01 | ✅ |
| `libflac-rs` v0.143.0 | libFLAC encode/decode | libFLAC 1.4.3 | 1.4.3 | ✅ (glibc libm) |
| `libzstd-bitexact-rs` v0.155.0 | libzstd encode/decode | zstd **1.5.5** | zstd **1.5.5** | ✅ |

**zstd drift RESOLVED (2026-06-19):** `libzstd-bitexact-rs` **0.155.0** (byte-exact vs zstd
1.5.5, chdman's version) is published to crates.io. chd-rs pins `=0.155` and gates `zstd`/`cdzs`
behind `write-zstd`. chdman's per-hunk path is **unknown-pledged-size** level 22, so use
`StreamEncoder::new(22).finish(..)` (not `with_pledged_src_size`, which downsizes windowLog and
changes the bytes); `compress(x, 22)` is byte-identical for a single `e_end`. The 0.157.x line
of the same crate tracks zstd 1.5.7 for other consumers. **Secondary:** `libflac-rs` float
parity is validated against **glibc**
libm — confirm the reference chdman is a glibc build (FLAC bit-exactness is libm-dependent on
both sides). The shared success criterion remains: **byte-identical output to the exact
bundled C version at the exact settings CHD uses**, proven by differential tests.

## 10. Out of scope (matches libchdman-rs TODO)

- GD-ROM (`creategd`/`extractgd`), Laserdisc/AV (`createld`/`createav`).
- v3/v4 *creation* (read stays supported).
- Reimplementing the `chdman` CLI wholesale (rchdman stays a thin proof-of-concept).
