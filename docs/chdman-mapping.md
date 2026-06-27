# chdman → chd-rs / rchdman command mapping

How each `chdman` 0.288 sub-command maps onto chd-rs's library API and the `rchdman` CLI. Unless
noted, the **created CHD is byte-for-byte identical to chdman** (the project's verification bar).
Library create/extract functions live in per-format
modules and take `progress: &mut dyn FnMut(CompressionProgress)` + `cancel: &dyn Fn() -> bool`
callbacks (extract takes `&mut dyn FnMut(u64)`); they need the `write` feature.

| chdman | rchdman | chd-rs library | notes |
| --- | --- | --- | --- |
| `info` | `info` | `Chd::info() -> ChdInfo` | read-side; header + metadata tags + track/type detection. |
| `verify` | `verify` | `Chd::verify() -> VerifyResult` | `verify` feature (also via `write`). Recomputes raw + overall SHA-1. Uncompressed CHDs carry no checksum → error. |
| `dumpmeta` | `dumpmeta` | `Chd::metadata_refs()` / `metadata()` | read a metadata record by `(tag, index)`. |
| `createraw` | `createraw` | `hd::create_raw_from_path` / `create_raw_from_reader` | no metadata. `-op <parent>` → `hd::create_raw_from_path_with_parent` (compressed child; `COMPRESSION_PARENT` refs). |
| `extractraw` | `extractraw` | `hd::extract_to_path` / `extract_to_writer` | streams the logical bytes (last-hunk padding trimmed). |
| `createhd` | `createhd` | `hd::create_from_path` / `create_from_reader` | writes a `GDDD` geometry record (auto via `compute_chs`, or `HdCreateOptions.geometry`) + optional `IDNT`. |
| `extracthd` | `extractraw` | `hd::extract_to_path` | an HD CHD's logical bytes are extracted exactly like `extractraw`. |
| `createcd` | `createcd` | `cd::create_from_cue` / `create_from_gdi` / `create_from_iso` | CUE (single/multi-file BIN), Sega `.gdi` (GD-ROM, `CHGD`), or a flat sector image. `CdCreateOptions.codecs` default `[cdlz, cdzl, cdfl, 0]`. |
| `extractcd` | `extractcd` | `cd::extract_to_cue` / `extract_to_gdi` | cue/bin or `.gdi` + split track files. `cd::extract_to_iso` / `CdCookedReader` give a single MODE1 track's cooked 2048-byte data (no chdman equivalent → round-trip-verified). |
| `createdvd` | `createdvd` | `dvd::create_from_iso` / `create_from_reader` | flat 2048-byte sectors + the empty `DVD ` record. |
| `extractdvd` | `extractdvd` | `dvd::extract_to_iso` / `extract_to_writer` | |
| `copy` | `copy` | `copy::copy` | recompress / re-hunk, cloning all metadata. |
| `addmeta` | `addmeta` | `metadata::write_metadata` | in-place edit of an existing CHD's metadata list (`Read + Write + Seek`). chdman only edits *uncompressed* CHDs; chd-rs also handles compressed. |
| `delmeta` | `delmeta` | `metadata::delete_metadata` | unlinks the record (does not recompute the overall SHA-1, matching chdman). |
| `listtemplates` | — | — | not implemented (HD templates). |
| `createld` / `extractld` | — | — | laserdisc / A/V not in scope. |

## Codec mnemonics (`-c`)

`codec::parse_codec_spec("lzma,zlib")` parses a chdman `-c` spec into a `[u32; 4]`. Mnemonics:
`none`, `zlib`, `zstd`, `lzma`, `huff`, `flac`, `cdzl`, `cdzs`, `cdlz`, `cdfl`, `avhu`. `zstd`/`cdzs`
need the `write-zstd` feature.

⚠️ **FLAC byte-identity is libm-gated.** `flac`/`cdfl` (and the HD/DVD default sets that include
`flac`) are byte-identical to a **glibc**-built chdman; against an MSVC/Windows chdman they are
round-trip-correct but may differ by a few bytes. `cdlz`/`cdzl`/`cdzs`/`zlib`/`lzma`/`zstd`/`huff`
are byte-identical on every platform.

## Runtime block device

`hd::HdImage` has no direct chdman command — it is the MAME `harddisk_image_device` surface:
`open` (uncompressed in place), `open_with_diff` / `reopen_diff` (an uncompressed diff over a
compressed parent), and `read_sector` / `write_sector`. The diff format is the one chdman's
`extracthd -ip <parent>` reads back.
