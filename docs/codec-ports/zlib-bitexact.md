# Handoff: `zlib-bitexact-rs` — bit-exact stock-zlib 1.3.1 deflate in Rust

> ✅ **BUILT & PUBLISHED (2026-06-20).** This outgoing spec is fulfilled: `zlib-bitexact-rs`
> **0.131.0** is on crates.io, byte-exact vs stock zlib 1.3.1 `deflate` at the CHD config. The
> module layout, hazards, and milestones below were the build plan. **To wire it into chd-rs, see
> [`zlib-bitexact-integration.md`](zlib-bitexact-integration.md)** (the return handoff).

> Working name (matches the `lib*-bitexact-rs` family). ⚠️ `zlib-rs`, `miniz_oxide`,
> `flate2`, `libdeflater` all exist and are **NOT byte-identical** to stock zlib — that's
> exactly why this is needed. `zlib-rs` 0.4.2 (chd-rs's current backend) is a from-scratch,
> zlib-ng-quality deflate that produces ~2 bytes smaller output than stock zlib 1.3.1.

## 0. Why this is the highest-priority codec port

zlib is on the critical path for **both** default codec sets:
- **HD default** = `[zlib]` — every hard-disk CHD.
- **CD default** = `[cdlz, cdzl, cdfl]` — **all three** CD codecs use zlib: `cdzl` deflates the
  sector + subcode streams; `cdlz` and `cdfl` deflate the **subcode** sub-stream.

So without bit-exact zlib, neither HD nor CD CHDs can be byte-identical to chdman. lzma/flac/zstd
are already done; **this is the remaining blocker for the primary use cases.**

## 1. Goal & success criterion

A pure-Rust port of **stock zlib 1.3.1's deflate** whose output is **byte-identical** to the C
`deflate()` at the exact settings the CHD codec uses. Decode is not needed (chd-rs already
inflates via `flate2`/`zlib-rs` — inflate is unambiguous).

**Done =** for every input in the corpus, `zlib_bitexact::deflate_raw(input)` equals the bytes C
zlib 1.3.1 produces with `deflateInit2(9, Z_DEFLATED, -15, 8, Z_DEFAULT_STRATEGY)` + a single
`deflate(Z_FINISH)`.

## 2. Provenance & license

- Upstream: **zlib 1.3.1** (Jean-loup Gailly & Mark Adler), vendored at `../mame/3rdparty/zlib/`.
- License: the **zlib license** (permissive, BSD-like: "use freely, subject to … don't
  misrepresent the origin … mark altered versions"). Fully compatible — publish the port BSD-3
  (or zlib), retaining the Gailly/Adler copyright.

## 3. Source files to port (deflate/encode only)

| C file | Role |
| --- | --- |
| `deflate.c` / `deflate.h` | The deflate engine: `deflate_slow` (lazy matching at level 9), `longest_match`, hash chains (`INSERT_STRING`/`UPDATE_HASH`), `fill_window`, the level config table |
| `trees.c` / `trees.h` | Huffman trees: `build_tree`, `scan_tree`/`send_tree`, `compress_block`, `_tr_flush_block` (stored vs static vs dynamic block choice), the fixed trees |
| `zutil.h` | Constants / small helpers |

**Omit:** inflate (`inflate.c`, `infback.c`, `inftrees.c`), gzip framing, `adler32.c`/`crc32.c`
(raw deflate has no checksum), `compress.c`/`uncompress.c`, dictionaries, and all flush modes
except `Z_FINISH` (CHD compresses each hunk one-shot).

## 4. Exact settings CHD uses (`chdcodec.cpp:910`)

```c
deflateInit2(&stream, Z_BEST_COMPRESSION /*9*/, Z_DEFLATED,
             -MAX_WBITS /*-15: raw, no zlib header*/, 8 /*memLevel*/, Z_DEFAULT_STRATEGY);
// then a single deflate(&stream, Z_FINISH) over the whole hunk.
```

Level-9 config (from zlib's `configuration_table[9]`): `good_match=32`, `max_lazy=258`,
`nice_match=258`, `max_chain=4096`, function = `deflate_slow`. memLevel 8 → `hash_bits=15`,
`hash_size=32768`, `lit_bufsize` per memLevel. Raw deflate (windowBits −15) → **no 2-byte zlib
header and no adler32 trailer** — the output is a bare DEFLATE stream, exactly what the CHD map
stores.

## 5. Suggested Rust module layout

```
zlib-bitexact-rs/
  src/
    deflate.rs     // deflate_state, deflate_slow, fill_window, level-9 config
    longest_match.rs // the match finder (hash chains, max_chain/nice/good/lazy thresholds)
    trees.rs       // dynamic/static/stored block build + emit; _tr_flush_block
    bitwriter.rs    // deflate's LSB-first bit packing (bi_buf/bi_valid) — NOT MSB-first!
    lib.rs         // pub fn deflate_raw(input) -> Vec<u8>  (level 9, raw, memLevel 8)
  cref/            // dev-only: vendored zlib 1.3.1 C + cc + FFI shim (the oracle)
  tests/differential.rs
```

## 6. Bit-exactness hazards

1. **LSB-first bit order.** deflate packs bits **least-significant-first** (`send_bits` →
   `bi_buf`/`bi_valid`), the opposite of MAME's `bitstream_out` (MSB-first, used by huff/the map).
   Don't reuse the huffman-encoder `BitWriter`; deflate needs its own.
2. **Lazy matching (`deflate_slow`).** The decision to defer a match by one byte hinges on exact
   `match_length`/`prev_length` comparisons and the `good_match`/`max_lazy` thresholds. Off-by-one
   here diverges immediately.
3. **`longest_match` tie-breaks.** Hash-chain traversal order, the `max_chain_length` cap, the
   `nice_match` early-out, and the "prefer closer match of equal length" rule must match exactly.
4. **Block-boundary decisions (`_tr_flush_block`).** When zlib closes a block and whether it emits
   it **stored / static / dynamic** is a cost comparison (`static_len` vs `opt_len` vs stored).
   Reproduce it bit-for-bit, including the `lit_bufsize`/`sym_end` full-buffer trigger.
5. **Hash function.** `UPDATE_HASH` uses `hash_shift = (hash_bits+MIN_MATCH-1)/MIN_MATCH`; with
   memLevel 8 → hash_bits 15. The exact hash determines chain contents → matches found.
6. **Window/`fill_window`.** For a single-hunk one-shot the whole input fits the 32 KiB window;
   still match zlib's window-fill and `strstart`/`lookahead` bookkeeping.

## 7. Differential testing

Gold = C zlib 1.3.1 (vendored under `cref/`), `deflateInit2(9, Z_DEFLATED, -15, 8,
Z_DEFAULT_STRATEGY)` + `deflate(Z_FINISH)`. Corpus: all-zeros, runs, text, random, and **real CHD
hunks** at CHD hunk sizes (4096, 2448-multiples for CD subcode, 19584). The killer test:
chd-rs's `chdman_compat::zlib_bit_exact_vs_chdman` (already written, currently `#[ignore]`) — flip
it on once this crate is wired and it must pass against chdman 0.288.

## 8. Public API

```rust
/// Raw DEFLATE stream (no zlib header/trailer), byte-identical to zlib 1.3.1
/// deflateInit2(9, Z_DEFLATED, -15, 8, Z_DEFAULT_STRATEGY) + deflate(Z_FINISH).
pub fn deflate_raw(input: &[u8]) -> Vec<u8>;
```
(That single entry point is all CHD needs. A `level`/`mem_level` parameter can come later.)

## 9. Milestones

- **D0** Bit writer (LSB-first) + stored blocks; byte-exact on incompressible input.
- **D1** Static-Huffman blocks; byte-exact on small/simple input.
- **D2** `longest_match` + `deflate_slow`; match the sequence of matches zlib finds.
- **D3** Dynamic Huffman trees + `_tr_flush_block` cost choice; full corpus byte-exact.
- **D4** Real CHD hunks byte-exact vs chdman; wire into chd-rs; un-ignore the zlib compat test.

## 10. Risk / effort

**Medium.** Deflate is intricate (lazy matching + tree cost decisions) but well-understood,
deterministic, fully specified in `deflate.c`/`trees.c` (~3–4k lines C total, less to port since
inflate/gzip/checksums are excluded), and permissively licensed. Comparable to `lzma-sdk-rs`.
**Alternative (faster, not pure-Rust):** link C zlib **1.3.1** for the encoder only (e.g. a thin
`libz-sys` pinned to 1.3.1) — gives instant bit-exactness but reintroduces a C toolchain for the
zlib path. Recommended only if the port is deferred; the port keeps chd-rs fully pure-Rust.
