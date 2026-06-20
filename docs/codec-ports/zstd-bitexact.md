# Handoff: `zstd-bitexact` — bit-exact libzstd encoder (level 22) in Rust

> Working name. `zstd`/`zstd-safe`/`zstd-sys` (C bindings) and `ruzstd` (pure-Rust
> **decoder**) are taken. Pick a name that signals "bit-exact encoder."

## 1. Goal & success criterion

A pure-Rust port of the **libzstd 1.5.5 encoder at level 22** whose per-hunk output is
**byte-identical** to `ZSTD_compressStream2(..., ZSTD_e_end)`. This is the **largest** port
(~4k LOC) but the per-hunk usage strips away most of zstd's complexity (no dictionary, LDM,
multithreading, or cross-block streaming).

**Done =** for every input in the corpus, `zstd_bitexact::compress_l22(input)` equals the
bytes libzstd 1.5.5 produces for one independent `ZSTD_e_end` frame at `ZSTD_maxCLevel()`.

## 2. Provenance & license

- Upstream: libzstd **1.5.5** (Meta), vendored at `../mame/3rdparty/zstd/lib/`.
- License: **BSD-3-Clause OR GPLv2** (dual) — `lib/compress/zstd_compress.c:1` header; full
  text in `../mame/3rdparty/zstd/LICENSE`. **Select BSD-3-Clause**; do not pull from
  `COPYING` (the GPLv2 copy). Publish the port BSD-3-Clause, retaining Meta's copyright.
- ⚠️ libzstd output is **version-sensitive** — this port targets **1.5.5 exactly**. State
  that prominently; a different libzstd version is a different gold standard.

## 3. Source files involved at level 22

Strategy at level 22 is **`btultra2`** (`clevels.h:50`). Compress path:

| C file | LOC | Role |
| --- | --- | --- |
| `compress/zstd_compress.c` | 7032 | Orchestrator: frame/block headers, seq store, entropy dispatch |
| `compress/zstd_opt.c` | 1472 | **btultra2 optimal parser** — match finding + cost-model DP |
| `compress/zstd_compress_internal.h` | 1532 | `optState_t`, `ZSTD_optimal_t`, `seqStore_t` |
| `compress/huf_compress.c` | 1435 | Huffman literals: canonical tree, weight table |
| `compress/fse_compress.c` | 624 | FSE tables for litLength / matchLength / offset codes |
| `compress/zstd_compress_sequences.c` | 442 | Sequence entropy mode selection + packing |
| `compress/zstd_compress_literals.c` | 235 | Literal block: raw / RLE / compressed |
| `common/hist.c` | 181 | Histogram counting |
| `common/entropy_common.c`, `fse_decompress.c` | 340/311 | Normalization + (de)serialize tables |
| `common/bitstream.h`, `bits.h`, `mem.h` | — | LE bit packing, ilog2, endian helpers |
| `compress/clevels.h` | 134 | Level parameter tables |

**Omit (not used per-hunk):** `zstd_ldm.c`, `zstdmt_compress.c`, `zstd_double_fast.c`,
`zstd_fast.c`, `zstd_lazy.c` (lower-level strategies), dictionary code, row-hash match finder,
external sequence producer, checksum (xxhash).

## 4. Level-22 parameters (`clevels.h`)

Two rows depending on input size (CHD hunks are small → second row):

| srcSize | W | C | H | S | L (minMatch) | TL | strategy |
| --- | --- | --- | --- | --- | --- | --- | --- |
| > 256 KB | 27 | 27 | 25 | 9 | 3 | 999 | btultra2 |
| **≤ 256 KB** | **18** | **19** | **19** | **13** | **3** | **999** | **btultra2** |

CHD never sets `pledgedSrcSize` (no `ZSTD_CCtx_setPledgedSrcSize`), so:
- frame header **omits content size** and **dict ID** and **checksum**;
- window is clamped to the hunk; one block per hunk (hunks ≪ `ZSTD_BLOCKSIZE_MAX` 128 KB),
  so **no block splitting**.

## 5. Encode pipeline (per hunk)

