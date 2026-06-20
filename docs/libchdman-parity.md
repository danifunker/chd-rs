# libchdman-rs API parity & completion handoff

**Purpose.** Bring chd-rs to **full functional parity** with `../libchdman-rs`'s public API, while
**keeping chd-rs's existing (read-side) API** where it makes sense and **documenting every
divergence**. This doc is the spec for that task *and* the roadmap for the rest of the write-support
work. Strategy lives in [PARITY_PLAN.md](../PARITY_PLAN.md); the live checklist is
[PROGRESS.md](../PROGRESS.md); the codec encode design is in [encode-integration.md](encode-integration.md).

Read this with the libchdman-rs surface in [docs/format-modules.md](../../libchdman-rs/docs/format-modules.md)
and [docs/chdman-mapping.md](../../libchdman-rs/docs/chdman-mapping.md).

---

## 1. Where we are (done) vs. what's left

**Done & byte-identical to chdman 0.288:**
- Codec encoders: `none`/`zlib`/`huff`/`lzma`/`zstd` byte-identical; **`flac`** wired
  (`RawFlacEncoder`) and **round-trip-verified** (byte-identity libm-gated — see §6).
- V5 writer core (`mod write`): `write_raw_uncompressed`, `write_raw_compressed`, and the
  **multi-codec `write_raw`** (per-hunk `find_best_compressor`) — header, `compress_v5_map`, SHA-1,
  per-hunk codec/none driver, **self-hunk dedup**. Full `chdman createraw` parity (no-parent),
  single- and multi-codec.
- **`codec` module** (public): `CHD_CODEC_*`, `parse_codec_spec`/`codec_name`/`codec_exists`.
- **`CompressionProgress`** (crate root) + the `progress`/`cancel` callback convention.
- **`hd` module** (public): `HdGeometry`, `HdCreateOptions`, `compute_chs`, `format_gddd`,
  `read_geometry`, `create_raw_from_reader`/`_path` (createraw), `extract_to_writer`/`_path`, and
  **`create_from_reader`/`_path` (full `createhd`** — GDDD + optional IDNT).
- **Metadata writer for new files** (`write.rs`, crate-internal): `build_metadata_blob` +
  `compute_overall_sha1`, wired into the compressed and uncompressed writers.

- **`copy` module** (public): `copy::copy` + `CopyOptions`, byte-identical to `chdman copy`.
- **`dvd` module** (public): `dvd::create_from_*`/`extract_to_*` + `DvdCreateOptions`, byte-identical
  to `chdman createdvd`.

**Left:** `createcd` (the CD create path); metadata write/delete on **existing** CHDs; `cd`;
`info`/`verify`; parent/diff + runtime writes; remaining docs. Everything below.

---

## 2. The core difference — and the reconciliation policy

This is the divergence to document front and center (decision **D3**, confirmed):

| | libchdman-rs | chd-rs |
| --- | --- | --- |
| `Chd` shape | **owned handle** over a path/`ChdIo`, with a `writeable` flag; `read_hunk`/`write_hunk`/`read_bytes`/`write_bytes`/`read_metadata`/… are **methods on it** | **generic, borrowed reader** `Chd<F: Read + Seek>` — read-only; accessors via `header()`/`map()`/`metadata()`; hunk reads via `hunk().read_hunk_in()` + `read::` adapters |
| Mutability | one handle reads *and* writes at runtime | reads only; creation is via free functions; runtime block writes via a future `HdImage` |
| Custom I/O | `ChdIo: Read + Write + Seek` trait | any `Read + Seek` — generic, no trait needed |
| Compression | async `ChdCompressor` + `ChdDataHandler` pull model | synchronous create functions with `progress`/`cancel` callbacks |

