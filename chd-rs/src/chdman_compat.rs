//! Dev-local **bit-exact** verification of chd-rs's codec encoders against a real
//! `chdman` 0.288 binary.
//!
//! Strategy (works before the CHD writer core exists): have chdman build a raw CHD with a
//! single codec, then for every hunk chdman compressed with that codec, decode the hunk's raw
//! data *and* read back its stored compressed bytes, re-encode the raw data with our encoder,
//! and assert the bytes are identical. This pins each codec to chdman 0.288 byte-for-byte.
//!
//! Enable with `cargo test -p chd --features "write-zstd chdman_compat_tests"`. chdman is
//! resolved via `$CHDMAN`, then `C:\Tools\chdman\chdman.exe`, then `chdman` on `PATH`.

use crate::compression::CodecEncodeImplementation;
use crate::header::{CodecType, Header};
use crate::map::{CompressionTypeV5, MapEntry};
use crate::Chd;
use num_traits::ToPrimitive;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::Command;

fn chdman_path() -> PathBuf {
    if let Ok(p) = std::env::var("CHDMAN") {
        return PathBuf::from(p);
    }
    let known = PathBuf::from(r"C:\Tools\chdman\chdman.exe");
    if known.exists() {
        return known;
    }
    PathBuf::from("chdman")
}

/// Deterministic, mixed-compressibility test data: alternating zero runs (very compressible),
/// repeated text (compressible), and xorshift pseudo-random bytes (incompressible). This gives
/// every codec hunks it will actually compress (so the comparison is not vacuous).
fn make_input(len: usize) -> Vec<u8> {
    let text = b"the quick brown fox jumps over the lazy dog. ";
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x2545_f491;
    for i in 0..len {
        let b = match (i / 96) % 3 {
            0 => 0u8,
            1 => text[i % text.len()],
            _ => {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xff) as u8
            }
        };
        v.push(b);
    }
    v
}