```
ZSTD_initCStream(s, 22)
ZSTD_compressStream2(s, out, in, ZSTD_e_end)
  ├─ writeFrameHeader      → magic 0xFD2FB528 | FHD byte | window descriptor   (~6 bytes)
  ├─ compress one block:
  │    ├─ buildSeqStore    → ZSTD_compressBlock_btultra2 (zstd_opt.c): matches + optimal parse
  │    └─ entropyCompressSeqStore:
  │         ├─ literals    → huf_compress (or raw/RLE)
  │         └─ sequences   → fse_compress (litLen/matchLen/offset) + bitstream
  ├─ block header (3 bytes LE): lastBlock | blockType<<1 | blockSize<<3
  └─ final empty block if needed
```

## 6. Bit-exactness hazards (honest)

1. **Optimal parser cost model** (`zstd_opt.c:20`): `ZSTD_fracWeight` fixed-point fractional
   bits + `ZSTD_highbit32`. The DP **tie-breaks** (equal-cost paths) resolve by insertion
   order / first-match — replicate exactly. *Helpful fact:* per-hunk, the first block uses
   **predefined statistics** (no entropy feedback), so costs are reproducible.
2. **FSE/Huffman table construction** (`fse_compress.c:100`, `huf_compress.c`): histogram
   **normalization** and symbol ordering must match on ties (equal frequencies). This is the
   subtlest determinism point — port the normalization algorithm verbatim, including its
   tie-handling.
3. **Literals/sequences mode selection**: predefined vs RLE vs compressed vs repeat. Per-hunk
   the tables are fresh so **repeat is unavailable on the first/only block** — simplifies to
   predefined/compressed/RLE by cost, first-match on ties.
4. **Frame header byte (FHD)**: with no content size / dict / checksum it's a fixed value;
   reproduce the flag packing in `ZSTD_writeFrameHeader` (`zstd_compress.c:4473`).
5. **Block header**: `MEM_writeLE24(lastBlock | type<<1 | size<<3)`.
6. **No SIMD / version drift**: scalar path only; pin to 1.5.5 semantics. xxhash checksum is
   **off** (don't emit it). Repcodes init `{1,4,8}` fresh per hunk.

## 7. What per-hunk usage removes (scope relief)

No dictionary, no long-distance matching, no multithreading, no cross-block streaming, no
row-hash finder, no checksum, no content-size field. Each hunk is one isolated frame with
predefined entropy stats — i.e. the **stateless, deterministic subset** of zstd. This is what
makes a bit-exact port tractable at all.

## 8. Differential testing

Gold = libzstd 1.5.5:
```c
ZSTD_CStream* s = ZSTD_createCStream();
ZSTD_initCStream(s, ZSTD_maxCLevel());                 // 22
ZSTD_inBuffer in = {src, srclen, 0};
ZSTD_outBuffer out = {dst, srclen, 0};
ZSTD_compressStream2(s, &out, &in, ZSTD_e_end);        // out.pos = gold length
```
Assert `compress_l22(src) == dst[0..out.pos]`. Corpus: all-zeros (RLE), highly repetitive
(matches), random (entropy worst case), real CHD hunk dumps at CHD hunk sizes. Build the port
component-by-component (frame/block header → entropy tables on fixed seq sets → parser) and
diff at each stage.

## 9. Public API

```rust
/// One independent zstd frame at level 22, no dict/checksum/content-size —
/// byte-identical to libzstd 1.5.5 ZSTD_compressStream2(..., ZSTD_e_end).
pub fn compress_l22(input: &[u8]) -> Vec<u8>;
```
(Keep it minimal; CHD only needs level-22 single-shot. A `level: i32` param can come later if
other consumers want it, but other levels are out of scope for bit-exact parity here.)

## 10. Milestones

- **Z0** Frame + block headers + raw/RLE blocks byte-exact (covers all-zeros / incompressible).
- **Z1** FSE + Huffman table construction byte-exact on **fixed, hand-supplied** sequence/
  literal sets (isolate entropy from the parser).
- **Z2** btultra2 match finder + optimal parse producing the same sequences as C.
- **Z3** Full pipeline byte-exact on the corpus at CHD hunk sizes.
- **Z4** API + docs + publish.

## 11. Risk / effort

**High / largest (~3.5–4.5k LOC).** Feasible *because* per-hunk usage is the deterministic
subset, but the optimal parser cost model and entropy-table tie-breaks are exacting. Port
**last** of the three. If full bit-exactness on the parser proves too costly, a documented
fallback is to keep `zstd` as a C-linked codec **only for the zstd path** while the rest of
chd-rs stays pure Rust — but that reopens the "no C toolchain" exception this project exists
to avoid, so treat it as a last resort, not a plan.
