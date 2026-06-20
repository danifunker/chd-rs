# CHD codec ports — bit-exact pure-Rust encoders

This directory holds **handoff specs** for the standalone Rust crates needed to give
[chd-rs](../../PARITY_PLAN.md) bit-for-bit CHD *write* support without any C toolchain.

Each crate is **independently useful** — anyone wanting byte-identical 7-zip LZMA,
libFLAC, or libzstd output in Rust can depend on it. chd-rs is just the first consumer.

## Why these exist

A CHD file's exact bytes are decided hunk-by-hunk by (1) each codec's exact compressed
bitstream and (2) which codec "wins" each hunk (smallest output). So **bit-for-bit parity
with `chdman` requires every encoder to emit output byte-identical to the exact C library
MAME bundles, at the exact settings CHD uses.** No existing Rust crate does this for LZMA,
FLAC, or zstd. These specs are the projects that close that gap. See
[PARITY_PLAN.md §11](../../PARITY_PLAN.md) for the full rationale.

## The projects — BUILT (sibling repos under `../`)

All four crates now exist and are bit-exact-verified against their C oracle. Status as of
2026-06-20:

| Crate | Spec | Targets | libchdman-rs (MAME 0.288) bundles | Aligned? | State |
| --- | --- | --- | --- | --- | --- |
| `lzma-sdk-rs` v0.2301.0 | [lzma-sdk-rs.md](lzma-sdk-rs.md) | LZMA SDK **23.01** | 23.01 | ✅ | byte-exact vs `LzmaEnc_MemEncode` across corpus incl. CHD hunk sizes; encoder+decoder; zero-dep |
| `libflac-rs` v0.143.0 | [libflac-rs.md](libflac-rs.md) | libFLAC **1.4.3** | 1.4.3 | ✅ | byte-exact for CHD config + all levels/depths + decoder + Ogg; has `encode_frames()`; zero-dep. ⚠️ float parity validated vs **glibc** libm |
| `libzstd-bitexact-rs` **v0.155.0** | [zstd-bitexact.md](zstd-bitexact.md) | zstd **1.5.5** | **1.5.5** | ✅ | full bit-exact zstd (all 22 levels + dict + streaming + MT + LDM), now **aligned to the 1.5.5 chdman uses**; published to crates.io |
| `zlib-bitexact-rs` **v0.131.0** | [zlib-bitexact.md](zlib-bitexact.md) · [integration](zlib-bitexact-integration.md) | stock zlib **1.3.1** | **1.3.1** | ✅ | byte-exact vs stock zlib 1.3.1 `deflate` at the CHD config (level 9, raw, memLevel 8); encode-only; zero-dep; **published to crates.io** |

### ✅ zstd drift RESOLVED (2026-06-19)

`libzstd-bitexact-rs` **0.155.0** is byte-exact with **zstd 1.5.5** (chdman's version, confirmed
via `../libchdman-rs/deps/mame/3rdparty/zstd`) and published to crates.io. The 0.157.x line of
the same crate stays on zstd 1.5.7 for consumers tracking current upstream.

**chd-rs wiring:** pin `libzstd-bitexact-rs = "=0.155"` (a bare add resolves to 0.157.x). chdman
compresses each hunk at level 22 with an **unknown pledged size**, so use
`StreamEncoder::new(22).finish(input, &mut out)` — **not** `with_pledged_src_size`, which
downsizes `windowLog` (27) and changes the bytes. `compress(input, 22)` is byte-identical for a
single `e_end`. For `cdzs`, the same encoder applies per sub-stream. ⚠️ A round-trip test cannot
catch encoder drift (zstd decode is format-stable) — guard byte-identity by comparing
*compressed* bytes to a zstd-1.5.5 golden. Deferred edge cases in the crate (non-canonical
`nbSeq==0` reject, large-dict suffix truncation, 4 GiB ceiling, ≤8-byte CDict, large-window LDM
*with* a dict) **do not touch chdman's no-dict, hunk-sized, level-22 path**.

> **Crate naming note:** the spec file is still `zstd-bitexact.md`; the shipped crate is
> `libzstd-bitexact-rs` (matches the `lib*-rs` family + the libchdman-rs naming).

### ✅ zlib DONE (2026-06-20)