**Policy:** *keep chd-rs's idioms; add the parity surface as free functions/modules; document the
gaps.*
1. **Keep** `Chd<F: Read + Seek>` and its read API as-is (it's nicer — zero-copy, generic, no FFI).
2. **Add** the create/extract/copy functionality as libchdman-rs-**named free functions** in new
   `hd`/`cd`/`dvd`/`copy` modules (this is where chd-rs has nothing today).
3. **Add** the small value types/constants libchdman-rs exposes (`CHD_CODEC_*`, `parse_codec_spec`,
   `CompressionProgress`, `ChdInfo`, options structs) — names matched exactly.
4. **Adapt, don't clone, the `Chd` handle.** Do *not* graft libchdman-rs's owned mutable `Chd`
   onto chd-rs. Where libchdman-rs uses `chd.read_hunk(n, buf)`, chd-rs users write
   `chd.hunk(n)?.read_hunk_in(&mut tmp, buf)`. Provide thin convenience methods only where they
   clearly help (e.g. `Chd::info()`), and document the mapping for everything else.
5. **Every divergence goes in `docs/libchdman-differences.md`** (see §5) so a libchdman-rs user can
   port mechanically.

---

## 3. Full API map (libchdman-rs → chd-rs)

`status`: ✅ exists · 🟡 exists but different shape (document it) · 🟢 done in `write` (needs public
wrapper) · ⬜ to build.

### 3.1 Top-level (`lib.rs`)

| libchdman-rs | chd-rs target | status | notes / difference |
| --- | --- | --- | --- |
| `Chd::open(path, writeable, parent)` | `Chd::open(reader, parent)` | 🟡 | takes `Read+Seek`, not a path; **no `writeable`** (reads only). Path helper: `Chd::open(BufReader::new(File::open(p)?), None)`. |
| `Chd::open_custom(io, …)` | `Chd::open(io, …)` | ✅ | any `Read+Seek` is "custom I/O"; `ChdIo` trait unneeded. |
| `Chd::create(file, lb, hb, ub, comp)` | `hd::create_*` / `write::write_raw_compressed` | 🟢/⬜ | chd-rs creates via the **format modules**, not a generic `Chd::create`. |
| `Chd::create_with_parent(…)` | parent/diff (§4 Phase F) | ⬜ | |
| `version/hunk_bytes/hunk_count/unit_bytes/unit_count/logical_bytes` | `chd.header().{hunk_count,hunk_size,unit_bytes,…}()` | 🟡 | accessor lives on `header()`. Optional: add passthroughs on `Chd`. |
| `sha1/raw_sha1/parent_sha1` | `chd.header().{sha1,raw_sha1,parent_sha1}()` | 🟡 | same. |
| `hunk_info(n) -> HunkInfo{compressor,compbytes}` | `chd.map().get_entry(n)` → `MapEntry` | 🟡 | map entry exposes `hunk_type()`+`block_size()`. Optional `HunkInfo` convenience. |
| `read_hunk(n, buf)` | `chd.hunk(n)?.read_hunk_in(&mut tmp, buf)` | 🟡 | needs a scratch buffer; document. |
| `read_bytes(off, buf)` | `read::ChdReader` (`Read+Seek`) → `read_exact` | 🟡 | |
| `write_hunk/write_bytes` | `hd::HdImage` (Phase F) | ⬜ | runtime writes to uncompressed CHDs only. |
| `read_metadata(tag, index)` | `chd.metadata()` / `metadata_refs()` filter | 🟡 | optional `Chd::read_metadata(tag,index)` convenience. |
| `write_metadata/delete_metadata` | `metadata::write_metadata`/`delete_metadata` | ✅ | done & byte-identical (free fns over `Read+Write+Seek`, not methods on `Chd`). chdman edits only uncompressed CHDs; chd-rs also handles compressed. |
| `clone_all_metadata(src)` | `copy` module (clones all metadata) | ✅ | done inside `copy::copy`. |
| `info() -> ChdInfo` | `ChdInfo` + `Chd::info()` | ✅ | done; header + metadata tags + track count + `is_hd/cd/gd/dvd/av` (read-side). |
| `verify()` | new `Chd::verify()` | ⬜ | deferred — needs a SHA-1 dep in the read-only build. |
| `make_tag(a,b,c,d)` | `make_tag(&[u8;4])` (private) | 🟡 | expose (note signature difference) or add a 4-arg form. |
| `CompressionProgress` | `crate::CompressionProgress` | ✅ | `{bytes_done,bytes_total,ratio}` matched exactly (crate root, `write` feature). |
| `ChdInfo` | `crate::ChdInfo` | ✅ | done (crate root). |
| `HunkInfo`, `CompressStep` | new value types | ⬜ | minor; `verify()` lands in Phase G. |
| `ChdIo`, `ChdDataHandler`, `ChdCompressor` | — | 🟡 (document) | not ported: chd-rs is generic + synchronous. Document the equivalent patterns. |

### 3.2 `codec` module

| libchdman-rs | chd-rs target | status | notes |
| --- | --- | --- | --- |
| `CHD_CODEC_NONE/ZLIB/ZSTD/LZMA/HUFF/FLAC/CD_*/AVHUFF` | `pub mod codec` consts | ✅ | done (`src/codec.rs`), re-exported at crate root. |
| `codec_exists(u32)` | `codec::codec_exists` | ✅ | done. |
| `codec_name(u32) -> Option<&str>` | `codec::codec_name` | ✅ | done. |
| `parse_codec_spec(&str) -> Result<[u32;4]>` | `codec::parse_codec_spec` | ✅ | done; `"none"` or 1..=4 mnemonics. |

### 3.3 `hd` module (createhd/extracthd/createraw/extractraw)

| libchdman-rs | chd-rs target | status | notes |
| --- | --- | --- | --- |
| `HdCreateOptions{logical_size,hunk_size,unit_size,codecs,geometry,ident}` | `hd::HdCreateOptions` | ✅ | done; `Default` = hunk 4096 / unit 512 / `[zlib,0,0,0]`. `geometry`/`ident` are Phase B (rejected by `create_raw_*`). |
| `HdGeometry{cylinders,heads,sectors,sector_bytes}` + `logical_bytes()` | `hd::HdGeometry` | ✅ | done. |
| `compute_chs(lb, ss) -> HdGeometry` | `hd::compute_chs` | ✅ | done; port of `guess_chs` (`chdman.cpp:1115`), verified vs chdman geometry. |
| `format_gddd(g)` / `read_geometry(chd)` | `hd::format_gddd` / `hd::read_geometry` | ✅ | done; `read_geometry` parses chdman's `createhd` `GDDD`. (Writing GDDD into a new CHD = Phase B.) |
| `create_from_path/create_from_reader` (createhd) | `hd::create_from_*` (createhd); `hd::create_raw_from_*` (createraw) | ✅ | both done & byte-identical (multi-codec, `progress`/`cancel`). `create_from_*` writes GDDD (+ optional IDNT) + the metadata-inclusive overall SHA-1. |
| `extract_to_path/extract_to_writer` | `hd::extract_to_path` / `hd::extract_to_writer` | ✅ | done; streams logical bytes via the existing decoder (truncates last-hunk padding). |
| `HdImage` (+`open`/`open_with_diff`/`reopen_diff`/`read_sector`/`write_sector`/`as_chd*`) | new (Phase F) | ⬜ | runtime block device; uncompressed writes + diff/parent. |

### 3.4 `cd` module (createcd/extractcd)

| libchdman-rs | chd-rs target | status | notes |
| --- | --- | --- | --- |
| `CdCreateOptions{hunk_size,codecs}` (default 19584, `[cdlz,cdzl,cdfl,0]`) | same | ⬜ | |
| `TrackType`/`SubcodeType`/`TrackInfo` | same (chd-rs has `cdrom`/`metadata::KnownMetadata`) | 🟡/⬜ | reconcile with chd-rs's CD constants. |
| `create_from_cue/create_from_iso` | new | ⬜ | **pure-Rust** CUE/GDI/Nero TOC parser; CD encoders wrap `none/zlib/lzma/zstd/flac` over the 2048+96 sector/subcode split with ECC (reuse `ecc.rs` `generate_ecc`). |
| `list_tracks(chd)` | new (read CHT2 metadata) | ⬜ | |
| `extract_to_cue/extract_to_iso/extract_to_gdi` | new | ⬜ | |
| `CdCookedReader` (+`open`/`open_track`/`Read+Seek`) | new | ⬜ | 2048-byte cooked stream over MODE1 tracks. |

### 3.5 `dvd` module

| libchdman-rs | chd-rs target | status | notes |
| --- | --- | --- | --- |
| `DvdCreateOptions{logical_size,hunk_size,codecs}` (default 4096, `[lzma,zlib,huff,flac]`) | `dvd::DvdCreateOptions` | ✅ | done. |
| `create_from_iso/create_from_reader`, `extract_to_iso/extract_to_writer` | `dvd::*` | ✅ | done & byte-identical; empty `DVD ` record (1-NUL-byte quirk). |

### 3.6 `copy` module

| libchdman-rs | chd-rs target | status | notes |
| --- | --- | --- | --- |
| `CopyOptions{hunk_size:Option<u32>,codecs}` | `copy::CopyOptions` | ✅ | done. |
| `copy(src, dst, opts, progress, cancel)` | `copy::copy` | ✅ | done & byte-identical: reads via `read::ChdReader`, recompresses via `write_raw`, clones all metadata, preserves `raw_sha1`/`unit_bytes`. (Legacy CD/GD metadata re-do = Phase E.) |

### 3.7 `enhancements` (libchdman-rs's pure-Rust helpers — chd-rs mostly already has these)

| libchdman-rs | chd-rs | status | notes |
| --- | --- | --- | --- |
| `Version` | `header::Version` | ✅ | |
| `HunkIter` / `MetadataIter` / `MetadataEntry` | `Chd::hunks()` / `metadata()` (+ `iter::`) | 🟡 | different names; document. |
| `ChdReader` / `HunkReader` | `read::ChdReader` / `read::HunkBufReader` | 🟡 | |
| `metadata::tags::*` / `make_tag` / `is_cdrom`/`is_gdrom` | `metadata::KnownMetadata` | 🟡 | re-expose the tag constants if useful. |
| `cdrom::{CD_*}` | `cdrom.rs` constants | 🟡 | |

---

## 4. Remaining work — phased to completion

Each phase: lands a slice of the API map, gets `chdman_compat` byte-identity tests, and updates docs.
Ordered by dependency; maps onto PARITY_PLAN M3–M8.

- **Phase A — public write surface + `codec` module + flac. ✅ DONE.** Shipped: the `codec` module;
  the `flac` encoder (`RawFlacEncoder`, round-trip-verified — libm-gated byte-identity); the
  **multi-codec `write_raw`**; `CompressionProgress` + the `progress: &mut dyn
  FnMut(CompressionProgress)` / `cancel: &dyn Fn() -> bool` convention; and the public `hd`
  createraw create (`create_raw_from_*`) + `extract_to_*` + geometry helpers (`compute_chs`,
  `format_gddd`, `read_geometry`). `cdfl` and the createhd GDDD-write move to E and B.
- **Phase B — `hd` createhd. ✅ DONE.** Shipped the metadata **writer** for new files
  (`build_metadata_blob`) + the metadata-inclusive overall SHA-1 (`compute_overall_sha1`), wired
  into both writers, and `hd::create_from_*` writing GDDD (+ optional IDNT). Verified byte-identical
  to `chdman createhd` (`-c none`/`zlib`/`lzma`, GDDD+IDNT). The new-file metadata writer is the
  foundation for Phase C's write/delete-on-existing + `copy`.
- **Phase C — metadata write/delete + `copy`. ✅ DONE.** `copy::copy` (recompress + clone metadata)
  and `metadata::write_metadata`/`delete_metadata` (in-place edit of an existing CHD's linked list:
  overwrite-in-place-or-append + relink + overall-SHA-1 update) are both byte-identical to chdman
  (`copy`/`addmeta`/`delmeta`). Note: chdman only edits *uncompressed* CHDs; chd-rs's free functions
  additionally handle compressed CHDs correctly.
- **Phase D — `dvd` module. ✅ DONE.** Flat 2048 sectors + the empty `DVD ` record (1-NUL payload),
  reusing the createhd metadata writer. `createdvd` verified byte-identical (`-c none/zlib/lzma`).
- **Phase E — `cd` module.** CD wrapper **encoders** ✅ DONE (`CdEncoder<E,S>`, cdzl/cdlz byte-
  identical; cdfl pending). Remaining: pure-Rust TOC parser, CHT2 metadata, the createcd/extractcd
  container, `list_tracks`, `extract_to_{cue,iso,gdi}`, `CdCookedReader`. Verify `createcd`/`extractcd`.
- **Phase F — parent/diff + `HdImage`.** Uncompressed diff children vs a compressed parent
  (parent-hunk dedup lights up here — the driver hook exists), runtime `read_sector`/`write_sector`,
  `write_hunk`/`write_bytes` equivalents.
- **Phase G — `Chd::info`/`verify`, `ChdInfo`, rchdman, docs.** Lift verify from rchdman; add
  `ChdInfo`; extend rchdman with `create*`/`copy`/`addmeta`/`delmeta`; port libchdman-rs's
  `format-modules.md` + `chdman-mapping.md`; README rewrite.

---

## 5. Documentation deliverables (update as you go)

1. **`docs/libchdman-differences.md` (new)** — the authoritative "porting from libchdman-rs" guide:
   every 🟡 row above, the `Chd<F>` vs owned-handle model, no `writeable` open, the read-accessor
   patterns, no `ChdIo`/`ChdCompressor`, and the create-function callback convention. Goal: a
   libchdman-rs user can mechanically translate their code.
2. **Port `chdman-mapping.md` + `format-modules.md`** from libchdman-rs into `chd-rs/docs/`, adjusted
   for chd-rs's function names/shapes.
3. **README** — add a write section + a "coming from libchdman-rs?" pointer.
4. **PROGRESS.md** — tick the API-map rows as they land; keep the session log current.

---

## 6. Verification (per phase)

- **chdman byte-identity** (the bar): each new `create*` produces a CHD byte-identical to the
  matching `chdman` command (extend `chdman_compat.rs`; gated `chdman_compat_tests`).
- **Round-trip**: `create → chd-rs decode → equal logical bytes + raw_sha1`.
- **API-shape tests**: a doc-test per new public function so the surface compiles as documented.
- ⚠️ **flac is libm-dependent** — byte-identity for `flac`/`cdfl`/dvd-default needs the reference
  chdman and `libflac-rs` to agree on libm (glibc). Confirm the reference build before asserting
  byte-identity on flac paths; until then gate those as round-trip-only.

---

## 7. Definition of done

Functional parity with every libchdman-rs public API (per §3), each create path byte-identical to
chdman 0.288 (or round-trip-correct where flac/libm blocks byte-identity), the differences doc
complete, and `chd-rs`'s existing read API unchanged.
