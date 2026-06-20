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