`zlib-bitexact-rs` **0.131.0** is a pure-Rust port of stock zlib 1.3.1 `deflate.c`/`trees.c`,
byte-identical to the C `deflate()` at the CHD config (level 9, raw `-15`, memLevel 8,
`Z_DEFAULT_STRATEGY`, one `Z_FINISH`), verified against the vendored zlib 1.3.1 C oracle and
**published to crates.io**. This closes the last default-codec gap: `zlib-rs` 0.4.2 (the old
backend) is a from-scratch, zlib-ng-quality deflate that is **~2 bytes smaller** than stock zlib
1.3.1 (proven by `chdman_compat::zlib_bit_exact_vs_chdman`: 1523 vs 1525). zlib is the **HD
default** *and* used by every **CD** codec (subcode + `cdzl`), so this was the highest-priority
remaining port.

**chd-rs wiring:** ✅ **done** (2026-06-20, per
**[zlib-bitexact-integration.md](zlib-bitexact-integration.md)**) — `zlib-bitexact-rs` added under
`write`, `compression/zlib.rs`'s `ZlibEncoder` swapped from `flate2::Compress` to
`zlib_bitexact_rs::deflate_raw` (decoder stays flate2), and `chdman_compat::zlib_bit_exact_vs_chdman`
un-ignored → **passes** against chdman 0.288, with a full `-c zlib` CHD byte-identical end-to-end.

### In-tree (no separate crate)

- **MAME static Huffman** (V5 map + `huff` codec) → small, CHD/MAME-specific, BSD-3. Lives
  **in-tree** in chd-rs (`huffman_encode.rs`, alongside the decoder). ✅ **done & bit-exact vs
  chdman 0.288.** Split into a `mame-huffman` crate only if another consumer appears.

## Shared rules for all three ports

1. **Success criterion = byte-identical to the C reference.** Not "valid output," not
   "decompresses correctly" — the exact same bytes as the bundled C library at the exact
   pinned version and settings. A single differing byte fails.

2. **Pin the version.** Reproduce the *specific* version MAME 0.288 bundles (LZMA SDK
   23.01 / libFLAC 1.4.3 / libzstd 1.5.5). These libraries change their output across
   versions; the port targets one frozen version. State it in the crate's docs.

3. **Differential testing is the whole game.** Each crate ships a dev-only harness that
   builds the C reference (from `../mame/3rdparty/<lib>` or pinned upstream) into a tiny
   "gold vector" generator, runs a corpus through both, and asserts `rust_out == c_out`
   byte-for-byte. Corpus must include: all-zeros, highly-repetitive, random/incompressible,
   real CHD hunk dumps, and the format's edge cases. CI gates on zero mismatches. The C
   reference is dev-local (like libchdman-rs's `chdman_compat_tests`); downstream consumers
   never need a C toolchain.

4. **Portable path only.** Port the reference C scalar path, never the SIMD/intrinsic
   variants (`*_intrin_*`, `*_asm_*`) — they can differ from the scalar path (esp. FLAC
   float autocorrelation) and would break bit-exactness. Optimize *after* parity holds, and
   only with SIMD proven bit-identical to the scalar path.

5. **Permissive licensing.** All three upstreams are public-domain or BSD-3. Carry the
   original copyright + a `// Ported from <lib> <version> (<license>)` note in each module,
   and publish the crate BSD-3-Clause. Do **not** pull from any GPL/LGPL file (FLAC's CLI
   tools, zstd's `COPYING`) — use only the library sources, which are BSD-3.

6. **No dependency on chd-rs.** These are general-purpose codec crates. chd-rs depends on
   them, not the reverse.

## Recommended build order

1. **`lzma-sdk-rs`** — cleanest (public domain, integer-only range coder, self-contained,
   no float hazards). Best first port; proves the differential-testing methodology.
2. **`libflac-rs`** — medium-high; the float arithmetic (windows, autocorrelation, LPC
   quantization `lround`) is the main risk. Needed for DVD default + CD `cdfl`.
3. **`zstd-bitexact`** — highest LOC; the level-22 optimal parser + FSE/Huffman table
   determinism. Per-hunk usage removes dictionaries/LDM/MT/streaming, which helps.

chd-rs's writer core (header, V5 map encoder, metadata, SHA-1, compressor driver) can be
built **in parallel** using `none`/`zlib`/`huff` first (see PARITY_PLAN.md M0–M1), so the
codec ports are not a hard blocker for early progress.
