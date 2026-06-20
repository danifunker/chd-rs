# Handoff: `lzma-sdk-rs` — bit-exact 7-zip LZMA encoder in Rust

> Working name. `lzma-rs`, `xz2`, `lzma-sys` are taken and **wrap a different encoder**
> (liblzma/xz) that produces different bytes. Pick a distinct name.

## 1. Goal & success criterion

A pure-Rust port of the **7-zip LZMA SDK 23.01 encoder** whose output is **byte-identical**
to `LzmaEnc_MemEncode(...)` for the same input and properties. Ship a matching raw decoder
too, for round-trip self-tests.

**Done =** for every input in the test corpus, `lzma_sdk_rs::encode(input, props)` returns
the exact bytes the C `LzmaEnc_MemEncode` returns (no LZMA file header, no end marker).

## 2. Provenance & license

- Upstream: 7-zip LZMA SDK **23.01**, vendored at `../mame/3rdparty/lzma/C/`.
- License: **public domain** — `../mame/3rdparty/lzma/DOC/lzma-sdk.txt:30` ("LZMA SDK is
  written and placed in the public domain by Igor Pavlov"). Publish the port **BSD-3-Clause**;
  no attribution is legally required but credit Igor Pavlov as a courtesy.
- This is the most legally frictionless of the three ports.

## 3. Source files to port (single-threaded encode only)

| C file | LOC | Role |
| --- | --- | --- |
| `LzmaEnc.c` / `.h` | 3144 / 83 | Encoder: range coder, optimal parser (`GetOptimum`), prob/state tables, props |
| `LzFind.c` / `.h` | 1717 / 159 | Match finder: BT4 binary tree, hash chains, window/cyclic buffer |
| `LzHash.h` | 34 | Hash constants (`kHash2/3Size`, CRC shifts) |
| `7zTypes.h` | 523 | Types (`UInt32`, `SRes`, error codes) — map to Rust types, mostly discard |
| `CpuArch.h/.c` | — | LE byte helpers (`SetUi32`) — reimplement in Rust, discard CPU detection |
| `Alloc.c/.h` | — | Allocator shim — replace with Rust `Vec`/allocations |

**Omit:** `LzFindMt.*` (multithreading — guard `#ifndef Z7_ST`), `Lzma2*`, `Xz*`, `7zDec`,
`Bcj*`, `Ppmd*`, AES. For tests only: `LzmaDec.c/.h`.

## 4. Exact settings CHD uses (`chdcodec.cpp:1310`)

```c
LzmaEncProps_Init(&props);
props.level = 8;
props.reduceSize = hunkbytes;     // caps the dictionary to the hunk size
LzmaEncProps_Normalize(&props);
```

After `LzmaEncProps_Normalize` (`LzmaEnc.c:68`), level 8 yields:

| Field | Value |
| --- | --- |
| `dictSize` | `min(1<<26, hunkbytes)` but `>= kReduceMin (4096)` (`LzmaEnc.c:81`) |
| `lc`, `lp`, `pb` | `3`, `0`, `2` |
| `algo` | `1` (optimal parser, **not** fast mode) |
| `fb` (fast bytes) | `64` |
| `btMode` | `1` (binary tree) |
| `numHashBytes` | `4` (→ BT4) |
| `mc` (match cycles / cutValue) | `48` |

Encode call (`chdcodec.cpp:1272`): `LzmaEnc_MemEncode(enc, dest, &len, src, srclen,
writeEndMark=0, ...)`. Output is the **raw LZMA stream with no 13-byte header and no EOS
marker**. The 5 decoder props bytes are derived separately via `LzmaEnc_WriteProperties`
(`decoder_props[0] = (pb*5+lp)*9+lc = 93 = 0x5D`, then LE `dictSize`) — chd-rs reconstructs
these from the hunk size at decode time; the encoder output does not carry them.

## 5. Suggested Rust module layout

```
lzma-sdk-rs/
  src/
    props.rs        // LzmaEncProps + Init/Normalize; level→params table
    rangecoder.rs   // RangeEnc: shift-low, encode bit/direct/tree; flush ×5 (LE)
    state.rs        // 12-state machine + next-state tables; prob model (11-bit, 2048)
    price.rs        // CProbPrice[512] price tables; GET_PRICE_* macros
    matchfinder/
      mod.rs        // CMatchFinder: window, cyclic buffer, read-block
      bt4.rs        // binary-tree-4 GetMatches; cutValue=48 search + tie-break
      hash.rs       // 2/3/4-byte rolling CRC hash (LzHash.h constants)
    optimum.rs      // GetOptimum DP parser (the heart); opt[] table
    encoder.rs      // public encode(): create→set_props→mem_encode loop
    decoder.rs      // raw LzmaDec port for round-trip tests (test-only)
  tests/
    differential.rs // vs C LzmaEnc_MemEncode gold vectors
```

## 6. Bit-exactness hazards (verified against the SDK)

1. **Range-coder flush is exactly 5 iterations** of `RangeEnc_ShiftLow` (`LzmaEnc.c:717`),
   little-endian. Off-by-one here corrupts the tail of every stream.
2. **Optimal-parser tie-breaks use strict `<`**, not `<=` (`LzmaEnc.c:1361,1406,1465`). On
   equal price the **first** candidate wins — replicate the comparison operators exactly.
3. **Match finder init order**: `Init_HighHash` → `Init_LowHash` → `Init_4` (pos starts at
   **1**) → `ReadBlock` (`LzFind.c:574`). Position must start at 1, hash tables zeroed in
   that order.
4. **BT4 search depth = cutValue (48)** and closest-distance tie-break: equal-length matches
   must return the **smallest distance**. The tree traversal order is part of the contract.
5. **Probability model is 11-bit** (`kNumBitModelTotalBits=11`, total 2048); price tables
   `CProbPrice[512]` precomputed at init. Reproduce the rounding in `GET_PRICE_*` exactly.
6. **State transition tables** (`kLiteral/Match/Rep/ShortRepNextStates`, `LzmaEnc.c:620`)
   select which prob arrays are used — copy verbatim.
7. **dictSize reduction** must apply `min(dictSize, reduceSize)` with the `kReduceMin=4096`
   floor (`LzmaEnc.c:81`) — affects max match distance → match selection.
8. **No SIMD / no `#ifdef MY_CPU_*`** influence on the single-threaded path; ensure all
   `Z7_ST` multithread blocks are disabled.

## 7. Differential testing

Gold generator (dev-local, links the vendored C):
```c
LzmaEncProps props; LzmaEncProps_Init(&props);
props.level = 8; props.reduceSize = srclen; LzmaEncProps_Normalize(&props);
CLzmaEncHandle e = LzmaEnc_Create(&alloc);
LzmaEnc_SetProps(e, &props);
SizeT dlen = cap;
LzmaEnc_MemEncode(e, dst, &dlen, src, srclen, /*writeEndMark=*/0, NULL, &alloc, &alloc);
// emit dst[0..dlen] as the gold vector for `src`
```
Assert `encode(src, props_for(srclen)) == gold` byte-for-byte. Corpus: all-zeros,
text/repetitive, an executable, random/incompressible, and dumped real CHD hunks at the
hunk sizes CHD uses (4096, 19584, 65536, …). Also round-trip via the ported decoder.

## 8. Public API (consumed by chd-rs and others)

```rust
pub struct LzmaProps { pub lc: u8, pub lp: u8, pub pb: u8, pub dict_size: u32,
                       pub fb: u32, pub mc: u32, /* algo=1, bt4 fixed */ }
impl LzmaProps {
    /// chd-rs calls this: level-8 props with dict reduced to `hunk_bytes`.
    pub fn chd_for_hunk(hunk_bytes: u32) -> Self;
}
/// Raw LZMA stream, no 13-byte header, no end marker — matches LzmaEnc_MemEncode.
pub fn encode(input: &[u8], props: &LzmaProps) -> Vec<u8>;
/// The 5 decoder property bytes for a given props (LzmaEnc_WriteProperties).
pub fn decoder_props(props: &LzmaProps) -> [u8; 5];
#[cfg(any(test, feature = "decode"))]
pub fn decode_raw(input: &[u8], props: &[u8; 5], out_len: usize) -> Vec<u8>;
```

## 9. Milestones

- **L0** Range coder + bit/tree/direct encoders; unit-test against hand-computed streams.
- **L1** Literal-only encoding (force no matches); match against C with a tiny dict.
- **L2** BT4 match finder + hash; verify match lists equal C's `GetMatches` on fixtures.
- **L3** Optimal parser (`GetOptimum`); first end-to-end byte-exact streams.
- **L4** Full corpus byte-exact at all CHD hunk sizes; ported decoder round-trips.
- **L5** API polish, docs, publish.

## 10. Risk / effort

**Medium.** Integer-only (no float hazards), public-domain, self-contained. The optimal
parser (`GetOptimum`, ~600 LOC of dense DP) is the hard part; everything else is mechanical.
Estimate ~3–4k LOC Rust. Highest-confidence of the three ports — **do this one first** to
establish the differential-test rig the others reuse.
