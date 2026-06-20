# Handoff: `libflac-rs` — bit-exact libFLAC encoder in Rust

> Working name. `flacenc` (the existing pure-Rust encoder) is taken and is **NOT
> byte-identical to libFLAC** — that's exactly why this project exists. Pick a name that
> signals bit-exactness (e.g. `libflac-rs`, `flac-bitexact`).

## 1. Goal & success criterion

A pure-Rust port of the **libFLAC 1.4.3 encoder** whose frame output is **byte-identical**
to libFLAC at the settings CHD uses. This is the **trickiest** of the three ports because
the encoder makes decisions in **floating point** (windows, autocorrelation, LPC
quantization) where rounding must match the C reference exactly.

**Done =** for every PCM input in the corpus, the encoded FLAC frames (STREAMINFO/seektable
stripped, as CHD does) are byte-identical to MAME's `flac_encoder` output.

## 2. Provenance & license

- Upstream: libFLAC **1.4.3** (Xiph.Org), vendored at `../mame/3rdparty/flac/src/libFLAC/`.
- License: **BSD-3-Clause** — every `src/libFLAC/*.c` carries the Xiph BSD-3 header
  (`stream_encoder.c:1`, © 2000-2009 Josh Coalson, © 2011-2023 Xiph.Org). The
  `COPYING.GPL`/`COPYING.LGPL` files cover only the **CLI tools**, not the library — do not
  read or port from those. Publish the port **BSD-3-Clause**, retaining the Xiph copyright.

## 3. Source files to port (encoder, portable path only)

| C file | LOC | Role |
| --- | --- | --- |
| `stream_encoder.c` | 4738 | Orchestrator: frame/subframe selection, level presets, partition search |
| `lpc.c` | 1629 | Windowing, autocorrelation, Levinson-Durbin, quantization, residual |
| `fixed.c` | 667 | Fixed predictors (orders 0–4), best-order selection |
| `bitwriter.c` | 955 | Bit I/O, CRC-8/CRC-16 |
| `stream_encoder_framing.c` | 594 | Frame header/footer + (stripped) STREAMINFO synthesis |
| `window.c` | 308 | Apodization windows (Tukey, `subdivide_tukey`) |
| `crc.c` | 436 | CRC-8 (header), CRC-16 (footer) |
| `format.c` | 608 | Header/validation constants |
| `memory.c` | 219 | Allocation — replace with Rust |
| `md5.c` | 517 | **Skip** — CHD disables MD5 |

**Omit every `*_intrin_*` / `*_asm_*` and the `deduplication/*_intrin_*` autocorrelation
variants.** Port only the scalar reference paths (`lpc.c:133` autocorrelation loop, etc.).
SIMD float paths use 24-bit intermediates and **diverge** from the scalar `double` path.

## 4. Exact settings CHD uses (`flac.cpp:77`, `chdcodec.cpp`)

```c
set_verify(false); /* md5 off */ set_compression_level(8);
set_channels(2); set_bits_per_sample(16); set_sample_rate(44100);
set_total_samples_estimate(0); set_streamable_subset(false);
set_blocksize(m_block_size);   // per-hunk, see below
```

**Level-8 preset** (`stream_encoder.c:124`, last row):
```
do_mid_side=true, loose_mid_side=false, max_lpc_order=12, qlp_coeff_precision=0(auto),
do_qlp_coeff_prec_search=false, do_escape_coding=false, do_exhaustive_model_search=false,
min_residual_partition_order=0, max_residual_partition_order=6,
rice_parameter_search_dist=0, apodization="subdivide_tukey(3)"
```

**Block size** (`chdcodec.cpp:1520`): `blocksize(bytes) = bytes/4`, halved while `>2048`
→ typically **2048** samples. Raw FLAC hunk: `bytes = hunk_bytes`. CD FLAC:
`bytes = frames_in_hunk * 2048` (`chdcodec.cpp:1602`).

**Raw framing**: CHD strips STREAMINFO/seektable (`flac.cpp:72,263` — `m_strip_metadata`,
skip leading metadata via `m_ignore_bytes`). The CHD raw-FLAC codec also prepends a single
**endian flag byte** `'L'`/`'B'` (`chdcodec.cpp:503`) and encodes both little- and
big-endian framings, keeping the smaller (`chdcodec.cpp:1498`). For CD, audio → FLAC and the
96-byte subcode → zlib deflate, split per `chdcodec.cpp:1634`.

## 5. Suggested Rust module layout

```
libflac-rs/
  src/
    bitwriter.rs    // bit packing + CRC-8/CRC-16 (bitwriter.c, crc.c)
    window.rs       // tukey + subdivide_tukey(3); EXACT float constants / cosf
    lpc/
      autocorr.rs   // double-precision dot products (scalar path only)
      levinson.rs   // Levinson-Durbin (double); reflection-coeff division
      quantize.rs   // qlp quantization with lround-equivalent + error feedback
      residual.rs   // FIR residual filter (16-bit + wide overflow path)
    fixed.rs        // fixed predictors 0..4, SSE-free
    rice.rs         // partition-order search, parameter estimation, Golomb-Rice
    subframe.rs     // CONSTANT/VERBATIM/FIXED/LPC selection
    frame.rs        // frame header/footer, mid-side decorrelation
    encoder.rs      // public API; level-8 preset wiring; per-block process
  tests/
    differential.rs // vs MAME flac_encoder gold vectors
```

