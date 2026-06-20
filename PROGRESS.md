# chd-rs write support — execution tracker

**Living document.** This is the task-level plan we tick off as we build. Strategy, decisions,
and rationale live in [PARITY_PLAN.md](PARITY_PLAN.md); codec-crate specs in
[docs/codec-ports/](docs/codec-ports/); the encode-layer design in
[docs/encode-integration.md](docs/encode-integration.md). This file tracks *what's done and
what's next*.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done & verified · `[!]` blocked/needs decision

---

## Current focus

**`chdman createraw` parity is COMPLETE & byte-identical to chdman 0.288** — uncompressed +
compressed (now **multi-codec**: zlib/huff/lzma/zstd + **flac**), **self-hunk dedup**, header,
`compress_v5_map`, and SHA-1, all verified end-to-end. All five bit-exact codec encoders are wired
(flac via `libflac-rs`, round-trip-verified through chdman; byte-identity is libm-gated — see below).

**Now:** **Phases A, B, C, and D are done.** A: `codec` module, `flac` encoder, multi-codec
`write_raw`, `CompressionProgress` + `progress`/`cancel`, public `hd` createraw + extract + geometry.
B: full **`createhd`** (metadata writer + overall SHA-1 + GDDD/IDNT). C: **`copy`** +
**`write_metadata`/`delete_metadata`** on existing CHDs. D: **`dvd`** (`createdvd`/`extractdvd`).
All byte-identical to chdman 0.288. Remaining: **E (`cd`)** — the big one (TOC parser, CD codecs,
ECC, CHT2); then F (parent/diff + `HdImage`), G (`info`/`verify`, rchdman).

