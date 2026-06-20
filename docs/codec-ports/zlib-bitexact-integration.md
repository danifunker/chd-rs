# Integrating `zlib-bitexact-rs` into chd-rs (return handoff)

> ✅ **The port is DONE.** `zlib-bitexact-rs` **0.131.0** is published to crates.io and is
> byte-identical to stock **zlib 1.3.1** `deflate` at the exact CHD settings. This is the *return*
> handoff — wire the crate into chd-rs and un-ignore the zlib parity test. The original outgoing
> spec is [`zlib-bitexact.md`](zlib-bitexact.md).

## What you're integrating

- **Crate:** `zlib-bitexact-rs` v0.131.0 (crates.io) — pure-Rust, **zero runtime deps**,
  `#![forbid(unsafe_code)]`. Encode-only, raw DEFLATE, one configuration.
- **API (the entire surface):**
  ```rust
  /// Raw DEFLATE (no zlib header, no adler32 trailer), byte-identical to zlib 1.3.1
  /// deflateInit2(9, Z_DEFLATED, -15, 8, Z_DEFAULT_STRATEGY) + a single deflate(Z_FINISH).
  pub fn deflate_raw(input: &[u8]) -> Vec<u8>;
  ```
- **Proof:** byte-for-byte vs the vendored zlib 1.3.1 C oracle across a broad corpus —
  tiny/boundary sizes, long runs, repeated text, incompressible random (stored blocks), skewed /
  geometric / Fibonacci distributions (the bit-length-overflow path), multi-block (>16383 symbols),
  window-sliding (>64 KiB), and CHD hunk sizes (4096 / 2448-multiples / 19584). CI green.

## Precondition — ✅ already verified

chdman/MAME **0.288 bundles zlib 1.3.1** (`ZLIB_VERSION "1.3.1"` in
`mame/3rdparty/zlib/zlib.h` and `libchdman-rs/deps/mame/3rdparty/zlib/zlib.h`), which is exactly
what this crate targets. So the bytes should match. If a future chdman bumps its bundled zlib, the
crate's minor version must be retargeted (the version encodes the upstream: `0.131.x` ⇒ zlib 1.3.1).

## Integration steps

### 1. Add the dependency — `chd-rs/chd-rs/Cargo.toml`

Match the sibling pattern (optional, gated by `write`):

```toml
[dependencies]
# published; byte-exact with stock zlib 1.3.1 (the version chdman 0.288 bundles)
zlib-bitexact-rs = { version = "0.131", optional = true }
# local-dev alternative against the sibling checkout:
# zlib-bitexact-rs = { path = "../../zlib-bitexact-rs", optional = true }
```

Add it to the base `write` feature (zlib is the **HD default** *and* underlies every **CD** codec,
so it belongs alongside lzma, not behind an optional sub-feature):

```toml
write = ["std", "dep:lzma-sdk-rs", "dep:zlib-bitexact-rs", "dep:sha1"]
```

### 2. Swap the encoder — `chd-rs/chd-rs/src/compression/zlib.rs`

Replace the `flate2::Compress`-backed `ZlibEncoder` with `deflate_raw`. **Leave the decoder
(`ZlibCodec`, flate2 inflate) untouched** — inflate is unambiguous, so flate2/zlib-rs stays for
reads.

```rust
#[cfg(feature = "write")]
pub struct ZlibEncoder; // no engine state needed

#[cfg(feature = "write")]
impl crate::compression::CodecEncodeImplementation for ZlibEncoder {
    fn new(_: u32) -> Result<Self> {
        Ok(ZlibEncoder)
    }

    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let compressed = zlib_bitexact_rs::deflate_raw(input);
        // Codec "loses" if the raw DEFLATE stream doesn't fit the hunk-sized output buffer;
        // the driver then stores the hunk uncompressed (NONE). Mirrors the old flate2 path.
        if compressed.len() > output.len() {
            return Err(Error::CompressionError);
        }
        output[..compressed.len()].copy_from_slice(&compressed);
        Ok(compressed.len())
    }
}
```

Notes:
- `deflate_raw` always consumes the whole input, so the old `total_in == input.len()` check is
  implicit; the only failure mode is "doesn't fit", i.e. `compressed.len() > output.len()`.
- The existing `#[cfg(all(test, feature = "write"))]` round-trip tests in this file still pass
  (decode via flate2). They're a fast local sanity check that doesn't need chdman.

### 3. Un-ignore the parity test — `chd-rs/chd-rs/src/chdman_compat.rs`

Drop the `#[ignore = "…"]` on `zlib_bit_exact_vs_chdman` (~line 142):

```rust
#[test]
fn zlib_bit_exact_vs_chdman() {
    assert_codec_bit_exact("zlib", CodecType::ZLibV5, 4096, 512);
}
```

### 4. Run it (needs a chdman 0.288 binary)

```sh
# chdman resolved via $CHDMAN, then C:\Tools\chdman\chdman.exe, then `chdman` on PATH
cargo test -p chd --features "write chdman_compat_tests" zlib_bit_exact_vs_chdman
```

The harness has chdman build a single-codec raw CHD (`createraw -c zlib`), then for every hunk
chdman compressed with zlib it decodes the raw data, re-encodes it with `ZlibEncoder`, and asserts
the stored bytes are identical. (Use `--features "write-zstd chdman_compat_tests"` to run the whole
compat suite, including the already-passing `huff`/`lzma`/`zstd` codecs.)

### 5. CD codecs — `cdzl` / `cdlz` / `cdfl`

All three CD codecs deflate via zlib: `cdzl` deflates the **sector + subcode** streams; `cdlz` and
`cdfl` deflate the **subcode** sub-stream. Once their *encoders* route those deflate sub-streams
through `deflate_raw` (reuse `ZlibEncoder`, or call `deflate_raw` directly), they become bit-exact
too. `compression/cdrom.rs` today holds the CD *decoders*; the CD *encode* path is part of the
writer WIP, so wire `deflate_raw` in there as those encoders land and add the analogous `cd*`
compat tests.

## Done =

- `zlib_bit_exact_vs_chdman` passes against chdman 0.288 (no `#[ignore]`).
- HD CHDs (`-c zlib`) and CD CHDs (`-c cdlz,cdzl,cdfl`) written by chd-rs are byte-identical to
  chdman 0.288.
- Mark zlib ✅ in [`README.md`](README.md), `PROGRESS.md`, and `PARITY_PLAN.md`.

## Gotchas

- **Encode-only by design.** Keep flate2/zlib-rs for decode; only the encoder swaps.
- **Allocation per hunk.** `deflate_raw` returns a `Vec<u8>`. Fine for correctness; if the hot path
  needs it later, add a buffer-reusing entry point to the crate (e.g. `deflate_raw_into(&mut Vec)`).
- **"Lose" semantics.** If a hunk's deflate output ≥ the hunk size, the encoder must return `Err`
  so the driver stores NONE — preserved by the `> output.len()` check above.
- **Repo/links:** crate <https://crates.io/crates/zlib-bitexact-rs>,
  source <https://github.com/danifunker/zlib-bitexact-rs>, docs <https://docs.rs/zlib-bitexact-rs>.