/// Create a single-codec raw CHD with chdman and assert our encoder reproduces chdman's
/// compressed bytes for every hunk that used the codec.
fn assert_codec_bit_exact(mnemonic: &str, codec: CodecType, hunk_size: u32, unit_size: u32) {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join(format!("chdrs_bitexact_in_{mnemonic}.bin"));
    let chd_path = dir.join(format!("chdrs_bitexact_ref_{mnemonic}.chd"));

    let nhunks = 24u32;
    let input = make_input((hunk_size * nhunks) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c {mnemonic} failed");

    let mut chd = Chd::open(BufReader::new(File::open(&chd_path).unwrap()), None).unwrap();
    let hunk_count = chd.header().hunk_count();

    let mut comp_buf = Vec::new();
    let mut decoded = chd.get_hunksized_buffer();
    let mut stored = Vec::new();
    let mut checked = 0usize;

    for n in 0..hunk_count {
        // Only compare hunks chdman compressed with codec slot 0 (the codec under test);
        // others are stored uncompressed (NONE).
        let is_codec0 = matches!(
            chd.map().get_entry(n as usize),
            Some(MapEntry::V5Compressed(e))
                if matches!(e.hunk_type(), Ok(CompressionTypeV5::CompressionType0))
        );
        if !is_codec0 {
            continue;
        }

        {
            let mut hunk = chd.hunk(n).unwrap();
            hunk.read_hunk_in(&mut comp_buf, &mut decoded).unwrap();
            hunk.read_raw_in(&mut stored).unwrap();
        }

        let mut enc = codec.init_encoder(hunk_size).unwrap();
        let mut mine = vec![0u8; hunk_size as usize];
        let n_enc = enc.compress(&decoded, &mut mine).unwrap();

        assert_eq!(
            &mine[..n_enc],
            &stored[..],
            "codec {mnemonic}: hunk {n} ({} bytes) differs from chdman ({} bytes)",
            n_enc,
            stored.len()
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no hunks used codec {mnemonic}; test is vacuous (make_input not compressible enough?)"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

// Now backed by `zlib-bitexact-rs` (bit-exact stock zlib 1.3.1 deflate).
#[test]
fn zlib_bit_exact_vs_chdman() {
    assert_codec_bit_exact("zlib", CodecType::ZLibV5, 4096, 512);
}

#[test]
fn huff_bit_exact_vs_chdman() {
    assert_codec_bit_exact("huff", CodecType::HuffV5, 4096, 512);
}

#[test]
fn lzma_bit_exact_vs_chdman() {
    assert_codec_bit_exact("lzma", CodecType::LzmaV5, 4096, 512);
}

#[cfg(feature = "write-zstd")]
#[test]
fn zstd_bit_exact_vs_chdman() {
    assert_codec_bit_exact("zstd", CodecType::ZstdV5, 4096, 512);
}

/// Isolation test for `compress_v5_map`: create a compressed CHD with chdman (huff — a
/// confirmed bit-exact codec), reconstruct its (decompressed) 12-byte rawmap from chd-rs's
/// map entries, re-encode with our `compress_v5_map`, and assert it matches chdman's stored
/// map bytes byte-for-byte. This validates the hardest deterministic piece of the writer
/// independently of the per-hunk driver.
#[test]
fn compress_v5_map_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_map_in.bin");
    let chd_path = dir.join("chdrs_map_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_input((hunk_size * 12) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "huff"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c huff failed");

    let file_bytes = std::fs::read(&chd_path).unwrap();
    let mut chd = Chd::open(BufReader::new(File::open(&chd_path).unwrap()), None).unwrap();
    let hunk_count = chd.header().hunk_count();
    let map_offset = match chd.header() {
        Header::V5Header(h) => h.map_offset,
        _ => panic!("expected a V5 header"),
    };

    // reconstruct the 12-byte rawmap entries from chd-rs's decompressed map
    let mut rawmap = vec![0u8; hunk_count as usize * 12];
    for n in 0..hunk_count {
        let entry = match chd.map().get_entry(n as usize) {
            Some(MapEntry::V5Compressed(e)) => e,
            _ => panic!("hunk {n}: expected a V5 compressed map entry"),
        };
        let type_byte = entry.hunk_type().unwrap().to_u8().unwrap();
        let complen = entry.block_size().unwrap();
        let offset = entry.block_offset().unwrap();
        let crc = entry.hunk_crc().unwrap();

        let base = n as usize * 12;
        rawmap[base] = type_byte;
        rawmap[base + 1] = (complen >> 16) as u8;
        rawmap[base + 2] = (complen >> 8) as u8;
        rawmap[base + 3] = complen as u8;
        rawmap[base + 4] = (offset >> 40) as u8;
        rawmap[base + 5] = (offset >> 32) as u8;
        rawmap[base + 6] = (offset >> 24) as u8;
        rawmap[base + 7] = (offset >> 16) as u8;
        rawmap[base + 8] = (offset >> 8) as u8;
        rawmap[base + 9] = offset as u8;
        rawmap[base + 10] = (crc >> 8) as u8;
        rawmap[base + 11] = crc as u8;
    }

    let ours = crate::write::compress_v5_map(&rawmap, hunk_count, hunk_size, unit_size);
    let reference = &file_bytes[map_offset as usize..];

    assert_eq!(
        ours.len(),
        reference.len(),
        "compressed-map size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        &ours[..],
        reference,
        "compressed-map bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// End-to-end: a complete uncompressed CHD written by chd-rs must be byte-identical to
/// `chdman createraw -c none`. Exercises the V5 header + uncompressed map + layout.
#[test]
fn raw_uncompressed_chd_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_e2e_none_in.bin");
    let chd_path = dir.join("chdrs_e2e_none_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    // 5 full hunks + a partial 6th (unit-aligned, as chdman createraw requires), to exercise
    // the data-start rounding and last-hunk zero-padding.
    let input = make_input((hunk_size * 5 + unit_size * 2) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "none"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c none failed");

    let reference = std::fs::read(&chd_path).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_uncompressed(&mut ours, &input, hunk_size, unit_size).unwrap();
    let ours = ours.into_inner();

    assert_eq!(
        ours.len(),
        reference.len(),
        "file size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(ours, reference, "uncompressed CHD bytes differ from chdman");

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// End-to-end: a complete **compressed** CHD (huff — a confirmed bit-exact codec) written by
/// chd-rs must be byte-identical to `chdman createraw -c huff`. Exercises the full writer core:
/// per-hunk codec/none decision, byte-packed data, `compress_v5_map`, and SHA-1.
#[test]
fn raw_compressed_huff_chd_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_e2e_huff_in.bin");
    let chd_path = dir.join("chdrs_e2e_huff_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_input((hunk_size * 10) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "huff"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c huff failed");

    let reference = std::fs::read(&chd_path).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut ours, &input, hunk_size, unit_size, CodecType::HuffV5)
        .unwrap();
    let ours = ours.into_inner();

    assert_eq!(
        ours.len(),
        reference.len(),
        "file size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "compressed (huff) CHD bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// End-to-end **multi-codec** createraw (`-c lzma,zlib`): the per-hunk best-of-N selection must
/// match chdman's `find_best_compressor` exactly (slot order, strictly-smaller wins), producing a
/// byte-identical whole file. Exercises `write_raw` choosing different codec slots across hunks.
#[test]
fn raw_compressed_multicodec_chd_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_e2e_multi_in.bin");
    let chd_path = dir.join("chdrs_e2e_multi_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_input((hunk_size * 16) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "lzma,zlib"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c lzma,zlib failed");

    let reference = std::fs::read(&chd_path).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw(
        &mut ours,
        &input,
        hunk_size,
        unit_size,
        &[CodecType::LzmaV5, CodecType::ZLibV5],
    )
    .unwrap();
    let ours = ours.into_inner();

    assert_eq!(
        ours.len(),
        reference.len(),
        "file size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "multi-codec (lzma,zlib) CHD bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// The public `hd::create_raw_from_path` must produce a CHD byte-identical to `chdman createraw`,
/// fire the `progress` callback, and honor `cancel` (returning `Cancelled` and leaving no file).
#[test]
fn hd_create_raw_bit_exact_and_callbacks() {
    use crate::hd::{self, HdCreateOptions};
    use crate::{CompressionProgress, CHD_CODEC_ZLIB};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_hd_in.bin");
    let ours_path = dir.join("chdrs_hd_ours.chd");
    let ref_path = dir.join("chdrs_hd_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_input((hunk_size * 12) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&ref_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "zlib"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c zlib failed");
    let reference = std::fs::read(&ref_path).unwrap();

    let opts = HdCreateOptions {
        hunk_size,
        unit_size,
        codecs: [CHD_CODEC_ZLIB, 0, 0, 0],
        ..Default::default()
    };
    let mut last = CompressionProgress {
        bytes_done: 0,
        bytes_total: 0,
        ratio: 1.0,
    };
    let mut count = 0usize;
    {
        let mut prog = |p: CompressionProgress| {
            last = p;
            count += 1;
        };
        hd::create_raw_from_path(&in_path, &ours_path, opts.clone(), &mut prog, &|| false).unwrap();
    }
    let ours = std::fs::read(&ours_path).unwrap();
    assert_eq!(
        ours, reference,
        "hd::create_raw_from_path differs from chdman createraw -c zlib"
    );
    assert!(count > 0, "progress callback never fired");
    assert_eq!(last.bytes_done, input.len() as u64);
    assert_eq!(last.bytes_total, input.len() as u64);

    // cancel → Cancelled + no output file left behind
    let cancel_path = dir.join("chdrs_hd_cancel.chd");
    let _ = std::fs::remove_file(&cancel_path);
    let mut noop = |_p: CompressionProgress| {};
    let r = hd::create_raw_from_path(&in_path, &cancel_path, opts, &mut noop, &|| true);
    assert!(
        matches!(r, Err(crate::Error::Cancelled)),
        "expected Cancelled, got {r:?}"
    );
    assert!(
        !cancel_path.exists(),
        "cancelled create must not leave an output file"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&ref_path);
    let _ = std::fs::remove_file(&ours_path);
}

/// `hd::extract_to_path` must reproduce the exact logical bytes (truncated past the last hunk's
/// zero-padding) for a compressed CHD with a partial last hunk.
#[test]
fn hd_extract_roundtrips() {
    use crate::hd;

    let dir = std::env::temp_dir();
    let chd_path = dir.join("chdrs_hd_extract.chd");
    let out_path = dir.join("chdrs_hd_extract_out.bin");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_input((hunk_size * 7 + unit_size * 2) as usize); // partial last hunk

    let mut cur = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut cur, &input, hunk_size, unit_size, CodecType::LzmaV5)
        .unwrap();
    std::fs::write(&chd_path, cur.into_inner()).unwrap();

    let mut total = 0u64;
    hd::extract_to_path(&chd_path, &out_path, &mut |d| total = d).unwrap();
    let extracted = std::fs::read(&out_path).unwrap();
    assert_eq!(
        extracted, input,
        "extract_to_path did not reproduce the logical bytes"
    );
    assert_eq!(total, input.len() as u64);

    let _ = std::fs::remove_file(&chd_path);
    let _ = std::fs::remove_file(&out_path);
}

/// `hd::read_geometry` must parse the `GDDD` record chdman's `createhd` writes, and our
/// `compute_chs` must predict the same geometry chdman chose.
#[test]
fn hd_read_geometry_vs_chdman() {
    use crate::hd;

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_geom_in.bin");
    let chd_path = dir.join("chdrs_geom_ref.chd");

    let input = make_input(256 * 1024); // 256 KiB → 1 cyl / 16 heads / 32 sectors @ 512
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createhd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-c", "none"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createhd failed");

    let mut chd = Chd::open(BufReader::new(File::open(&chd_path).unwrap()), None).unwrap();
    let g = hd::read_geometry(&mut chd).unwrap();
    assert_eq!(
        (g.cylinders, g.heads, g.sectors, g.sector_bytes),
        (1, 16, 32, 512)
    );
    let predicted = hd::compute_chs(input.len() as u64, 512).unwrap();
    assert_eq!(
        predicted, g,
        "compute_chs disagrees with chdman's chosen geometry"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// Full **createhd** byte-identity: chd-rs `hd::create_from_path` (deriving geometry, writing the
/// `GDDD` record) must equal `chdman createhd -c <mnemonic>` byte-for-byte — exercising the
/// metadata writer (placement + linked list), `meta_offset`, and (compressed) the
/// metadata-inclusive overall SHA-1.
fn assert_createhd_bit_exact(mnemonic: &str, codecs: [u32; 4]) {
    use crate::hd::{self, HdCreateOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join(format!("chdrs_createhd_in_{mnemonic}.bin"));
    let ours_path = dir.join(format!("chdrs_createhd_ours_{mnemonic}.chd"));
    let ref_path = dir.join(format!("chdrs_createhd_ref_{mnemonic}.chd"));

    // 256 KiB → geometry 1/16/32 @ 512, default hunk 4096 (matches chdman's createhd defaults).
    let input = make_input(256 * 1024);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&ref_path);
    let status = Command::new(&chdman)
        .arg("createhd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createhd -c {mnemonic} failed");
    let reference = std::fs::read(&ref_path).unwrap();

    let opts = HdCreateOptions {
        codecs,
        ..Default::default()
    };
    let _ = std::fs::remove_file(&ours_path);
    hd::create_from_path(&in_path, &ours_path, opts, &mut |_p| {}, &|| false).unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createhd -c {mnemonic} size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "createhd -c {mnemonic} bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

#[test]
fn hd_createhd_none_bit_exact_vs_chdman() {
    assert_createhd_bit_exact("none", [0, 0, 0, 0]);
}

#[test]
fn hd_createhd_zlib_bit_exact_vs_chdman() {
    assert_createhd_bit_exact("zlib", [crate::CHD_CODEC_ZLIB, 0, 0, 0]);
}

#[test]
fn hd_createhd_lzma_bit_exact_vs_chdman() {
    assert_createhd_bit_exact("lzma", [crate::CHD_CODEC_LZMA, 0, 0, 0]);
}

/// createhd with an **IDNT** ident record (in addition to GDDD) — exercises the multi-entry
/// metadata linked list (`next` pointers) and the **sorted** multi-entry overall SHA-1. chdman
/// derives geometry from the ident's embedded CHS (LE u16 at offsets 2/6/12), so we pass the same
/// geometry explicitly to chd-rs.
#[test]
fn hd_createhd_ident_bit_exact_vs_chdman() {
    use crate::hd::{self, HdCreateOptions, HdGeometry};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_createhd_ident_in.bin");
    let ident_path = dir.join("chdrs_createhd_ident.bin");
    let ours_path = dir.join("chdrs_createhd_ident_ours.chd");
    let ref_path = dir.join("chdrs_createhd_ident_ref.chd");

    // ident blob: CHS at LE offsets 2/6/12 → 2 cyls / 4 heads / 8 sectors = 64 sectors.
    let mut ident = vec![0u8; 16];
    ident[0] = 0xCA;
    ident[1] = 0xFE;
    ident[2] = 2; // cylinders (LE u16)
    ident[6] = 4; // heads
    ident[12] = 8; // sectors
    ident[14] = 0x5A;
    ident[15] = 0xA5;
    std::fs::write(&ident_path, &ident).unwrap();

    let geom = HdGeometry {
        cylinders: 2,
        heads: 4,
        sectors: 8,
        sector_bytes: 512,
    };
    let input = make_input(geom.logical_bytes() as usize); // 64 * 512 = 32768
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&ref_path);
    let status = Command::new(&chdman)
        .arg("createhd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", "zlib"])
        .arg("--ident")
        .arg(&ident_path)
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createhd --ident failed");
    let reference = std::fs::read(&ref_path).unwrap();

    let opts = HdCreateOptions {
        codecs: [crate::CHD_CODEC_ZLIB, 0, 0, 0],
        geometry: Some(geom),
        ident: Some(ident.clone()),
        ..Default::default()
    };
    let _ = std::fs::remove_file(&ours_path);
    hd::create_from_path(&in_path, &ours_path, opts, &mut |_p| {}, &|| false).unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createhd --ident size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "createhd --ident (GDDD+IDNT) bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&ident_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

/// `copy` byte-identity: build an HD source CHD with chdman (`createhd -c <source_codec>`), then
/// `chdman copy -c <target>` and chd-rs `copy::copy` must produce the same file — exercising
/// re-compression, unit-size preservation, and **metadata (GDDD) cloning** (+ overall SHA-1).
fn assert_copy_bit_exact(source_codec: &str, target_mnemonic: &str, target_codecs: [u32; 4]) {
    use crate::copy::{self, CopyOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let tag = format!("{source_codec}_{target_mnemonic}");
    let in_path = dir.join(format!("chdrs_copy_in_{tag}.bin"));
    let src_path = dir.join(format!("chdrs_copy_src_{tag}.chd"));
    let ours_path = dir.join(format!("chdrs_copy_ours_{tag}.chd"));
    let ref_path = dir.join(format!("chdrs_copy_ref_{tag}.chd"));

    let input = make_input(256 * 1024);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&src_path);
    let s = Command::new(&chdman)
        .arg("createhd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&src_path)
        .args(["-c", source_codec])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "createhd source (-c {source_codec}) failed");

    let _ = std::fs::remove_file(&ref_path);
    let s = Command::new(&chdman)
        .arg("copy")
        .arg("-i")
        .arg(&src_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", target_mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman copy -c {target_mnemonic} failed");
    let reference = std::fs::read(&ref_path).unwrap();

    let _ = std::fs::remove_file(&ours_path);
    copy::copy(
        &src_path,
        &ours_path,
        CopyOptions {
            hunk_size: None,
            codecs: target_codecs,
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "copy {tag} size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(ours, reference, "copy {tag} bytes differ from chdman");

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

#[test]
fn copy_hd_none_to_zlib_bit_exact_vs_chdman() {
    // uncompressed source → compressed copy (exercises overall SHA-1 + GDDD clone)
    assert_copy_bit_exact("none", "zlib", [crate::CHD_CODEC_ZLIB, 0, 0, 0]);
}

#[test]
fn copy_hd_zlib_to_none_bit_exact_vs_chdman() {
    // compressed source → uncompressed copy (SHA-1 zero; GDDD clone after the map)
    assert_copy_bit_exact("zlib", "none", [0, 0, 0, 0]);
}

#[test]
fn copy_hd_lzma_to_zlib_bit_exact_vs_chdman() {
    // compressed → compressed recompression
    assert_copy_bit_exact("lzma", "zlib", [crate::CHD_CODEC_ZLIB, 0, 0, 0]);
}

/// Full **createdvd** byte-identity: chd-rs `dvd::create_from_iso` (writing the `DVD ` record) must
/// equal `chdman createdvd -c <mnemonic>` byte-for-byte — exercising the 2048-unit DVD layout, the
/// 1-NUL `DVD ` metadata record, and (compressed) the metadata-inclusive overall SHA-1.
fn assert_createdvd_bit_exact(mnemonic: &str, codecs: [u32; 4]) {
    use crate::dvd::{self, DvdCreateOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join(format!("chdrs_createdvd_in_{mnemonic}.iso"));
    let ours_path = dir.join(format!("chdrs_createdvd_ours_{mnemonic}.chd"));
    let ref_path = dir.join(format!("chdrs_createdvd_ref_{mnemonic}.chd"));

    // 256 KiB = 128 × 2048-byte sectors.
    let input = make_input(256 * 1024);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let status = Command::new(&chdman)
        .arg("createdvd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createdvd -c {mnemonic} failed");
    let reference = std::fs::read(&ref_path).unwrap();

    let opts = DvdCreateOptions {
        codecs,
        ..Default::default()
    };
    dvd::create_from_iso(&in_path, &ours_path, opts, &mut |_p| {}, &|| false).unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createdvd -c {mnemonic} size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "createdvd -c {mnemonic} bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

#[test]
fn createdvd_none_bit_exact_vs_chdman() {
    assert_createdvd_bit_exact("none", [0, 0, 0, 0]);
}

#[test]
fn createdvd_zlib_bit_exact_vs_chdman() {
    assert_createdvd_bit_exact("zlib", [crate::CHD_CODEC_ZLIB, 0, 0, 0]);
}

#[test]
fn createdvd_lzma_bit_exact_vs_chdman() {
    assert_createdvd_bit_exact("lzma", [crate::CHD_CODEC_LZMA, 0, 0, 0]);
}

/// `dvd::extract_to_iso` round-trips the logical image (partial last hunk) and rejects a non-DVD
/// CHD with `UnsupportedFormat`.
#[test]
fn dvd_extract_roundtrips_and_rejects_non_dvd() {
    use crate::dvd::{self, DvdCreateOptions};

    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_dvd_rt_in.iso");
    let chd_path = dir.join("chdrs_dvd_rt.chd");
    let out_path = dir.join("chdrs_dvd_rt_out.iso");
    let raw_path = dir.join("chdrs_dvd_rt_raw.chd");

    let input = make_input(2048 * 53); // 2048-aligned, partial last 4096 hunk
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    dvd::create_from_iso(
        &in_path,
        &chd_path,
        DvdCreateOptions {
            codecs: [crate::CHD_CODEC_LZMA, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();

    let mut total = 0u64;
    dvd::extract_to_iso(&chd_path, &out_path, &mut |d| total = d).unwrap();
    assert_eq!(
        std::fs::read(&out_path).unwrap(),
        input,
        "dvd round-trip mismatch"
    );
    assert_eq!(total, input.len() as u64);

    // a raw (non-DVD) CHD must be rejected by dvd::extract
    let mut cur = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut cur, &input, 4096, 2048, CodecType::LzmaV5).unwrap();
    std::fs::write(&raw_path, cur.into_inner()).unwrap();
    assert!(
        matches!(
            dvd::extract_to_iso(&raw_path, &out_path, &mut |_d| {}),
            Err(crate::Error::UnsupportedFormat)
        ),
        "dvd::extract should reject a non-DVD CHD"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&raw_path);
}

/// `metadata::write_metadata` must match `chdman addmeta` byte-for-byte: append a new record to a
/// CHD (linked into the list at EOF). chdman only edits **uncompressed** CHDs (MAME refuses a
/// writeable open of a compressed one), so the base is `-c none` and the overall SHA-1 stays zero.
#[test]
fn addmeta_bit_exact_vs_chdman() {
    use crate::metadata::{self, METADATA_FLAG_CHECKSUM};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_addmeta_in.bin");
    let base = dir.join("chdrs_addmeta_base.chd");
    let ours = dir.join("chdrs_addmeta_ours.chd");
    let reference = dir.join("chdrs_addmeta_ref.chd");

    let input = make_input(256 * 1024);
    File::create(&in_path).unwrap().write_all(&input).unwrap();
    let s = Command::new(&chdman)
        .arg("createhd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&base)
        .args(["-c", "none"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "createhd base failed");

    std::fs::copy(&base, &ours).unwrap();
    std::fs::copy(&base, &reference).unwrap();

    // chdman addmeta mutates the input in place. `--valuetext` stores the string + NUL terminator.
    let s = Command::new(&chdman)
        .arg("addmeta")
        .arg("-i")
        .arg(&reference)
        .args(["--tag", "TEST"])
        .args(["--valuetext", "hello world"])
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman addmeta failed");

    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&ours)
            .unwrap();
        metadata::write_metadata(
            &mut f,
            crate::make_tag(b"TEST"),
            0,
            b"hello world\0", // text form: trailing NUL like chdman's std::string overload
            METADATA_FLAG_CHECKSUM,
        )
        .unwrap();
    }

    let ours_bytes = std::fs::read(&ours).unwrap();
    let ref_bytes = std::fs::read(&reference).unwrap();
    assert_eq!(
        ours_bytes.len(),
        ref_bytes.len(),
        "addmeta size differs: ours={}, chdman={}",
        ours_bytes.len(),
        ref_bytes.len()
    );
    assert_eq!(ours_bytes, ref_bytes, "addmeta bytes differ from chdman");

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&base);
    let _ = std::fs::remove_file(&ours);
    let _ = std::fs::remove_file(&reference);
}

/// `metadata::delete_metadata` must match `chdman delmeta` byte-for-byte: unlink the record (and,
/// like chdman, leave the overall SHA-1 stale).
#[test]
fn delmeta_bit_exact_vs_chdman() {
    use crate::metadata;

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_delmeta_in.bin");
    let base = dir.join("chdrs_delmeta_base.chd");
    let ours = dir.join("chdrs_delmeta_ours.chd");
    let reference = dir.join("chdrs_delmeta_ref.chd");

    let input = make_input(256 * 1024);
    File::create(&in_path).unwrap().write_all(&input).unwrap();
    let s = Command::new(&chdman)
        .arg("createhd")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&base)
        .args(["-c", "none"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "createhd base failed");

    std::fs::copy(&base, &ours).unwrap();
    std::fs::copy(&base, &reference).unwrap();

    let s = Command::new(&chdman)
        .arg("delmeta")
        .arg("-i")
        .arg(&reference)
        .args(["--tag", "GDDD"])
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman delmeta failed");

    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&ours)
            .unwrap();
        metadata::delete_metadata(&mut f, crate::make_tag(b"GDDD"), 0).unwrap();
    }

    let ours_bytes = std::fs::read(&ours).unwrap();
    let ref_bytes = std::fs::read(&reference).unwrap();
    assert_eq!(ours_bytes, ref_bytes, "delmeta bytes differ from chdman");

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&base);
    let _ = std::fs::remove_file(&ours);
    let _ = std::fs::remove_file(&reference);
}

/// `Chd::info` reports the right header fields + type flags (`is_hd`/`is_dvd`/…) for HD and DVD
/// CHDs created by chd-rs.
#[test]
fn info_reports_hd_and_dvd() {
    use crate::dvd::{self, DvdCreateOptions};
    use crate::hd::{self, HdCreateOptions};

    let dir = std::env::temp_dir();
    let input = make_input(256 * 1024);

    // HD (compressed zlib) → is_hd, GDDD, compressed.
    let hd_in = dir.join("chdrs_info_hd_in.bin");
    let hd_chd = dir.join("chdrs_info_hd.chd");
    File::create(&hd_in).unwrap().write_all(&input).unwrap();
    hd::create_from_path(
        &hd_in,
        &hd_chd,
        HdCreateOptions {
            codecs: [crate::CHD_CODEC_ZLIB, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    {
        let mut chd = Chd::open(BufReader::new(File::open(&hd_chd).unwrap()), None).unwrap();
        let info = chd.info().unwrap();
        assert_eq!(info.version, 5);
        assert_eq!(info.hunk_bytes, 4096);
        assert_eq!(info.unit_bytes, 512);
        assert_eq!(info.logical_bytes, input.len() as u64);
        assert_eq!(info.codecs[0], crate::CHD_CODEC_ZLIB);
        assert!(info.compressed);
        assert!(info.is_hd, "should be HD");
        assert!(!info.is_dvd && !info.is_cd && !info.is_av && !info.is_gd);
        assert!(!info.has_parent);
        assert_eq!(info.track_count, 0);
        assert!(info
            .metadata_tags
            .iter()
            .any(|&(t, _)| t == crate::make_tag(b"GDDD")));
    }

    // DVD (uncompressed) → is_dvd, DVD record, 2048 units, not compressed.
    let dvd_in = dir.join("chdrs_info_dvd_in.iso");
    let dvd_chd = dir.join("chdrs_info_dvd.chd");
    File::create(&dvd_in).unwrap().write_all(&input).unwrap();
    dvd::create_from_iso(
        &dvd_in,
        &dvd_chd,
        DvdCreateOptions {
            codecs: [0, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    {
        let mut chd = Chd::open(BufReader::new(File::open(&dvd_chd).unwrap()), None).unwrap();
        let info = chd.info().unwrap();
        assert!(info.is_dvd, "should be DVD");
        assert!(!info.is_hd);
        assert!(!info.compressed);
        assert_eq!(info.unit_bytes, 2048);
        assert!(info
            .metadata_tags
            .iter()
            .any(|&(t, _)| t == crate::make_tag(b"DVD ")));
    }

    let _ = std::fs::remove_file(&hd_in);
    let _ = std::fs::remove_file(&hd_chd);
    let _ = std::fs::remove_file(&dvd_in);
    let _ = std::fs::remove_file(&dvd_chd);
}

/// Per-hunk byte-identity for a **CD wrapper codec**: build a MODE1/2352 BIN+CUE (valid sync+ECC,
/// so the codec exercises the ECC-strip path), `chdman createcd -c <mnemonic>`, then for every hunk
/// chdman compressed with the codec, decode it (chd-rs restores sync+ECC + subcode), re-encode with
/// our `CdEncoder`, and assert the stored bytes match.
#[cfg(all(feature = "want_raw_data_sector", feature = "want_subcode"))]
fn assert_cd_codec_bit_exact(mnemonic: &str, codec: CodecType) {
    use crate::cdrom::{CD_MAX_SECTOR_DATA, CD_MODE_OFFSET, CD_SYNC_HEADER};
    use crate::compression::ecc::ErrorCorrectedSector;

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let bin_path = dir.join(format!("chdrs_cd_{mnemonic}.bin"));
    let cue_path = dir.join(format!("chdrs_cd_{mnemonic}.cue"));
    let chd_path = dir.join(format!("chdrs_cd_{mnemonic}.chd"));

    // 64 MODE1 sectors (8 frames/hunk → 8 full hunks). Compressible payload (via make_input) +
    // a freshly-generated valid P/Q ECC so chdman's codec strips the sync header + ECC.
    let nsectors = 64usize;
    let mut bin = make_input(nsectors * CD_MAX_SECTOR_DATA as usize);
    for s in 0..nsectors {
        let sector = &mut bin[s * CD_MAX_SECTOR_DATA as usize..][..CD_MAX_SECTOR_DATA as usize];
        sector[..CD_SYNC_HEADER.len()].copy_from_slice(&CD_SYNC_HEADER);
        sector[CD_MODE_OFFSET] = 1; // MODE1
        let mut sec = <&mut [u8; CD_MAX_SECTOR_DATA as usize]>::try_from(&mut sector[..]).unwrap();
        sec.generate_ecc();
    }
    File::create(&bin_path).unwrap().write_all(&bin).unwrap();

    let bin_name = bin_path.file_name().unwrap().to_str().unwrap();
    let cue = format!("FILE \"{bin_name}\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n");
    File::create(&cue_path)
        .unwrap()
        .write_all(cue.as_bytes())
        .unwrap();

    let status = Command::new(&chdman)
        .arg("createcd")
        .arg("-i")
        .arg(&cue_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-c", mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createcd -c {mnemonic} failed");

    let mut chd = Chd::open(BufReader::new(File::open(&chd_path).unwrap()), None).unwrap();
    let hunk_count = chd.header().hunk_count();
    let hunk_size = chd.header().hunk_size();

    let mut comp_buf = Vec::new();
    let mut decoded = chd.get_hunksized_buffer();
    let mut stored = Vec::new();
    let mut checked = 0usize;
    for n in 0..hunk_count {
        let is_codec0 = matches!(
            chd.map().get_entry(n as usize),
            Some(MapEntry::V5Compressed(e))
                if matches!(e.hunk_type(), Ok(CompressionTypeV5::CompressionType0))
        );
        if !is_codec0 {
            continue;
        }
        {
            let mut hunk = chd.hunk(n).unwrap();
            hunk.read_hunk_in(&mut comp_buf, &mut decoded).unwrap();
            hunk.read_raw_in(&mut stored).unwrap();
        }
        let mut enc = codec.init_encoder(hunk_size).unwrap();
        let mut mine = vec![0u8; hunk_size as usize];
        let n_enc = enc.compress(&decoded, &mut mine).unwrap();
        assert_eq!(
            &mine[..n_enc],
            &stored[..],
            "cd codec {mnemonic}: hunk {n} ({n_enc} bytes) differs from chdman ({} bytes)",
            stored.len()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no hunks used codec {mnemonic}; test is vacuous"
    );

    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(&cue_path);
    let _ = std::fs::remove_file(&chd_path);
}

#[cfg(all(feature = "want_raw_data_sector", feature = "want_subcode"))]
#[test]
fn cd_zlib_bit_exact_vs_chdman() {
    assert_cd_codec_bit_exact("cdzl", CodecType::ZLibCdV5);
}

#[cfg(all(feature = "want_raw_data_sector", feature = "want_subcode"))]
#[test]
fn cd_lzma_bit_exact_vs_chdman() {
    assert_cd_codec_bit_exact("cdlz", CodecType::LzmaCdV5);
}

/// `HdImage` diff cross-check: chd-rs writes sectors into an uncompressed diff over a chdman-made
/// compressed parent, then **chdman `extracthd -ip parent`** reads the diff back — proving our diff
/// (parent_sha1 link, 4-byte map, materialised hunks) is chdman-compatible and the merged image
/// matches (parent with our sector writes overlaid).
#[test]
fn hd_image_diff_readable_by_chdman() {
    use crate::hd::HdImage;

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let parent_in = dir.join("chdrs_hddiff_in.bin");
    let parent_chd = dir.join("chdrs_hddiff_parent.chd");
    let diff_chd = dir.join("chdrs_hddiff_diff.chd");
    let merged = dir.join("chdrs_hddiff_merged.bin");

    let img: Vec<u8> = (0..256 * 1024).map(|i| (i * 5 + 1) as u8).collect();
    File::create(&parent_in).unwrap().write_all(&img).unwrap();

    let s = Command::new(&chdman)
        .arg("createhd")
        .args(["-i".as_ref(), parent_in.as_os_str()])
        .args(["-o".as_ref(), parent_chd.as_os_str()])
        .args(["-c", "zlib"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman createhd (parent) failed");

    let mut expected = img.clone();
    {
        let mut hd = HdImage::open_with_diff(&parent_chd, &diff_chd).unwrap();
        let ss = hd.sector_size() as usize;
        for &lba in &[3u64, 77, 313, 511] {
            let pat = vec![(lba as u8) ^ 0xa5; ss];
            hd.write_sector(lba, &pat).unwrap();
            expected[lba as usize * ss..][..ss].copy_from_slice(&pat);
        }
        hd.flush().unwrap();
    }

    let s = Command::new(&chdman)
        .arg("extracthd")
        .args(["-i".as_ref(), diff_chd.as_os_str()])
        .args(["-ip".as_ref(), parent_chd.as_os_str()])
        .args(["-o".as_ref(), merged.as_os_str()])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman extracthd of our diff failed");

    assert_eq!(
        std::fs::read(&merged).unwrap(),
        expected,
        "chdman-extracted diff differs from the expected merged image"
    );

    for p in [&parent_in, &parent_chd, &diff_chd, &merged] {
        let _ = std::fs::remove_file(p);
    }
}

/// Build `pattern_ids.len()` hunks; hunks sharing a pattern id are byte-identical (forcing
/// `COMPRESSION_SELF` refs). Pattern 0 is all-zeros; others are a distinct compressible sawtooth.
fn build_hunks(hunk_size: usize, pattern_ids: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; hunk_size * pattern_ids.len()];
    for (h, &pid) in pattern_ids.iter().enumerate() {
        if pid == 0 {
            continue; // all zeros
        }
        let hunk = &mut v[h * hunk_size..(h + 1) * hunk_size];
        let period = pid as usize * 13 + 7;
        for (i, b) in hunk.iter_mut().enumerate() {
            *b = ((i % period) as u32 + pid as u32) as u8;
        }
    }
    v
}

/// End-to-end with **self-hunk dedup**: an input with byte-identical hunks at various distances
/// (consecutive, repeated, all-zero) must produce the same `COMPRESSION_SELF` references chdman
/// emits — byte-identical whole file.
#[test]
fn raw_compressed_dedup_chd_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_e2e_dedup_in.bin");
    let chd_path = dir.join("chdrs_e2e_dedup_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    // hunk pattern ids: repeats force SELF refs (incl. consecutive zeros and far repeats).
    let pattern_ids = [1u8, 2, 1, 0, 0, 2, 3, 0, 3, 1];
    let input = build_hunks(hunk_size as usize, &pattern_ids);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "huff"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c huff (dedup) failed");

    let reference = std::fs::read(&chd_path).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut ours, &input, hunk_size, unit_size, CodecType::HuffV5)
        .unwrap();
    let ours = ours.into_inner();

    assert_eq!(
        ours.len(),
        reference.len(),
        "file size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "dedup (SELF-ref) CHD bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// End-to-end compressed CHD with **zlib** — the HD-default codec, now backed by
/// `zlib-bitexact-rs`. A full `-c zlib` CHD must be byte-identical to chdman 0.288.
#[test]
fn raw_compressed_zlib_chd_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_e2e_zlib_in.bin");
    let chd_path = dir.join("chdrs_e2e_zlib_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_input((hunk_size * 10) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "zlib"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c zlib failed");

    let reference = std::fs::read(&chd_path).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut ours, &input, hunk_size, unit_size, CodecType::ZLibV5)
        .unwrap();
    let ours = ours.into_inner();

    assert_eq!(
        ours.len(),
        reference.len(),
        "file size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "compressed (zlib) CHD bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// Audio-like 16-bit stereo PCM (a bounded triangle wave per channel), little-endian, so the
/// FLAC codec actually compresses (instead of overflowing to NONE on incompressible data).
fn make_audio_input(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let (mut l, mut r, mut dl, mut dr): (i32, i32, i32, i32) = (0, 0, 37, 53);
    while v.len() < len {
        l += dl;
        if !(-20000..=20000).contains(&l) {
            dl = -dl;
            l += 2 * dl;
        }
        r += dr;
        if !(-18000..=18000).contains(&r) {
            dr = -dr;
            r += 2 * dr;
        }
        v.extend_from_slice(&(l as i16).to_le_bytes());
        v.extend_from_slice(&(r as i16).to_le_bytes());
    }
    v.truncate(len);
    v
}

/// End-to-end **flac** — a libm-dependent codec. libflac-rs's float parity is validated against
/// **glibc** libm, while the reference chdman here is the **Windows/MSVC** 0.288 build, so we do
/// **not** assert byte-identity to chdman (it may differ by a few bytes). Instead we assert
/// *round-trip correctness against the oracle*: chd-rs writes a `-c flac` CHD, our own decoder
/// reproduces the logical bytes, and **chdman extracts our CHD back to the original input**. (On a
/// glibc chdman this can be upgraded to a byte-identity assertion like the other codecs.)
#[test]
fn raw_compressed_flac_roundtrips_via_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let chd_path = dir.join("chdrs_e2e_flac_ref.chd");
    let out_path = dir.join("chdrs_e2e_flac_out.bin");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    let input = make_audio_input((hunk_size * 8) as usize);

    // write the flac CHD with chd-rs
    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut ours, &input, hunk_size, unit_size, CodecType::FlacV5)
        .unwrap();
    let ours = ours.into_inner();
    std::fs::write(&chd_path, &ours).unwrap();

    // sanity + chd-rs self round-trip
    {
        let mut chd = Chd::open(BufReader::new(File::open(&chd_path).unwrap()), None).unwrap();
        let hunk_count = chd.header().hunk_count();

        let used_codec = (0..hunk_count).any(|n| {
            matches!(
                chd.map().get_entry(n as usize),
                Some(MapEntry::V5Compressed(e))
                    if matches!(e.hunk_type(), Ok(CompressionTypeV5::CompressionType0))
            )
        });
        assert!(used_codec, "no hunk used the flac codec; test is vacuous");

        let mut decoded = chd.get_hunksized_buffer();
        let mut comp = Vec::new();
        let mut got = Vec::with_capacity(input.len());
        for n in 0..hunk_count {
            let mut hunk = chd.hunk(n).unwrap();
            hunk.read_hunk_in(&mut comp, &mut decoded).unwrap();
            got.extend_from_slice(&decoded);
        }
        assert_eq!(
            &got[..input.len()],
            &input[..],
            "chd-rs flac self round-trip mismatch"
        );
    }

    // the oracle must accept and losslessly extract our flac CHD
    let _ = std::fs::remove_file(&out_path);
    let status = Command::new(&chdman)
        .arg("extractraw")
        .arg("-i")
        .arg(&chd_path)
        .arg("-o")
        .arg(&out_path)
        .arg("-f")
        .status()
        .expect("failed to run chdman extractraw");
    assert!(status.success(), "chdman extractraw of our flac CHD failed");

    let extracted = std::fs::read(&out_path).unwrap();
    assert_eq!(extracted, input, "chdman-extracted bytes differ from input");

    let _ = std::fs::remove_file(&chd_path);
    let _ = std::fs::remove_file(&out_path);
}

/// Dump a chd-rs `flac` CHD + its input to `C:\Temp\flac_check\` so an **external glibc-built
/// chdman** can be compared byte-for-byte (our `libflac-rs` is validated vs glibc libm, but the
/// usual oracle here is the MSVC chdman). `#[ignore]`d — run explicitly when a glibc chdman is
/// available: `cargo test ... dump_flac_artifacts_for_glibc_check -- --ignored`.
#[test]
#[ignore = "writes flac artifacts for an external glibc-chdman byte-identity check"]
fn dump_flac_artifacts_for_glibc_check() {
    let dir = std::path::Path::new(r"C:\Temp\flac_check");
    std::fs::create_dir_all(dir).unwrap();
    let (hunk, unit) = (4096u32, 512u32);
    let input = make_audio_input((hunk * 8) as usize);
    std::fs::write(dir.join("input.bin"), &input).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut ours, &input, hunk, unit, CodecType::FlacV5).unwrap();
    std::fs::write(dir.join("ours.chd"), ours.into_inner()).unwrap();
    eprintln!("wrote {}\\input.bin and ours.chd", dir.display());
}

/// Dump a chd-rs **`cdfl`** CD CHD (single AUDIO track of smooth PCM, so FLAC engages) + its CUE/BIN
/// to `C:\Temp\cdfl_check\` for a byte-for-byte comparison against an external glibc chdman:
/// `chdman createcd -i audio.cue -o chdman.chd -c cdfl`. `#[ignore]`d (libm-gated, glibc only).
#[test]
#[ignore = "writes cdfl artifacts for an external glibc-chdman byte-identity check"]
fn dump_cdfl_artifacts_for_glibc_check() {
    use crate::cd::{self, CdCreateOptions};

    let dir = std::path::Path::new(r"C:\Temp\cdfl_check");
    std::fs::create_dir_all(dir).unwrap();
    // 64 AUDIO sectors (8 full hunks, no padding) of smooth stereo PCM.
    let bin = make_audio_input(64 * 2352);
    std::fs::write(dir.join("audio.bin"), &bin).unwrap();
    std::fs::write(
        dir.join("audio.cue"),
        b"FILE \"audio.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n".as_slice(),
    )
    .unwrap();

    cd::create_from_cue(
        &dir.join("audio.cue"),
        &dir.join("ours.chd"),
        CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_FLAC, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    eprintln!("wrote cdfl artifacts to {}", dir.display());
}

/// End-to-end compressed CHD with **lzma** (an external-crate codec) and a **partial last
/// hunk** — confirms the driver is codec-agnostic and that `raw_sha1` is over the logical
/// (unpadded) data with the final hunk zero-padded.
#[test]
fn raw_compressed_lzma_partial_chd_bit_exact_vs_chdman() {
    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let in_path = dir.join("chdrs_e2e_lzma_in.bin");
    let chd_path = dir.join("chdrs_e2e_lzma_ref.chd");

    let hunk_size = 4096u32;
    let unit_size = 512u32;
    // 7 full hunks + a partial 8th (unit-aligned).
    let input = make_input((hunk_size * 7 + unit_size * 3) as usize);
    File::create(&in_path).unwrap().write_all(&input).unwrap();

    let _ = std::fs::remove_file(&chd_path);
    let status = Command::new(&chdman)
        .arg("createraw")
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&chd_path)
        .args(["-hs", &hunk_size.to_string()])
        .args(["-us", &unit_size.to_string()])
        .args(["-c", "lzma"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createraw -c lzma failed");

    let reference = std::fs::read(&chd_path).unwrap();

    let mut ours = std::io::Cursor::new(Vec::new());
    crate::write::write_raw_compressed(&mut ours, &input, hunk_size, unit_size, CodecType::LzmaV5)
        .unwrap();
    let ours = ours.into_inner();

    assert_eq!(
        ours.len(),
        reference.len(),
        "file size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "compressed (lzma) CHD bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&chd_path);
}

/// Build `nsectors` MODE1/2352 sectors (sync header + MODE1 byte + compressible payload + a freshly
/// generated valid P/Q ECC, so the CD codec strips them). Same construction the per-hunk CD codec
/// test uses, hoisted for the `createcd` container tests.
fn build_mode1_bin(nsectors: usize) -> Vec<u8> {
    use crate::cdrom::{CD_MAX_SECTOR_DATA, CD_MODE_OFFSET, CD_SYNC_HEADER};
    use crate::compression::ecc::ErrorCorrectedSector;

    let mut bin = make_input(nsectors * CD_MAX_SECTOR_DATA as usize);
    for s in 0..nsectors {
        let sector = &mut bin[s * CD_MAX_SECTOR_DATA as usize..][..CD_MAX_SECTOR_DATA as usize];
        sector[..CD_SYNC_HEADER.len()].copy_from_slice(&CD_SYNC_HEADER);
        sector[CD_MODE_OFFSET] = 1; // MODE1
        let mut sec = <&mut [u8; CD_MAX_SECTOR_DATA as usize]>::try_from(&mut sector[..]).unwrap();
        sec.generate_ecc();
    }
    bin
}

/// Full **createcd** byte-identity for a single MODE1/2352 track: chd-rs `cd::create_from_cue`
/// (CUE parse → 2448-byte-frame assembly → `CHT2` metadata → V5 writer) must equal
/// `chdman createcd -c <mnemonic>` byte-for-byte. 50 frames → 7 hunks (a partial last hunk).
fn assert_createcd_cue_bit_exact(mnemonic: &str, codecs: [u32; 4]) {
    use crate::cd::{self, CdCreateOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let bin_path = dir.join(format!("chdrs_createcd_{mnemonic}.bin"));
    let cue_path = dir.join(format!("chdrs_createcd_{mnemonic}.cue"));
    let ours_path = dir.join(format!("chdrs_createcd_ours_{mnemonic}.chd"));
    let ref_path = dir.join(format!("chdrs_createcd_ref_{mnemonic}.chd"));

    let bin = build_mode1_bin(50);
    File::create(&bin_path).unwrap().write_all(&bin).unwrap();
    let bin_name = bin_path.file_name().unwrap().to_str().unwrap();
    let cue = format!("FILE \"{bin_name}\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n");
    File::create(&cue_path)
        .unwrap()
        .write_all(cue.as_bytes())
        .unwrap();

    let status = Command::new(&chdman)
        .arg("createcd")
        .arg("-i")
        .arg(&cue_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createcd -c {mnemonic} failed");
    let reference = std::fs::read(&ref_path).unwrap();

    cd::create_from_cue(
        &cue_path,
        &ours_path,
        CdCreateOptions {
            codecs,
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createcd -c {mnemonic} size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "createcd -c {mnemonic} bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(&cue_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

#[test]
fn createcd_cue_cdzl_bit_exact_vs_chdman() {
    assert_createcd_cue_bit_exact("cdzl", [crate::CHD_CODEC_CD_ZLIB, 0, 0, 0]);
}

#[test]
fn createcd_cue_cdlz_bit_exact_vs_chdman() {
    assert_createcd_cue_bit_exact("cdlz", [crate::CHD_CODEC_CD_LZMA, 0, 0, 0]);
}

/// **createcd** byte-identity for a **multi-track** single-BIN CUE: a MODE1/2352 data track
/// followed by an AUDIO track with an in-file pregap (`INDEX 00`/`INDEX 01`). Exercises per-track
/// `CHT2` (incl. the `V`-prefixed `PGTYPE` for a data-bearing pregap), the audio byte-swap, and the
/// 4-frame track padding between tracks.
fn assert_createcd_multitrack_bit_exact(mnemonic: &str, codecs: [u32; 4]) {
    use crate::cd::{self, CdCreateOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let bin_path = dir.join(format!("chdrs_createcd_mt_{mnemonic}.bin"));
    let cue_path = dir.join(format!("chdrs_createcd_mt_{mnemonic}.cue"));
    let ours_path = dir.join(format!("chdrs_createcd_mt_ours_{mnemonic}.chd"));
    let ref_path = dir.join(format!("chdrs_createcd_mt_ref_{mnemonic}.chd"));

    // 50 MODE1 frames then 30 AUDIO frames, one bin. INDEX 00 of track 2 = frame 50 (so track 1 is
    // 50 frames), INDEX 01 = frame 53 (a 3-frame pregap).
    let mut bin = build_mode1_bin(50);
    bin.extend_from_slice(&make_audio_input(30 * 2352));
    File::create(&bin_path).unwrap().write_all(&bin).unwrap();
    let bin_name = bin_path.file_name().unwrap().to_str().unwrap();
    let cue = format!(
        "FILE \"{bin_name}\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  \
         TRACK 02 AUDIO\n    INDEX 00 00:00:50\n    INDEX 01 00:00:53\n"
    );
    File::create(&cue_path)
        .unwrap()
        .write_all(cue.as_bytes())
        .unwrap();

    let status = Command::new(&chdman)
        .arg("createcd")
        .arg("-i")
        .arg(&cue_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", mnemonic])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createcd (multitrack) failed");
    let reference = std::fs::read(&ref_path).unwrap();

    cd::create_from_cue(
        &cue_path,
        &ours_path,
        CdCreateOptions {
            codecs,
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createcd multitrack size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(
        ours, reference,
        "createcd multitrack bytes differ from chdman"
    );

    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(&cue_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

#[test]
fn createcd_multitrack_cdlz_bit_exact_vs_chdman() {
    assert_createcd_multitrack_bit_exact("cdlz", [crate::CHD_CODEC_CD_LZMA, 0, 0, 0]);
}

/// **createcd** from a flat `.iso` (no CUE): chd-rs `cd::create_from_iso` infers a single
/// MODE1/2048 track from the 2048-multiple size and must match `chdman createcd` on the same file.
#[test]
fn createcd_iso_cdlz_bit_exact_vs_chdman() {
    use crate::cd::{self, CdCreateOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir();
    let iso_path = dir.join("chdrs_createcd_iso.iso");
    let ours_path = dir.join("chdrs_createcd_iso_ours.chd");
    let ref_path = dir.join("chdrs_createcd_iso_ref.chd");

    // 100 cooked MODE1/2048 sectors.
    let iso = make_input(2048 * 100);
    File::create(&iso_path).unwrap().write_all(&iso).unwrap();

    let status = Command::new(&chdman)
        .arg("createcd")
        .arg("-i")
        .arg(&iso_path)
        .arg("-o")
        .arg(&ref_path)
        .args(["-c", "cdlz"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(status.success(), "chdman createcd (iso) failed");
    let reference = std::fs::read(&ref_path).unwrap();

    cd::create_from_iso(
        &iso_path,
        &ours_path,
        CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_LZMA, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    let ours = std::fs::read(&ours_path).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createcd iso size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(ours, reference, "createcd iso bytes differ from chdman");

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&ours_path);
    let _ = std::fs::remove_file(&ref_path);
}

/// **extractcd** byte-identity: build a CD CHD (chdman `createcd`), extract it with both chdman
/// (`extractcd -o out.cue -ob out.bin`) and chd-rs (`cd::extract_to_cue`), and assert the emitted
/// CUE **and** BIN are byte-identical — plus that the extracted BIN equals the original source BIN
/// (full create→extract round-trip). `ref`/`ours` go in sibling dirs so the cue's `FILE "out.bin"`
/// line matches.
fn assert_extractcd_bit_exact(name: &str, src_bin: &[u8], cue_text: &str) {
    let chdman = chdman_path();
    let base = std::env::temp_dir();
    let srcdir = base.join(format!("chdrs_ecd_src_{name}"));
    let refdir = base.join(format!("chdrs_ecd_ref_{name}"));
    let oursdir = base.join(format!("chdrs_ecd_ours_{name}"));
    for d in [&srcdir, &refdir, &oursdir] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d).unwrap();
    }

    std::fs::write(srcdir.join("src.bin"), src_bin).unwrap();
    std::fs::write(srcdir.join("src.cue"), cue_text.as_bytes()).unwrap();
    let src_chd = srcdir.join("src.chd");

    let s = Command::new(&chdman)
        .arg("createcd")
        .args(["-i".as_ref(), srcdir.join("src.cue").as_os_str()])
        .args(["-o".as_ref(), src_chd.as_os_str()])
        .args(["-c", "cdlz"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "createcd ({name}) failed");

    let s = Command::new(&chdman)
        .arg("extractcd")
        .args(["-i".as_ref(), src_chd.as_os_str()])
        .args(["-o".as_ref(), refdir.join("out.cue").as_os_str()])
        .args(["-ob".as_ref(), refdir.join("out.bin").as_os_str()])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "extractcd ({name}) failed");

    crate::cd::extract_to_cue(
        &src_chd,
        &oursdir.join("out.cue"),
        &oursdir.join("out.bin"),
        &mut |_| {},
    )
    .unwrap();

    let ref_cue = std::fs::read(refdir.join("out.cue")).unwrap();
    let ours_cue = std::fs::read(oursdir.join("out.cue")).unwrap();
    assert_eq!(
        ours_cue,
        ref_cue,
        "extractcd cue differs from chdman ({name})\n--- ours ---\n{}\n--- chdman ---\n{}",
        String::from_utf8_lossy(&ours_cue),
        String::from_utf8_lossy(&ref_cue),
    );

    let ref_bin = std::fs::read(refdir.join("out.bin")).unwrap();
    let ours_bin = std::fs::read(oursdir.join("out.bin")).unwrap();
    assert_eq!(
        ours_bin.len(),
        ref_bin.len(),
        "extractcd bin size differs ({name}): ours={}, chdman={}",
        ours_bin.len(),
        ref_bin.len()
    );
    assert_eq!(
        ours_bin, ref_bin,
        "extractcd bin differs from chdman ({name})"
    );
    assert_eq!(ours_bin, src_bin, "extractcd bin != source bin ({name})");

    for d in [&srcdir, &refdir, &oursdir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn extractcd_single_mode1_bit_exact_vs_chdman() {
    let bin = build_mode1_bin(50);
    let cue = "FILE \"src.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n";
    assert_extractcd_bit_exact("mode1", &bin, cue);
}

#[test]
fn extractcd_multitrack_bit_exact_vs_chdman() {
    let mut bin = build_mode1_bin(50);
    bin.extend_from_slice(&make_audio_input(30 * 2352));
    let cue = "FILE \"src.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  \
               TRACK 02 AUDIO\n    INDEX 00 00:00:50\n    INDEX 01 00:00:53\n";
    assert_extractcd_bit_exact("multi", &bin, cue);
}

/// `list_tracks` reads back the `CHT2` records: the multi-track CD's two tracks with the right
/// types, frame counts, and the audio track's in-file pregap.
#[test]
fn list_tracks_reads_cht2() {
    use crate::cd::{SubcodeType, TrackType};

    let dir = std::env::temp_dir();
    let srcdir = dir.join("chdrs_listtracks");
    let _ = std::fs::remove_dir_all(&srcdir);
    std::fs::create_dir_all(&srcdir).unwrap();
    let mut bin = build_mode1_bin(50);
    bin.extend_from_slice(&make_audio_input(30 * 2352));
    std::fs::write(srcdir.join("src.bin"), &bin).unwrap();
    std::fs::write(
        srcdir.join("src.cue"),
        "FILE \"src.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  \
         TRACK 02 AUDIO\n    INDEX 00 00:00:50\n    INDEX 01 00:00:53\n",
    )
    .unwrap();
    let chd_path = srcdir.join("src.chd");
    crate::cd::create_from_cue(
        &srcdir.join("src.cue"),
        &chd_path,
        crate::cd::CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_LZMA, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();

    let mut chd = Chd::open(BufReader::new(File::open(&chd_path).unwrap()), None).unwrap();
    let tracks = crate::cd::list_tracks(&mut chd).unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].track_num, 1);
    assert_eq!(tracks[0].track_type, TrackType::Mode1Raw);
    assert_eq!(tracks[0].frames, 50);
    assert_eq!(tracks[0].pregap, 0);
    assert_eq!(tracks[1].track_num, 2);
    assert_eq!(tracks[1].track_type, TrackType::Audio);
    assert_eq!(tracks[1].subcode_type, SubcodeType::None);
    assert_eq!(tracks[1].frames, 30);
    assert_eq!(tracks[1].pregap, 3);

    let _ = std::fs::remove_dir_all(&srcdir);
}

/// `extract_to_iso` + `CdCookedReader` on a cooked MODE1/2048 CD: the round-trip iso →
/// `create_from_iso` → `extract_to_iso` reproduces the input exactly, and `CdCookedReader` streams
/// the same bytes (incl. a mid-stream seek).
#[test]
fn cooked_iso_roundtrip_mode1_2048() {
    use crate::cd::{self, CdCookedReader, CdCreateOptions};
    use std::io::{Read, Seek, SeekFrom};

    let dir = std::env::temp_dir();
    let iso_in = dir.join("chdrs_cooked_2048.iso");
    let chd = dir.join("chdrs_cooked_2048.chd");
    let iso_out = dir.join("chdrs_cooked_2048_out.iso");

    let data = make_input(2048 * 100); // 100 cooked MODE1/2048 sectors
    File::create(&iso_in).unwrap().write_all(&data).unwrap();
    cd::create_from_iso(
        &iso_in,
        &chd,
        CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_ZLIB, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();

    cd::extract_to_iso(&chd, &iso_out, &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read(&iso_out).unwrap(),
        data,
        "extract_to_iso (MODE1/2048) round-trip mismatch"
    );

    // CdCookedReader streams the same bytes, and a seek lands on the right sector.
    let opened = Chd::open(BufReader::new(File::open(&chd).unwrap()), None).unwrap();
    let mut r = CdCookedReader::open(opened).unwrap();
    assert_eq!(r.len(), data.len() as u64);
    let mut got = Vec::new();
    r.read_to_end(&mut got).unwrap();
    assert_eq!(got, data, "CdCookedReader full read mismatch");
    r.seek(SeekFrom::Start(2048 * 5)).unwrap();
    let mut sector5 = vec![0u8; 2048];
    r.read_exact(&mut sector5).unwrap();
    assert_eq!(
        sector5,
        data[2048 * 5..2048 * 6],
        "CdCookedReader seek mismatch"
    );

    for p in [&iso_in, &chd, &iso_out] {
        let _ = std::fs::remove_file(p);
    }
}

/// `extract_to_iso` on a raw MODE1/2352 CD yields the 2048-byte cooked user data (sync header +
/// ECC/EDC stripped): sector `s`'s output equals `src[s*2352 + 16 ..][..2048]`.
#[test]
fn cooked_iso_strips_mode1_raw() {
    use crate::cd::{self, CdCreateOptions};

    let dir = std::env::temp_dir();
    let bin = dir.join("chdrs_cooked_raw.bin");
    let cue = dir.join("chdrs_cooked_raw.cue");
    let chd = dir.join("chdrs_cooked_raw.chd");
    let iso_out = dir.join("chdrs_cooked_raw_out.iso");

    let nsectors = 40usize;
    let src = build_mode1_bin(nsectors);
    File::create(&bin).unwrap().write_all(&src).unwrap();
    let bin_name = bin.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        &cue,
        format!("FILE \"{bin_name}\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n"),
    )
    .unwrap();
    cd::create_from_cue(
        &cue,
        &chd,
        CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_ZLIB, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();

    cd::extract_to_iso(&chd, &iso_out, &mut |_| {}).unwrap();
    let out = std::fs::read(&iso_out).unwrap();
    assert_eq!(out.len(), nsectors * 2048);
    for s in 0..nsectors {
        assert_eq!(
            &out[s * 2048..(s + 1) * 2048],
            &src[s * 2352 + 16..s * 2352 + 16 + 2048],
            "cooked user data mismatch at sector {s}"
        );
    }

    for p in [&bin, &cue, &chd, &iso_out] {
        let _ = std::fs::remove_file(p);
    }
}

/// Full **createcd from a `.gdi`** byte-identity (GD-ROM): a 3-track Dreamcast index (data / audio /
/// data) with small inter-track LBA gaps → per-track `padframes`. chd-rs `cd::create_from_gdi` must
/// equal `chdman createcd -c cdlz` byte-for-byte — exercising the GDI parser, the `padframes`
/// zero-fill in the logical assembly, the audio byte-swap, and the `CHGD` (GD-ROM) metadata records.
#[test]
fn createcd_gdi_bit_exact_vs_chdman() {
    use crate::cd::{self, CdCreateOptions};

    let chdman = chdman_path();
    let dir = std::env::temp_dir().join("chdrs_gdi");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // track1 data @ LBA 0 (90 frames), track2 audio @ LBA 100 (140 frames), track3 data @ LBA 250
    // (50 frames). Gaps: track1 +10 pad (ends 90, next @ 100), track2 +10 pad (ends 240, next @ 250).
    std::fs::write(dir.join("t1.bin"), build_mode1_bin(90)).unwrap();
    std::fs::write(dir.join("t2.raw"), make_audio_input(140 * 2352)).unwrap();
    std::fs::write(dir.join("t3.bin"), build_mode1_bin(50)).unwrap();
    let gdi = "3\n1 0 4 2352 \"t1.bin\" 0\n2 100 0 2352 \"t2.raw\" 0\n3 250 4 2352 \"t3.bin\" 0\n";
    std::fs::write(dir.join("disc.gdi"), gdi).unwrap();

    let ref_chd = dir.join("ref.chd");
    let ours_chd = dir.join("ours.chd");

    let s = Command::new(&chdman)
        .arg("createcd")
        .args(["-i".as_ref(), dir.join("disc.gdi").as_os_str()])
        .args(["-o".as_ref(), ref_chd.as_os_str()])
        .args(["-c", "cdlz"])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman createcd (gdi) failed");
    let reference = std::fs::read(&ref_chd).unwrap();

    cd::create_from_gdi(
        &dir.join("disc.gdi"),
        &ours_chd,
        CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_LZMA, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();
    let ours = std::fs::read(&ours_chd).unwrap();

    assert_eq!(
        ours.len(),
        reference.len(),
        "createcd gdi size differs: ours={}, chdman={}",
        ours.len(),
        reference.len()
    );
    assert_eq!(ours, reference, "createcd gdi bytes differ from chdman");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Full **extractcd → `.gdi`** byte-identity (GD-ROM): build a GD-ROM CHD (via `create_from_gdi`),
/// then extract it to a `.gdi` + split track files with both chdman (`extractcd -o disc.gdi`) and
/// chd-rs (`cd::extract_to_gdi`); assert the `.gdi` index **and** every per-track file are
/// byte-identical, and that the data/audio track files round-trip to the originals.
#[test]
fn extractcd_gdi_bit_exact_vs_chdman() {
    use crate::cd::{self, CdCreateOptions};

    let chdman = chdman_path();
    let base = std::env::temp_dir();
    let srcdir = base.join("chdrs_egdi_src");
    let refdir = base.join("chdrs_egdi_ref");
    let oursdir = base.join("chdrs_egdi_ours");
    for d in [&srcdir, &refdir, &oursdir] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d).unwrap();
    }

    let t1 = build_mode1_bin(90);
    let t2 = make_audio_input(140 * 2352);
    let t3 = build_mode1_bin(50);
    std::fs::write(srcdir.join("t1.bin"), &t1).unwrap();
    std::fs::write(srcdir.join("t2.raw"), &t2).unwrap();
    std::fs::write(srcdir.join("t3.bin"), &t3).unwrap();
    std::fs::write(
        srcdir.join("disc.gdi"),
        "3\n1 0 4 2352 \"t1.bin\" 0\n2 100 0 2352 \"t2.raw\" 0\n3 250 4 2352 \"t3.bin\" 0\n",
    )
    .unwrap();
    let chd = srcdir.join("disc.chd");
    cd::create_from_gdi(
        &srcdir.join("disc.gdi"),
        &chd,
        CdCreateOptions {
            codecs: [crate::CHD_CODEC_CD_LZMA, 0, 0, 0],
            ..Default::default()
        },
        &mut |_p| {},
        &|| false,
    )
    .unwrap();

    let s = Command::new(&chdman)
        .arg("extractcd")
        .args(["-i".as_ref(), chd.as_os_str()])
        .args(["-o".as_ref(), refdir.join("disc.gdi").as_os_str()])
        .arg("-f")
        .status()
        .expect("failed to run chdman");
    assert!(s.success(), "chdman extractcd (gdi) failed");

    cd::extract_to_gdi(&chd, &oursdir.join("disc.gdi"), &mut |_| {}).unwrap();

    // .gdi index + each split track file must match chdman byte-for-byte.
    for name in ["disc.gdi", "disc01.bin", "disc02.raw", "disc03.bin"] {
        let r = std::fs::read(refdir.join(name)).unwrap();
        let o = std::fs::read(oursdir.join(name)).unwrap();
        assert_eq!(o, r, "extractcd gdi: file {name} differs from chdman");
    }
    // round-trip: the data/audio track files reproduce the originals.
    assert_eq!(std::fs::read(oursdir.join("disc01.bin")).unwrap(), t1);
    assert_eq!(std::fs::read(oursdir.join("disc02.raw")).unwrap(), t2);
    assert_eq!(std::fs::read(oursdir.join("disc03.bin")).unwrap(), t3);

    for d in [&srcdir, &refdir, &oursdir] {
        let _ = std::fs::remove_dir_all(d);
    }
}