**Verification oracle:** `C:\Tools\chdman\chdman.exe` (0.288, the parity target). See
"Verification gates" below. **43 write/compat tests green** (the 5 failing `read_*` tests are
pre-existing missing-fixture cases, unrelated to write). **CI** added (`.github/workflows/ci.yml`) and **verified green on GitHub Actions** (all 4 jobs:
lint + test on ubuntu/windows/macos): clones the sibling codec crates at their tags (they're public),
then `cargo build -p chd` + `cargo test -p chd --features write-zstd -- --skip tests::read` + fmt.
**`Chd::info()` + `ChdInfo`** (read-side) also landed (Phase G partial).

**CI:** chd-rs has **no CI** (siblings do). Blocked: the `write` feature's optional **path deps** on
the sibling crates (`../../lzma-sdk-rs`, …) make even the default build's dependency resolution fail
in a fresh checkout. Unblock = switch to crates.io **version deps** (all four siblings are
published/tagged) + a workspace `[patch.crates-io]` for local dev. Deferred pending the crate-identity
decision (may become a new crate).

---

## Dependencies (codec crates) — status

- [x] `lzma-sdk-rs` v0.2301.0 — byte-exact vs LZMA SDK 23.01 ✅ (aligned)
- [x] `libflac-rs` v0.143.0 — byte-exact vs libFLAC 1.4.3 ✅ (aligned; glibc-libm caveat). **Wired
  into chd-rs** (`write` dep) as the raw `flac` encoder (`RawFlacEncoder`, `compression/flac.rs`);
  `cdfl` + DVD default reuse it later. ⚠️ Byte-identity is **libm-gated**: validated vs glibc libm,
  but `C:\Tools\chdman` is a Windows/MSVC build → flac is verified **round-trip** (chd-rs encode →
  chdman `extractraw` reproduces the input), not byte-identity, against this chdman.
- [x] `libzstd-bitexact-rs` **v0.155.0** — byte-exact vs zstd **1.5.5** ✅ (aligned; published to
  crates.io 2026-06-19; chd-rs pins `=0.155` and gates `zstd`/`cdzs` behind `write-zstd`).
  ⚠️ chdman uses **unknown-pledged-size** level 22 → `StreamEncoder::new(22).finish(..)` (NOT
  `with_pledged_src_size`); `compress(x,22)` is byte-identical for a single `e_end`.
- [x] **`zlib-bitexact-rs` v0.131.0** — byte-exact vs stock zlib **1.3.1** ✅ (published to
  crates.io 2026-06-20; wired into chd-rs under `write`, encoder swapped in `compression/zlib.rs`;
  decoder stays on flate2). **`zlib_bit_exact_vs_chdman` passes** + full `-c zlib` CHD byte-identical
  to chdman. zlib was the HD default + every CD codec's deflate → now unblocked.

---

## M0 — Foundations

- [x] `write` feature in `chd-rs/chd-rs/Cargo.toml` (off by default; gates all encode code)
- [x] `CodecEncode` trait (`CompressionEncoder` + `CodecEncodeImplementation`) in `compression/mod.rs`
- [x] `CodecType::init_encoder(hunk_size)` dispatch (none/zlib/lzma/zstd) — `header.rs`
- [x] ~~`CodecsEncode` enum~~ — not needed; the driver calls `CodecType::init_encoder` directly.
- [x] `sha1` dep + SHA-1 generation: `raw_sha1`=SHA1(logical), `sha1`=SHA1(raw_sha1) when no
      metadata (ref `chd.cpp:1709` `compute_overall_sha1`). *(Metadata-checksummed overall SHA-1
      lands with the metadata writer, M4.)*
- [x] Round-trip test harness pattern (encode → existing decode → assert equal)
- [ ] `NOTICE` attribution for ported BSD-3 algorithms (Aaron Giles / MAME)

## M1 — V5 writer core + `none`/`zlib`

- [x] `NoneEncoder` (copy) + round-trip ✅
- [x] `ZlibEncoder` — backed by `zlib-bitexact-rs` 0.131 (was flate2/`zlib-rs`, which wasn't
      byte-exact); round-trip + reject-incompressible tests ✅
- [x] **zlib bit-exact** — resolved via `zlib-bitexact-rs` 0.131 (`zlib_bit_exact_vs_chdman`
      passes); the old `zlib-rs` deflate was ~2 bytes off stock zlib 1.3.1. See Dependencies.
- [x] V5 header writer (124-byte BE layout, verified against chdman) — `write.rs`
- [x] **Uncompressed CHD writer — byte-identical to `chdman createraw -c none`** ✅
      (`write_raw_uncompressed`; header + 4-byte map (entry = offset/hunk_bytes) + hunk-aligned
      data + last-hunk zero-pad; SHA-1 fields zero, as chdman leaves them for uncompressed).
      End-to-end test `raw_uncompressed_chd_bit_exact_vs_chdman` green (incl. partial last hunk).
      *Learned:* `createraw` requires `logical_bytes % unit_bytes == 0`.
- [x] **V5 compressed-map encoder `compress_v5_map` — byte-identical to chdman** ✅
      (`write.rs`; RLE + 16-code/8-bit Huffman via `export_tree_rle`, per-entry length/crc/self/
      parent bits, 16-byte header, CRC-16 reusing `block_hash::CRC16`). Isolation test
      `compress_v5_map_bit_exact_vs_chdman` green (rawmap reconstructed from a chdman CHD).
- [x] SHA-1 generation (`sha1` dep): `raw_sha1 = SHA1(logical)`, `sha1 = SHA1(raw_sha1)` (no
      metadata). For uncompressed createraw, SHA-1 fields are zero (chdman behavior).
- [x] Per-hunk driver (`write_raw_compressed`): codec-or-NONE (`complen < hunk_bytes`),
      byte-packed data, rawmap build, **+ self-hunk dedup** (a hunk byte-identical to an earlier
      written one → `COMPRESSION_SELF` ref, keyed by whole-hunk crc16+sha1, first wins — exactly
      `chd_file_compressor::compress_continue`). Now **fully general** for `createraw` (no-parent).
- [x] **Compressed CHD byte-identical to chdman** ✅ — `raw_compressed_zlib` (HD default),
      `raw_compressed_huff` (in-tree), and `raw_compressed_lzma_partial` (external crate + partial
      last hunk) all green. (Byte-identity ⇒ `chdman verify` passes.)
- [x] **self-hunk dedup** ✅ (`raw_compressed_dedup` e2e: byte-identical to chdman incl.
      consecutive/far/zero duplicates). Parent-hunk dedup deferred to M7 (needs parent CHDs).

## M2 — Codec encoders

- [x] `HuffmanEncoder` in-tree ✅ — faithful port of MAME `huffman.cpp` (`build_tree`,
      `compute_tree_from_histo`, `assign_canonical_codes`, `export_tree_huffman`) + MSB-first
      `BitWriter` (port of `bitstream_out`) in `huffman_encode.rs`; round-trip green. The
      `BitWriter` + `HuffEncoder<16,8>` are **reused by the V5 map writer** (M1).
- [x] `LzmaEncoder` via `lzma-sdk-rs` (`LzmaProps::chd_for_hunk` + `encode`) + round-trip ✅
      **(cross-crate integration proven)**
- [x] `RawFlacEncoder` via `libflac-rs` ✅ — `EncoderConfig::chd(blocksize)` + `encode_frames` with
      the `'L'`/`'B'` both-endian trial (ties → `'L'`), `blocksize = bytes/4 halved while >2048`,
      and MAME's `hunkbytes-1` overflow guard (`compression/flac.rs`). Round-trip green + chdman
      `extractraw` of our flac CHD reproduces the input.
- [x] `ZstdEncoder` via `libzstd-bitexact-rs` 0.155 (`StreamEncoder::new(22).finish`) — gated
      `write-zstd`; round-trip ✅
- [x] Per-codec round-trip (none/zlib/lzma/zstd/huff/flac) ✅
- [x] **Bit-exact vs chdman 0.288** (`chdman_compat` tests, `createraw -c <codec>`, compare our
      encoder's bytes to chdman's stored compressed bytes per hunk):
  - [x] **zlib** ✅ · **huff** ✅ · **lzma** ✅ · **zstd** ✅ — all byte-identical to chdman 0.288
  - [x] **multi-codec** ✅ — `-c lzma,zlib` (per-hunk `find_best_compressor`) byte-identical
  - [~] **flac** — **round-trip** verified (libm-gated byte-identity; this chdman is MSVC, not glibc)

## M3 — `hd` module + `codec` module

> **API-parity handoff:** [docs/libchdman-parity.md](docs/libchdman-parity.md) maps every
> libchdman-rs public item → chd-rs equivalent + the phased roadmap (A–G) to full parity.

- [x] `codec` module ✅ — `CHD_CODEC_*` consts, `parse_codec_spec` / `codec_name` / `codec_exists`
      (`src/codec.rs`, re-exported at crate root, matches libchdman-rs; unconditional, no `write`
      dep). 3 unit tests + doctest green.
- [x] `compute_chs` ✅ — faithful port of `guess_chs` (`chdman.cpp:1115`); verified to reproduce
      chdman's chosen geometry (`hd.rs`).
- [x] `format_gddd` / `read_geometry` ✅ — `"CYLS:%d,HEADS:%d,SECS:%d,BPS:%d"` + NUL; `read_geometry`
      parses chdman's `createhd` `GDDD` record.
- [x] `HdCreateOptions` + `create_raw_from_path`/`create_raw_from_reader` + `extract_to_*` ✅
      (`hd.rs`; createraw byte-identical, multi-codec, `progress`/`cancel`; extract via the existing
      decoder).
- [x] **Full `createhd`** ✅ — `create_from_reader`/`create_from_path` write GDDD (+ optional IDNT)
      via the metadata writer; byte-identical to `chdman createhd` (`-c none`/`zlib`/`lzma` + IDNT).
- [x] Tests ✅ — compute_chs vs chdman, GDDD format/parse round-trip, `create_raw` byte-identical +
      callbacks + cancel, extract round-trip (partial last hunk), `read_geometry` vs chdman,
      **createhd byte-identical** (none/zlib/lzma + GDDD/IDNT 2-entry list).

## M4 — Metadata write/delete + `copy`

- [x] Metadata linked-list **writer for new files** ✅ — `write::build_metadata_blob` +
      `compute_overall_sha1` (port of `chd.cpp:1709`: `SHA1(raw_sha1 ‖ sorted[tag(4)‖SHA1(payload)])`),
      wired into `write_raw_inner` (metadata before hunks) and `write_uncompressed_inner` (after map).
      Verified via createhd byte-identity. Layout learned: compressed = header→meta→hunks→map
      (`meta_offset=124`); uncompressed = header→map→meta→pad→data (`meta_offset=124+mapsize`,
      SHA-1 zero).
- [x] `write_metadata` / `delete_metadata` on **existing** CHDs ✅ (`metadata.rs`, gated `write`) —
      free fns over `Read+Write+Seek`; overwrite-in-place-or-append + relink + overall-SHA-1 update
      (compressed only). Byte-identical to `chdman addmeta`/`delmeta` (which only edit *uncompressed*
      CHDs — MAME refuses a writeable open of a compressed one; chd-rs additionally handles
      compressed correctly). Ports `metadata_find`/`metadata_set_previous_next`/`metadata_update_hash`.
- [x] `CopyOptions` + `copy` ✅ (`copy.rs`) — recompress source logical bytes (preserve
      unit_bytes; default hunk = source's), clone all metadata in source order with flags. Byte-
      identical to `chdman copy`.
- [x] Tests ✅ — HD codec change (none↔zlib, lzma→zlib) byte-identical. (DVD/CD copy come with D/E.)

## M5 — `dvd` module

- [x] `DvdCreateOptions` (hunk 4096, codecs `[lzma,zlib,huff,flac]`, unit 2048) ✅
- [x] `create_from_reader`/`create_from_iso` + `extract_to_writer`/`extract_to_iso` ✅ — empty
      `DVD ` record (the 1-NUL-byte quirk: chdman's `write_metadata(.., "")` stores the string's NUL
      terminator → 1-byte payload). Reuses the createhd machinery (`DVD ` instead of GDDD).
- [x] Tests ✅ — `createdvd -c none/zlib/lzma` byte-identical to chdman; extract round-trip
      (partial last hunk) + non-DVD rejection.

## M6 — `cd` module

- [ ] CUE/GDI/ISO/Nero TOC parser (pure Rust)
- [ ] CD wrapper encoders `CdEncoder<E,S>` + `CdFlacEncoder` (split 2048+96, ECC strip via
      `ecc.rs`, header) — ref `cdrom.rs` decode + `ecc.rs` `generate_ecc`
- [ ] CHT2 metadata; `TrackInfo`/`TrackType`/`SubcodeType`; `list_tracks`
- [ ] `create_from_cue`/`create_from_iso`; `extract_to_cue`/`extract_to_iso`/`extract_to_gdi`
- [ ] Tests: BIN/CUE round-trip, codec matrix, multi-track, GDI

## M7 — Parent/diff + `HdImage`

- [ ] Uncompressed diff children vs compressed parent; parent SHA-1 linkage
- [ ] `HdImage` block device: `read_sector`/`write_sector`, `open_with_diff`/`reopen_diff`

## M8 — rchdman + docs

- [ ] rchdman: `createhd`/`createcd`/`createdvd`/`copy`/`addmeta`/`delmeta`
- [ ] Port `docs/format-modules.md` + `docs/chdman-mapping.md`; README rewrite

---

## Verification gates (apply continuously)

- **Byte-for-byte vs chdman 0.288** (the bit-for-bit goal) — `chdman_compat_tests` (dev-local):
  create the same CHD/codec with chdman and assert the files (or per-hunk codec bytes) are
  identical. **Implemented**: per-codec bit-exact (zlib/huff/lzma/zstd), `compress_v5_map`, and the
  full createraw writer (uncompressed/compressed/dedup) — 16 tests. Byte-identity ⇒ `chdman verify`
  passes for free.
- Round-trip (encode → chd-rs decode → equal logical bytes) as a fast local gate without chdman.

### Testing oracle — bit-exact verification strategy

Three layers, by who provides the reference:

1. **Codec crates (lzma/flac/zstd)** — already self-verified byte-exact vs their bundled C
   oracle. Nothing to do in chd-rs beyond the round-trip integration tests (done for
   lzma/zstd).
2. **In-tree codec (huff)** — chd-rs's own bytes, verified **directly against chdman 0.288** via
   the `chdman_compat` tests (byte-identical per hunk). (zlib started in-tree but became its own
   crate, `zlib-bitexact-rs`, with a vendored-zlib-1.3.1 `cref` oracle.)
3. **CHD container + end-to-end (header/map/metadata/SHA-1/codec-selection)** — needs a
   **chdman reference**. ✅ **Oracle available: `C:\Tools\chdman\chdman.exe` (chdman 0.288,
   mame0288)** — exactly the parity target. The `chdman_compat` test module (in-crate, gated
   `chdman_compat_tests`) shells out to it. Codec-level bit-exact tests are live (M2) **and the
   end-to-end container tests are done** (M1: uncompressed/compressed/dedup all byte-identical).
   ⚠️ FLAC bit-exactness is libm-dependent — confirm the reference chdman is a **glibc** build to
   match `libflac-rs` (this chdman is the Windows 0.288 build; revisit for flac).

## Open decisions / waiting

- ~~zlib bit-exact deflate~~ — **RESOLVED** (`zlib-bitexact-rs` 0.131, wired + bit-exact).
- FLAC float parity is validated vs **glibc** libm; the reference chdman here is the Windows
  0.288 build — revisit when wiring `libflac-rs` (`flac`/`cdfl`).

## API parity (libchdman-rs)

Tracked in detail in [docs/libchdman-parity.md](docs/libchdman-parity.md). Phases:
- [x] **A** — public write surface + `codec` module + flac. ✅ `codec`, `flac` encoder, multi-codec
  `write_raw`, `CompressionProgress` + `progress`/`cancel`, public `hd` createraw create/extract +
  geometry helpers, and `docs/libchdman-differences.md` all landed. (createhd-with-GDDD is the only
  hd item carried into B, since it needs the metadata writer.)
- [x] **B** — `hd` createhd ✅ — metadata writer (new files) + overall SHA-1 + `create_from_*`
  writing GDDD (+ optional IDNT), byte-identical to chdman.
- [x] **C** — `copy` ✅ + `write_metadata`/`delete_metadata` on existing CHDs ✅ — all byte-identical.
- [x] **D** — `dvd` ✅ — `createdvd`/`extractdvd` byte-identical (`DVD ` record, 2048 sectors).
- [ ] **E** — `cd` · [ ] **F** — parent/diff + `HdImage` · [~] **G** — `Chd::info` ✅ + `ChdInfo`;
  `verify` (needs a read-side SHA-1 dep) + rchdman + remaining docs pending.

## Session log

- 2026-06-20: **Phase C completed (metadata write/delete) + CI added.** (1) `metadata::write_metadata`
  /`delete_metadata` for existing V5 CHDs (`Read+Write+Seek` free fns): ports of
  `metadata_find`/`metadata_set_previous_next`/`metadata_update_hash` — overwrite-in-place-or-append
  + relink, overall-SHA-1 recompute on write (compressed only). **Byte-identical to `chdman
  addmeta`/`delmeta`.** Learned chdman only edits *uncompressed* CHDs (MAME refuses a writeable open
  of a compressed one → "File not writeable"); chd-rs's fns also handle compressed correctly. (2)
  **CI** (`.github/workflows/ci.yml`, user chose the "clone siblings" approach): clones the four
  sibling codec crates at their tags into the checkout's parent (so `../../<crate>` path deps
  resolve), then `cargo build -p chd` + `cargo test -p chd --features write-zstd -- --skip
  tests::read` on ubuntu/windows/macos, plus a `cargo fmt -p chd --check` lint job. (The 3
  crate-level doctests were already fixed; clippy isn't `-D warnings` yet — legacy lints.) 42
  write/compat tests green.
- 2026-06-20: **Phase D `dvd` landed — byte-identical to `chdman createdvd`.** New `dvd` module
  (`DvdCreateOptions`, `create_from_reader`/`create_from_iso`, `extract_to_writer`/`extract_to_iso`,
  `DVD_SECTOR_SIZE`/`DEFAULT_HUNK_SIZE`). It's createhd with a `DVD ` record (1-NUL payload — chdman
  writes an empty C string, storing the NUL terminator) instead of GDDD, unit 2048. Hoisted the
  shared create dispatch (`read_and_pad` + `write_create`) into `write.rs` so hd/copy/dvd reuse it.
  Verified byte-identical for `-c none/zlib/lzma` + extract round-trip / non-DVD rejection. 40
  write/compat tests green. **Also investigated CI:** chd-rs has none; it's blocked by the write
  feature's sibling path deps (even default resolve fails without them) — see the CI note above.
- 2026-06-20: **Phase C `copy` landed — byte-identical to `chdman copy`.** New `copy` module
  (`copy::copy` + `CopyOptions{hunk_size:Option<u32>, codecs}`, matching libchdman-rs): open the
  source, snapshot its metadata in linked-list order, read the full logical image via `ChdReader`
  (truncated past last-hunk padding), then recompress with `write_raw_inner`/`write_uncompressed_inner`
  cloning the metadata verbatim (tag/flags/payload) — **unit_bytes preserved**, default hunk = the
  source's. Factored `resolve_codecs` into `write.rs` (shared by `hd`+`copy`). Verified byte-identical
  to `chdman copy` for none→zlib (uncompressed→compressed + GDDD clone + overall SHA-1), zlib→none
  (→uncompressed), and lzma→zlib. CD/GD legacy-metadata re-do is deferred to Phase E. **36
  write/compat tests green.**
- 2026-06-20: **Phase B landed — full `createhd`, byte-identical to chdman.** (1) **Metadata
  writer for new files** (`write.rs`): `MetaEntry` + `build_metadata_blob` (the linked list —
  `tag(4)+flags(1)+len(3)+next(8)+payload`, `next` chaining each entry, the header's `meta_offset`
  pointing at the first). (2) **Overall SHA-1** `compute_overall_sha1` — port of `chd.cpp:1709`:
  `SHA1(raw_sha1 ‖ sorted[ tag(4 BE) ‖ SHA1(payload) ])` over the CHECKSUM-flagged entries, sorted
  by the 24-byte `(tag,sha1)` memcmp. (3) Wired both into the writers via new `pub(crate)`
  `write_raw_inner`(+`metadata`) and `write_uncompressed_inner`(+`metadata`); **layout learned
  empirically**: compressed createhd = header(124)→metadata(@124)→hunks→map; uncompressed =
  header→map→metadata→pad→data (`meta_offset=124+mapsize`, SHA-1 fields stay zero). Existing
  createraw byte-identity preserved (empty-metadata path is unchanged). (4) **`hd::create_from_reader`
  /`create_from_path`** (`createhd`): derive geometry (or use `opts.geometry`), write GDDD (+ optional
  IDNT), `progress`/`cancel`; refactored the create body into shared `read_and_pad` + `write_create`
  helpers. **Byte-identical to chdman `createhd`** verified for `-c none` (uncompressed), `-c zlib`
  and `-c lzma` (compressed → overall SHA-1), and `--ident` (GDDD+IDNT 2-entry list + sorted SHA-1).
  **33 write/compat tests green**; read-only build clean.
- 2026-06-20: **Phase A landed (flac + multi-codec + public `hd` surface).** (1) **Wired
  `libflac-rs`** as `RawFlacEncoder` (`compression/flac.rs`): `EncoderConfig::chd(blocksize)` +
  `encode_frames` with the `'L'`/`'B'` both-endian trial (ties → `'L'`), `blocksize=bytes/4 halved
  while >2048`, MAME's `hunkbytes-1` overflow guard; registered in `init_encoder` + codecs exports;
  added to the `write` feature. Round-trip green and **chdman `extractraw` reproduces our flac CHD**
  (byte-identity is libm-gated — this chdman is MSVC, libflac-rs is glibc-validated). (2)
  **Multi-codec `write_raw`** — generalized the single-codec writer to a per-hunk
  `find_best_compressor` (slot order, strictly-smaller wins, ties → earlier slot), header
  `compression[0..4]` per slot; `write_raw_compressed` is now `write_raw(.., &[codec])`. `-c
  lzma,zlib` is **byte-identical to chdman**. Made `CodecType: Copy`. (3) **`CompressionProgress`**
  `{bytes_done,bytes_total,ratio}` at the crate root + the `progress: &mut dyn FnMut(..)` /
  `cancel: &dyn Fn()->bool` convention; threaded through `write_raw_inner` (cancel before each hunk
  → `Error::Cancelled` *before any bytes hit `out`*). (4) **Public `hd` module** — `HdGeometry`,
  `HdCreateOptions`, `compute_chs` (port of `guess_chs`, verified vs chdman geometry), `format_gddd`
  /`read_geometry`, and `create_raw_from_reader`/`_path` + `extract_to_writer`/`_path`. createraw is
  byte-identical; extract truncates past the last hunk's padding. createhd-with-GDDD deferred to
  Phase B (needs the metadata writer). (5) Added `docs/libchdman-differences.md`. **29 write/compat
  tests green**; read-only build clean.
- 2026-06-18: Plan consolidated into this tracker. Codec crates built (lzma/flac aligned, zstd
  needs 1.5.5 retarget). Integration specced (`docs/encode-integration.md`). Starting M0.
- 2026-06-18: M0/M1/M2 foundation landed. Added `write` feature, `CompressionEncoder` /
  `CodecEncodeImplementation` traits, `CodecType::init_encoder` dispatch, and `none`/`zlib`/
  `lzma` encoders. All three round-trip through the existing decoders (3 tests green); the
  lzma path wires the `lzma-sdk-rs` sibling crate (path dep, `write` feature) and proves the
  cross-crate integration end-to-end. Read-only (no-`write`) build unaffected. Remaining
  warnings in the `write` build are expected "never used" on the encoders until the M1 driver
  calls them.
- 2026-06-19: zstd drift **resolved** — `libzstd-bitexact-rs` 0.155 (byte-exact vs zstd 1.5.5)
  published. Wired `ZstdEncoder` behind a new `write-zstd` feature using
  `StreamEncoder::new(22).finish(..)` (chdman's unknown-pledged-size level-22 path); round-trip
  green (the windowLog-27/LDM frame decodes through chd-rs's ruzstd decoder). 4 encode tests
  pass. Four codecs done: none/zlib/lzma/zstd.
- 2026-06-19: `HuffmanEncoder` ported in-tree (`huffman_encode.rs`): faithful MAME
  `huffman.cpp` encode path + MSB-first `BitWriter` (port of `bitstream_out`). Round-trip green.
  Five codecs done: none/zlib/lzma/zstd/huff. Added the testing-oracle strategy (above): codec
  crates self-verify; zlib/huff can self-verify via a vendored C oracle; the CHD container needs
  a chdman reference (chdman not on PATH — needs a binary or a libchdman-rs build).
- 2026-06-19: **Oracle wired** — `C:\Tools\chdman\chdman.exe` (chdman **0.288**, the parity
  target). Built the `chdman_compat` test module (`createraw -c <codec>` → compare our encoder's
  bytes to chdman's stored compressed bytes per hunk). **Result: huff ✅, lzma ✅, zstd ✅ are
  byte-identical to chdman 0.288.** **zlib ✗** — `zlib-rs` 0.4.2 deflate ≠ stock zlib 1.3.1
  (2 bytes smaller); test `#[ignore]`-d pending a bit-exact deflate. zlib is the HD default →
  **DECISION NEEDED: port stock-zlib-1.3.1 deflate (4th codec crate) vs link C zlib.** Next:
  V5 writer core, verified end-to-end against chdman using a confirmed-bit-exact codec.
- 2026-06-19: **First end-to-end bit-exact writer** — `write_raw_uncompressed` (`write.rs`)
  produces a complete uncompressed CHD **byte-identical to `chdman createraw -c none`** (header,
  4-byte map, hunk-aligned data, partial-last-hunk pad; SHA-1 fields zero as chdman leaves them).
  Decoded the V5 layout empirically from chdman output. Learned `createraw` needs
  `logical_bytes % unit_bytes == 0`. Next: the compressed writer core (`compress_v5_map` + SHA-1
  + per-hunk driver).
- 2026-06-20: **libchdman-rs API-parity handoff written** (`docs/libchdman-parity.md`) — the full
  API map (every public item → chd-rs equivalent, marked exists/differs/done/to-build), the
  `Chd<F>`-reader vs owned-handle reconciliation policy (keep chd-rs idioms, add parity surface as
  free functions, document divergences), and the phased roadmap A–G. Started Phase A: shipped the
  `codec` module (`CHD_CODEC_*`, `parse_codec_spec`/`codec_name`/`codec_exists`) matching
  libchdman-rs, re-exported at the crate root. Remaining Phase A: flac wiring + public hd
  create/extract + `CompressionProgress`.
- 2026-06-20: **Self-hunk dedup — DONE.** `write_raw_compressed` now reproduces
  `chd_file_compressor::compress_continue`: per-hunk it computes the whole-hunk crc16+sha1, and a
  hunk byte-identical to an earlier *written* hunk becomes a `COMPRESSION_SELF` ref
  (`type=5, complen=0, offset=refhunk, crc=0`) instead of being re-stored (first occurrence wins;
  SELF hunks aren't re-added). `compress_v5_map`'s SELF_0/SELF_1 promotions handle the encoding.
  `raw_compressed_dedup` e2e (10 hunks w/ consecutive, far, and zero duplicates) is byte-identical
  to chdman. **`createraw` is now fully general.** 16 write/compat tests green.
- 2026-06-20: **zlib bit-exact — DONE.** `zlib-bitexact-rs` 0.131.0 (byte-exact stock zlib 1.3.1)
  published + wired into chd-rs (`write` dep; encoder swapped in `compression/zlib.rs`, decoder
  stays flate2). Un-ignored `zlib_bit_exact_vs_chdman` → passes; added `raw_compressed_zlib` e2e →
  full `-c zlib` CHD byte-identical to chdman. **All four primary codecs (zlib/huff/lzma/zstd) now
  byte-identical to chdman 0.288**, and the HD default is unblocked. 15 write/compat tests green.
  Integration handoff: `docs/codec-ports/zlib-bitexact-integration.md`.
- 2026-06-19: **Compressed V5 writer core DONE & byte-identical to chdman 0.288.** Ported
  `compress_v5_map` (verified byte-exact in isolation against chdman's stored map), added SHA-1
  (`raw_sha1`=SHA1(logical), `sha1`=SHA1(raw_sha1)), and the per-hunk driver
  (`write_raw_compressed`: codec-or-NONE, byte-packed data, compressed map at EOF). Full
  end-to-end CHDs match chdman byte-for-byte: `-c huff` (in-tree codec) and `-c lzma` (external
  crate + partial last hunk). Compressed layout: header(124) → byte-packed hunks → compressed
  map at `map_offset`. 13 write/compat tests green (5 pre-existing read fixture failures
  unrelated). Remaining: self/parent dedup, public API wrapper, flac wiring, format modules.