## 6. Bit-exactness hazards — FLOAT is the enemy

This is where a careless port silently diverges. Each item must match the C reference
**bit-for-bit in IEEE-754**:

1. **Apodization windows** (`window.c`): coefficients via `cosf`/`fabsf` (single precision).
   `cosf` differs across libms by ±1 ULP → different windowed signal → different LPC.
   **Mitigation:** reproduce the exact float expressions and evaluation order; if Rust's
   `f32::cos` disagrees with the reference libm, precompute the window tables from the C
   reference and embed them, or implement the same polynomial. Verify per-coefficient.
2. **Autocorrelation** (`lpc.c:133`): accumulate in **`double`**, operands promoted from
   `f32`. Keep the exact loop/accumulation order; never use the SIMD `f32` path.
3. **Levinson-Durbin** (`lpc.c:176`): reflection coefficient `r /= err` in `double`. Early
   `if(err == 0.0)` exit. FPU rounding mode (round-to-nearest-even) must hold.
4. **LPC quantization** (`lpc.c:265`): `q = lround(error); error -= q;` with error feedback.
   `lround` tie behavior differs: **MSVC = half-away-from-zero, glibc = half-to-even**.
   Pick the semantics matching the libm the gold vectors are generated with, and implement
   that explicitly in Rust (don't rely on `f64::round`, which is half-away-from-zero).
5. **Auto qlp precision** (`lpc.c:~3983`): `min(15, 32 - subframe_bps - ilog2(order))`.
   Integer, but feeds quantization — get `ilog2` and bps (17 after mid-side) right.
6. **Rice partition search** (`stream_encoder.c:4089`): integer math, but tie-breaks in
   choosing partition order and parameter must match (first-best wins). `do_escape_coding`
   is **false** at level 8 → no raw-bit escape path.
7. **Mid-side decision** (`stream_encoder.c:3265`): with `loose_mid_side=false`, the
   per-frame channel-assignment choice (L/R vs M/S vs L/S vs R/S) is by estimated bits —
   replicate the estimate exactly or the chosen assignment flips.
8. **`f32` (`FLAC__real`) vs `f64`**: libFLAC stores LPC coeffs as `float` but computes
   autocorrelation in `double`. Match each variable's type precisely; a stray `f64` where C
   uses `f32` changes quantization.

## 7. Differential testing

Generate gold via MAME's `flac_encoder` (or libFLAC directly) at level 8, subset off,
blocksize 2048, 2ch/16-bit/44100, verify off; strip the leading metadata exactly as
`flac.cpp` does; compare frame bytes. Corpus: silence (→ CONSTANT subframes), full-scale
±32767, sine sweeps 1 Hz–20 kHz, decorrelated noise (exercises mid-side), and **real 16-bit
CD audio rips**. Compare in stages: frame header → subframe header → qlp_coeff integers →
residual/rice → CRC-16. Diff at the first mismatching field to localize float drift.

## 8. Public API

```rust
pub struct FlacEncoder { /* level-8 preset baked in */ }
impl FlacEncoder {
    /// 2ch/16-bit/44100, level 8, subset off, given block size — CHD's config.
    pub fn new_chd(block_size: u32) -> Self;
    /// Encode interleaved i16 stereo; returns raw frames (no STREAMINFO/seektable).
    pub fn encode_interleaved(&mut self, samples: &[i16]) -> Vec<u8>;
    pub fn finish(self) -> Vec<u8>;
}
```
chd-rs wraps this for both the raw `flac` codec (with the `'L'/'B'` endian byte + both-endian
trial) and the `cdfl` codec (audio→FLAC, subcode→deflate split).

## 9. Milestones

- **F0** Bitwriter + CRC-8/16; verify frame skeleton bytes.
- **F1** CONSTANT + VERBATIM + FIXED subframes (no float LPC); byte-exact on silence /
  simple signals.
- **F2** Windows + autocorrelation + Levinson + quantization; **the float-parity gate** —
  byte-exact LPC subframes on sine/noise.
- **F3** Rice partition search + mid-side; full corpus byte-exact.
- **F4** CD subcode split + endian-trial wrapper (or leave that to chd-rs); publish.

## 10. Risk / effort

**High.** The codec structure is well-defined and BSD-3, but **floating-point parity is the
real risk** — windows and `lround` tie-breaking can diverge from the reference libm and are
tedious to chase. Budget significant time for F2. ~5–7k LOC. Recommend porting **after**
`lzma-sdk-rs` so the differential-test rig already exists. If float parity proves
intractable on some platform, the fallback is embedding reference-computed window tables and
a fixed `lround` policy, validated against the gold libm.
