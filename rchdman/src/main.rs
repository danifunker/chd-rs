use anyhow::anyhow;
use chd::header::{CodecType, Header};
use chd::iter::LendingIterator;
use chd::map::{CompressionTypeLegacy, CompressionTypeV5, MapEntry};
use chd::metadata::Metadata;
use chd::Chd;
use clap::{Parser, Subcommand};
use num_traits::cast::FromPrimitive;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use thousands::Separable;

/// Parse a chdman-style `-c` compression spec (e.g. `"lzma,zlib"` or `"none"`).
fn parse_comp(spec: &str) -> anyhow::Result<[u32; 4]> {
    chd::parse_codec_spec(spec).map_err(|_| anyhow!("invalid compression spec: {spec}"))
}

/// Refuse to clobber an existing output unless `--force`.
fn check_overwrite(out: &Path, force: bool) -> anyhow::Result<()> {
    if !force && out.exists() {
        return Err(anyhow!(
            "Output file already exists (use --force to overwrite): {}",
            out.display()
        ));
    }
    Ok(())
}

/// Per-hunk progress line for the create/copy commands.
fn print_progress(p: chd::CompressionProgress) {
    if p.bytes_total > 0 {
        print!(
            "\rCompressing, {:5.1}% complete... (ratio={:.1}%)",
            100.0 * p.bytes_done as f64 / p.bytes_total as f64,
            100.0 * p.ratio
        );
        let _ = std::io::stdout().flush();
    }
}

fn validate_file_exists(s: &OsStr) -> Result<PathBuf, std::io::Error> {
    let path = PathBuf::from(s);
    if path.exists() && path.is_file() {
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "File not found or not a file.",
    ))
}

fn try_fourcc_to_u32(s: &str) -> anyhow::Result<u32> {
    const fn make_tag(a: &[u8; 4]) -> u32 {
        ((a[0] as u32) << 24) | ((a[1] as u32) << 16) | ((a[2] as u32) << 8) | (a[3] as u32)
    }

    let s = s.as_bytes();
    let tag = [
        s.get(0).map_or(b' ', |f| *f),
        s.get(1).map_or(b' ', |f| *f),
        s.get(2).map_or(b' ', |f| *f),
        s.get(3).map_or(b' ', |f| *f),
    ];

    Ok(make_tag(&tag))
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
#[clap(propagate_version = true)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Displays information about a CHD
    Info {
        /// input file name
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,

        /// output additional information
        #[clap(short, long)]
        verbose: bool,
    },
    /// Benchmark chd-rs
    Benchmark {
        /// input file name
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        /// parent file name for input CHD
        #[clap(short = 'p', long, parse(try_from_os_str = validate_file_exists))]
        inputparent: Option<PathBuf>,
    },
    /// Verifies the integrity of a CHD
    Verify {
        /// input file name
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        /// parent file name for input CHD
        #[clap(short = 'p', long, parse(try_from_os_str = validate_file_exists))]
        inputparent: Option<PathBuf>,
    },
    /// Dump metadata from the CHD to stdout or to a file
    Dumpmeta {
        /// input file name
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        /// output file name
        #[clap(short, long)]
        output: Option<PathBuf>,
        /// force overwriting an existing file
        #[clap(short, long)]
        force: bool,
        /// 4-character tag for metadata
        #[clap(short, long, parse(try_from_str = try_fourcc_to_u32))]
        tag: u32,
        #[clap(short = 'x', long, default_value = "0")]
        index: u32,
    },
    /// Extract raw file from a CHD input file
    Extractraw {
        /// output file name
        #[clap(short, long)]
        output: PathBuf,
        /// force overwriting an existing file
        #[clap(short, long)]
        force: bool,
        /// input file name
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        /// parent file name for input CHD
        #[clap(short = 'p', long, parse(try_from_os_str = validate_file_exists))]
        inputparent: Option<PathBuf>,
    },
    /// Create a raw CHD from the input file
    Createraw {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        /// parent CHD for the output (creates a compressed child/diff)
        #[clap(long = "outputparent", short = 'p')]
        outputparent: Option<PathBuf>,
        #[clap(long = "hunksize", default_value = "4096")]
        hunksize: u32,
        #[clap(long = "unitsize", default_value = "512")]
        unitsize: u32,
        #[clap(short = 'c', long, default_value = "zlib")]
        compression: String,
        #[clap(short, long)]
        force: bool,
    },
    /// Create a hard-disk CHD from the input file
    Createhd {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        #[clap(long = "hunksize", default_value = "4096")]
        hunksize: u32,
        #[clap(long = "unitsize", default_value = "512")]
        unitsize: u32,
        #[clap(short = 'c', long, default_value = "lzma,zlib,huff,flac")]
        compression: String,
        #[clap(short, long)]
        force: bool,
    },
    /// Create a CD CHD from a CUE / GDI / ISO input
    Createcd {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        #[clap(short = 'c', long, default_value = "cdlz,cdzl,cdfl")]
        compression: String,
        #[clap(short, long)]
        force: bool,
    },
    /// Create a DVD CHD from the input ISO
    Createdvd {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        #[clap(short = 'c', long, default_value = "lzma,zlib,huff,flac")]
        compression: String,
        #[clap(short, long)]
        force: bool,
    },
    /// Copy a CHD, optionally recompressing / re-hunking
    Copy {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        #[clap(long = "hunksize")]
        hunksize: Option<u32>,
        #[clap(short = 'c', long, default_value = "zlib")]
        compression: String,
        #[clap(short, long)]
        force: bool,
    },
    /// Add a metadata item to a CHD
    Addmeta {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long, parse(try_from_str = try_fourcc_to_u32))]
        tag: u32,
        #[clap(short = 'x', long, default_value = "0")]
        index: u32,
        /// text value (a trailing NUL is appended, as chdman does)
        #[clap(long = "valuetext", short = 'v')]
        valuetext: Option<String>,
        /// file whose raw contents become the value
        #[clap(long = "valuefile", short = 'b')]
        valuefile: Option<PathBuf>,
        /// do not flag the entry as checksummed
        #[clap(long = "nochecksum", short = 'n')]
        nochecksum: bool,
    },
    /// Delete a metadata item from a CHD
    Delmeta {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long, parse(try_from_str = try_fourcc_to_u32))]
        tag: u32,
        #[clap(short = 'x', long, default_value = "0")]
        index: u32,
    },
    /// Extract a CD CHD to a CUE/GDI + binary track file(s)
    Extractcd {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        /// output bin filename (default: the output name with a .bin extension)
        #[clap(long = "outputbin", short = 'b')]
        outputbin: Option<PathBuf>,
        #[clap(short, long)]
        force: bool,
    },
    /// Extract a DVD CHD to an ISO
    Extractdvd {
        #[clap(short, long, parse(try_from_os_str = validate_file_exists))]
        input: PathBuf,
        #[clap(short, long)]
        output: PathBuf,
        #[clap(short, long)]
        force: bool,
    },
}

fn info(input: &PathBuf, verbose: bool) -> anyhow::Result<()> {
    fn get_file_version(chd: &Header) -> usize {
        match chd {
            Header::V1Header(_) => 1,
            Header::V2Header(_) => 2,
            Header::V3Header(_) => 3,
            Header::V4Header(_) => 4,
            Header::V5Header(_) => 5,
        }
    }

    fn print_hash(header: &Header) {
        match header {
            Header::V1Header(h) | Header::V2Header(h) => {
                println!("MD5:\t\t{}", hex::encode(h.md5));
                if header.has_parent() {
                    println!("Parent MD5:\t{}", hex::encode(h.parent_md5));
                }
            }
            Header::V3Header(h) => {
                println!("MD5:\t\t{}", hex::encode(h.md5));
                if header.has_parent() {
                    println!("Parent MD5:\t{}", hex::encode(h.parent_md5));
                }
                println!("SHA1:\t\t{}", hex::encode(h.sha1));
                if header.has_parent() {
                    println!("Parent SHA1:\t{}", hex::encode(h.parent_sha1));
                }
            }
            Header::V4Header(h) => {
                println!("SHA1:\t\t{}", hex::encode(h.sha1));
                if header.has_parent() {
                    println!("Parent SHA1:\t{}", hex::encode(h.parent_sha1));
                }
            }
            Header::V5Header(h) => {
                println!("SHA1:\t\t{}", hex::encode(h.sha1));
                println!("Data SHA1:\t{}", hex::encode(h.raw_sha1));
                if header.has_parent() {
                    println!("Parent SHA1:\t{}", hex::encode(h.parent_sha1));
                }
            }
        }
    }

    fn codec_name(ty: CodecType) -> &'static str {
        match ty {
            CodecType::None => "Copy from self",
            CodecType::Zlib => "Legacy zlib (Deflate)",
            CodecType::ZlibPlus => "Legacy zlib+ (Deflate)",
            CodecType::AV => "Legacy A/V",
            CodecType::ZLibV5 => "Deflate",
            CodecType::ZLibCdV5 => "CD Deflate",
            CodecType::LzmaCdV5 => "CD LZMA",
            CodecType::FlacCdV5 => "CD FLAC",
            CodecType::FlacV5 => "FLAC",
            CodecType::LzmaV5 => "LZMA",
            CodecType::AVHuffV5 => "A/V Huffman",
            CodecType::HuffV5 => "Huffman",
            CodecType::ZstdV5 => "Zstandard",
            CodecType::ZstdCdV5 => "CD Zstandard",
        }
    }

    fn print_compression(header: &Header) {
        fn to_chdman_compression_name(ty: CodecType) -> &'static str {
            match ty {
                CodecType::None => "none",
                CodecType::Zlib => "Legacy zlib (Deflate)",
                CodecType::ZlibPlus => "Legacy zlib+ (Deflate)",
                CodecType::AV => "Legacy av (AV)",
                CodecType::ZLibV5 => "zlib (Deflate)",
                CodecType::ZLibCdV5 => "cdzl (CD Deflate)",
                CodecType::LzmaCdV5 => "cdlz (CD LZMA)",
                CodecType::FlacCdV5 => "cdfl (CD FLAC)",
                CodecType::FlacV5 => "flac (FLAC)",
                CodecType::LzmaV5 => "lzma (LZMA)",
                CodecType::AVHuffV5 => "avhu (A/V Huffman)",
                CodecType::HuffV5 => "huff (Huffman)",
                CodecType::ZstdV5 => "zstd (Zstandard)",
                CodecType::ZstdCdV5 => "cdzs (CD Zstandard)",
            }
        }

        print!("Compression:\t");
        if !header.is_compressed() {
            println!("none");
            return;
        }

        match header {
            Header::V1Header(h) | Header::V2Header(h) => {
                println!(
                    "{}",
                    to_chdman_compression_name(CodecType::from_u32(h.compression).unwrap())
                );
            }
            Header::V3Header(h) => {
                println!(
                    "{}",
                    to_chdman_compression_name(CodecType::from_u32(h.compression).unwrap())
                );
            }
            Header::V4Header(h) => {
                println!(
                    "{}",
                    to_chdman_compression_name(CodecType::from_u32(h.compression).unwrap())
                );
            }
            Header::V5Header(h) => {
                for compression in h.compression {
                    if compression == 0 {
                        break;
                    }
                    print!(
                        "{}, ",
                        to_chdman_compression_name(CodecType::from_u32(compression).unwrap())
                    );
                }
                println!();
            }
        }
    }

    fn to_fourcc(fourcc: u32) -> anyhow::Result<[char; 4]> {
        let parts = [
            (fourcc >> 24) & 0xff,
            (fourcc >> 16) & 0xff,
            (fourcc >> 8) & 0xff,
            fourcc & 0xff,
        ];
        let res = parts.map(char::from_u32);
        if res.iter().any(|f| f.is_none()) {
            return Err(anyhow!("unable to parse"));
        }
        Ok(res.map(Option::unwrap))
    }

    fn print_verbose<F: Seek + Read>(chd: &Chd<F>) -> anyhow::Result<()> {
        // can only have 4 comptypes.
        // first four is for the four comp types.
        // next four is NONE, SELF, PARENT, MINI, UNKNOWN
        let mut hunk_count = [0u64; 9];

        let num_hunks = chd.map().len();
        println!();
        println!("     Hunks  Percent  Name");
        println!("----------  -------  ------------------------------------");

        for i in 0..num_hunks {
            let hunk = chd.map().get_entry(i).unwrap();
            match hunk {
                MapEntry::V5Compressed(c) => match c.hunk_type()? {
                    CompressionTypeV5::CompressionType0 => {
                        hunk_count[0] += 1;
                    }
                    CompressionTypeV5::CompressionType1 => {
                        hunk_count[1] += 1;
                    }
                    CompressionTypeV5::CompressionType2 => {
                        hunk_count[2] += 1;
                    }
                    CompressionTypeV5::CompressionType3 => {
                        hunk_count[3] += 1;
                    }
                    CompressionTypeV5::CompressionNone => {
                        hunk_count[4] += 1;
                    }
                    CompressionTypeV5::CompressionSelf
                    | CompressionTypeV5::CompressionSelf0
                    | CompressionTypeV5::CompressionSelf1 => {
                        hunk_count[5] += 1;
                    }
                    CompressionTypeV5::CompressionParent
                    | CompressionTypeV5::CompressionParentSelf
                    | CompressionTypeV5::CompressionParent0
                    | CompressionTypeV5::CompressionParent1 => {}
                    _ => {
                        hunk_count[6] += 1;
                    }
                },
                MapEntry::V5Uncompressed(_) => {
                    hunk_count[4] += 1;
                }
                MapEntry::LegacyEntry(c) => {
                    match c.hunk_type()? {
                        CompressionTypeLegacy::Invalid => {}
                        CompressionTypeLegacy::Compressed => {
                            hunk_count[0] += 1;
                        }
                        CompressionTypeLegacy::Uncompressed => {
                            hunk_count[4] += 1;
                        }
                        CompressionTypeLegacy::Mini => {
                            hunk_count[7] += 1;
                        }
                        CompressionTypeLegacy::SelfHunk => {
                            hunk_count[5] += 1;
                        }
                        CompressionTypeLegacy::ParentHunk => {
                            hunk_count[6] += 1;
                        }
                        CompressionTypeLegacy::ExternalCompressed => {
                            // not sure this is valid.
                            hunk_count[8] += 1;
                        }
                    }
                }
            }
        }

        let results: Vec<(u64, f64, &'static str)> = hunk_count
            .iter()
            .enumerate()
            .map(|(i, count)| {
                let percent = *count as f64 / num_hunks as f64;
                let name = match i {
                    4 => "Uncompressed",
                    5 => "Copy from self",
                    6 => "Copy from parent",
                    7 => "Legacy 8-byte mini",
                    8 => "Unknown",
                    i => codec_name(
                        CodecType::from_u32(match chd.header() {
                            Header::V1Header(h) => h.compression,
                            Header::V2Header(h) => h.compression,
                            Header::V3Header(h) => h.compression,
                            Header::V4Header(h) => h.compression,
                            Header::V5Header(h) => h.compression[i],
                        })
                        .unwrap(),
                    ),
                };
                (*count, percent, name)
            })
            .collect();

        for (count, percent, name) in &results[4..] {
            if *count == 0u64 {
                continue;
            }
            println!(
                "{:>10}   {:>5.1}%  {:<40}",
                count.separate_with_commas(),
                100f64 * percent,
                name
            );
        }

        for (count, percent, name) in &results[..4] {
            if *count == 0u64 {
                continue;
            }
            println!(
                "{:>10}   {:>5.1}%  {:<40}",
                count.separate_with_commas(),
                100f64 * percent,
                name
            );
        }

        Ok(())
    }

    println!("\nchd-rs - rchdman info");
    let mut f = File::open(input)?;
    let fsize = f.metadata()?.len();
    let mut chd = Chd::open(&mut f, None)?;
    println!("Input file:\t{}", input.display());
    println!("File Version:\t{}", get_file_version(chd.header()));
    println!(
        "Logical size:\t{} bytes",
        chd.header().logical_bytes().separate_with_commas()
    );
    println!(
        "Hunk Size:\t{} bytes",
        chd.header().hunk_size().separate_with_commas()
    );
    println!(
        "Total Hunks:\t{}",
        chd.header().hunk_count().separate_with_commas()
    );
    println!(
        "Unit Size:\t{} bytes",
        chd.header().unit_bytes().separate_with_commas()
    );
    println!(
        "Total Units:\t{}",
        chd.header().unit_count().separate_with_commas()
    );
    print_compression(chd.header());
    println!("CHD size:\t{} bytes", fsize.separate_with_commas());

    if chd.header().is_compressed() {
        println!(
            "Ratio:\t\t{:.1}%",
            100.0 * fsize as f64 / chd.header().logical_bytes() as f64
        );
    }

    // hash
    print_hash(chd.header());

    if let Ok(metadata) = Vec::<Metadata>::try_from(chd.metadata_refs()) {
        for meta in metadata {
            let tag = to_fourcc(meta.metatag);
            if let Ok(tag) = tag {
                println!(
                    "Metadata:\tTag='{}'  Index={}  Length={} bytes",
                    tag.iter().collect::<String>(),
                    meta.index,
                    meta.length
                );
            } else {
                println!(
                    "Metadata:\tTag={:0x}  Index={}  Length={} bytes",
                    meta.metatag, meta.index, meta.length
                );
            }
            print!("              \t");
            println!(
                "{}",
                meta.value
                    .iter()
                    .map(|u| {
                        if u.is_ascii_alphanumeric()
                            || u.is_ascii_whitespace()
                            || u.is_ascii_punctuation()
                        {
                            *u as char
                        } else {
                            '.'
                        }
                    })
                    .collect::<String>()
            );
        }
    }

    if verbose {
        print_verbose(&chd)?;
    }

    Ok(())
}

fn benchmark(p: impl AsRef<Path>, ip: Option<impl AsRef<Path>>) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman benchmark");
    let f = BufReader::new(File::open(p)?);
    let ipf = ip.map(|ip| BufReader::new(File::open(ip).unwrap()));

    let start = Instant::now();
    let ipchd = ipf.map(|ipf| Chd::open(ipf, None));

    let mut chd = if let Some(ip) = ipchd {
        Chd::open(f, Some(Box::new(ip?)))?
    } else {
        Chd::open(f, None)?
    };

    let mut hunk_buf = chd.get_hunksized_buffer();
    let mut cmp_buf = Vec::new();
    let hunk_iter = chd.hunks();
    let mut bytes = 0;
    let mut hunk_num = 0;

    hunk_iter.for_each(|mut hunk| {
        bytes += hunk
            .read_hunk_in(&mut cmp_buf, &mut hunk_buf)
            .unwrap_or_else(|_| panic!("could not read_hunk {}", hunk_num));
        hunk_num += 1;
    });

    let time = Instant::now().saturating_duration_since(start);
    println!(
        "Read {} bytes ({} hunks) in {} seconds",
        bytes,
        hunk_num,
        time.as_secs_f64()
    );
    println!(
        "Rate is {} MB/s",
        (bytes / (1024 * 1024)) as f64 / time.as_secs_f64()
    );

    Ok(())
}

fn verify(input: impl AsRef<Path>, inputparent: Option<impl AsRef<Path>>) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman verify");
    let f = BufReader::new(File::open(input)?);

    let p = if let Some(parent) = inputparent {
        let f = BufReader::new(File::open(parent)?);
        let parent_chd = Chd::open(f, None)?;
        Some(Box::new(parent_chd))
    } else {
        None
    };

    let mut chd = Chd::open(f, p)?;

    if !chd.header().is_compressed() {
        return Err(anyhow!("No verification to be done; CHD is uncompressed"));
    }

    // Full verification: raw (data) SHA-1 over the logical bytes + the metadata-inclusive overall.
    let r = chd.verify()?;

    if r.raw_sha1_valid() {
        println!("Raw SHA1 verification successful!");
    } else {
        eprintln!(
            "Error: Raw SHA1 in header = {}\n              actual SHA1 = {}",
            hex::encode(r.expected_raw_sha1),
            hex::encode(r.computed_raw_sha1)
        );
    }
    if r.overall_sha1_valid() {
        println!("Overall SHA1 verification successful!");
    } else {
        eprintln!(
            "Error: Overall SHA1 in header = {}\n                  actual SHA1 = {}",
            hex::encode(r.expected_sha1),
            hex::encode(r.computed_sha1)
        );
    }
    Ok(())
}

fn createraw(
    input: &Path,
    output: &Path,
    outputparent: Option<&PathBuf>,
    hunksize: u32,
    unitsize: u32,
    compression: &str,
    force: bool,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman createraw");
    check_overwrite(output, force)?;
    let opts = chd::hd::HdCreateOptions {
        hunk_size: hunksize,
        unit_size: unitsize,
        codecs: parse_comp(compression)?,
        ..Default::default()
    };
    if let Some(parent) = outputparent {
        chd::hd::create_raw_from_path_with_parent(
            input,
            output,
            parent,
            opts,
            &mut print_progress,
            &|| false,
        )?;
    } else {
        chd::hd::create_raw_from_path(input, output, opts, &mut print_progress, &|| false)?;
    }
    println!("\nCompression complete");
    Ok(())
}

fn createhd(
    input: &Path,
    output: &Path,
    hunksize: u32,
    unitsize: u32,
    compression: &str,
    force: bool,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman createhd");
    check_overwrite(output, force)?;
    let opts = chd::hd::HdCreateOptions {
        hunk_size: hunksize,
        unit_size: unitsize,
        codecs: parse_comp(compression)?,
        ..Default::default()
    };
    chd::hd::create_from_path(input, output, opts, &mut print_progress, &|| false)?;
    println!("\nCompression complete");
    Ok(())
}

fn createcd(input: &Path, output: &Path, compression: &str, force: bool) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman createcd");
    check_overwrite(output, force)?;
    let opts = chd::cd::CdCreateOptions {
        codecs: parse_comp(compression)?,
        ..Default::default()
    };
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "cue" => chd::cd::create_from_cue(input, output, opts, &mut print_progress, &|| false)?,
        "gdi" => chd::cd::create_from_gdi(input, output, opts, &mut print_progress, &|| false)?,
        _ => chd::cd::create_from_iso(input, output, opts, &mut print_progress, &|| false)?,
    }
    println!("\nCompression complete");
    Ok(())
}

fn createdvd(input: &Path, output: &Path, compression: &str, force: bool) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman createdvd");
    check_overwrite(output, force)?;
    let opts = chd::dvd::DvdCreateOptions {
        codecs: parse_comp(compression)?,
        ..Default::default()
    };
    chd::dvd::create_from_iso(input, output, opts, &mut print_progress, &|| false)?;
    println!("\nCompression complete");
    Ok(())
}

fn copy(
    input: &Path,
    output: &Path,
    hunksize: Option<u32>,
    compression: &str,
    force: bool,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman copy");
    check_overwrite(output, force)?;
    let opts = chd::copy::CopyOptions {
        hunk_size: hunksize,
        codecs: parse_comp(compression)?,
    };
    chd::copy::copy(input, output, opts, &mut print_progress, &|| false)?;
    println!("\nCompression complete");
    Ok(())
}

fn addmeta(
    input: &Path,
    tag: u32,
    index: u32,
    valuetext: Option<&String>,
    valuefile: Option<&PathBuf>,
    nochecksum: bool,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman addmeta");
    let data = if let Some(text) = valuetext {
        let mut b = text.clone().into_bytes();
        b.push(0); // chdman stores the C string's NUL terminator
        b
    } else if let Some(file) = valuefile {
        std::fs::read(file)?
    } else {
        return Err(anyhow!("either --valuetext or --valuefile is required"));
    };
    let flags = if nochecksum {
        0
    } else {
        chd::metadata::METADATA_FLAG_CHECKSUM
    };
    let mut file = OpenOptions::new().read(true).write(true).open(input)?;
    chd::metadata::write_metadata(&mut file, tag, index, &data, flags)?;
    println!("Metadata added");
    Ok(())
}

fn delmeta(input: &Path, tag: u32, index: u32) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman delmeta");
    let mut file = OpenOptions::new().read(true).write(true).open(input)?;
    chd::metadata::delete_metadata(&mut file, tag, index)?;
    println!("Metadata deleted");
    Ok(())
}

fn extractcd(
    input: &Path,
    output: &Path,
    outputbin: Option<&PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman extractcd");
    check_overwrite(output, force)?;
    let is_gdi = output
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gdi"))
        .unwrap_or(false);
    if is_gdi {
        chd::cd::extract_to_gdi(input, output, &mut |_| {})?;
    } else {
        let bin = outputbin
            .cloned()
            .unwrap_or_else(|| output.with_extension("bin"));
        check_overwrite(&bin, force)?;
        chd::cd::extract_to_cue(input, output, &bin, &mut |_| {})?;
    }
    println!("Extraction complete");
    Ok(())
}

fn extractdvd(input: &Path, output: &Path, force: bool) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman extractdvd");
    check_overwrite(output, force)?;
    chd::dvd::extract_to_iso(input, output, &mut |_| {})?;
    println!("Extraction complete");
    Ok(())
}

fn dumpmeta(
    input: impl AsRef<Path>,
    output: Option<&PathBuf>,
    force: bool,
    tag: u32,
    index: u32,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman dumpmeta");

    let mut f = BufReader::new(File::open(input)?);
    let mut chd = Chd::open(&mut f, None)?;

    let metas: Vec<Metadata> = chd.metadata_refs().try_into()?;
    let tag = metas
        .iter()
        .find(|p| p.metatag == tag && p.index == index)
        .ok_or_else(|| anyhow!("Error reading metadata: can't find metadata"))?;

    if let Some(output) = output {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(!force)
            .create(true)
            .truncate(true)
            .open(output)?;
        file.write_all(&*tag.value)?;
        println!("File ({}) written, {} bytes", output.display(), tag.length)
    } else {
        println!("{}", String::from_utf8_lossy(&*tag.value));
    }
    Ok(())
}

fn extractraw(
    input: &PathBuf,
    inputparent: Option<impl AsRef<Path>>,
    output: &PathBuf,
    force: bool,
) -> anyhow::Result<()> {
    println!("\nchd-rs - rchdman extractraw");
    let mut output_file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(!force)
            .create(true)
            .truncate(true)
            .open(output)?,
    );

    println!("Output File:  {}", output.display());
    println!("Input CHD:    {}", input.display());

    let f = BufReader::new(File::open(input)?);

    let p = if let Some(parent) = inputparent {
        let f = BufReader::new(File::open(parent)?);
        let parent_chd = Chd::open(f, None)?;
        Some(Box::new(parent_chd))
    } else {
        None
    };

    let mut chd = Chd::open(f, p)?;
    let mut cmp_buf = Vec::new();
    let mut out_buf = chd.get_hunksized_buffer();
    let mut hunk_iter = chd.hunks();
    while let Some(mut hunk) = hunk_iter.next() {
        hunk.read_hunk_in(&mut cmp_buf, &mut out_buf)?;
        output_file.write_all(&out_buf)?;
    }
    println!("Extraction complete");
    output_file.flush()?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Info { input, verbose } => info(input, *verbose)?,
        Commands::Benchmark { input, inputparent } => benchmark(input, inputparent.as_ref())?,
        Commands::Verify { input, inputparent } => verify(input, inputparent.as_deref())?,
        Commands::Dumpmeta {
            input,
            output,
            force,
            tag,
            index,
        } => dumpmeta(input, output.as_ref(), *force, *tag, *index)?,
        Commands::Extractraw {
            input,
            inputparent,
            force,
            output,
        } => extractraw(input, inputparent.as_deref(), output, *force)?,
        Commands::Createraw {
            input,
            output,
            outputparent,
            hunksize,
            unitsize,
            compression,
            force,
        } => createraw(
            input,
            output,
            outputparent.as_ref(),
            *hunksize,
            *unitsize,
            compression,
            *force,
        )?,
        Commands::Createhd {
            input,
            output,
            hunksize,
            unitsize,
            compression,
            force,
        } => createhd(input, output, *hunksize, *unitsize, compression, *force)?,
        Commands::Createcd {
            input,
            output,
            compression,
            force,
        } => createcd(input, output, compression, *force)?,
        Commands::Createdvd {
            input,
            output,
            compression,
            force,
        } => createdvd(input, output, compression, *force)?,
        Commands::Copy {
            input,
            output,
            hunksize,
            compression,
            force,
        } => copy(input, output, *hunksize, compression, *force)?,
        Commands::Addmeta {
            input,
            tag,
            index,
            valuetext,
            valuefile,
            nochecksum,
        } => addmeta(
            input,
            *tag,
            *index,
            valuetext.as_ref(),
            valuefile.as_ref(),
            *nochecksum,
        )?,
        Commands::Delmeta { input, tag, index } => delmeta(input, *tag, *index)?,
        Commands::Extractcd {
            input,
            output,
            outputbin,
            force,
        } => extractcd(input, output, outputbin.as_ref(), *force)?,
        Commands::Extractdvd {
            input,
            output,
            force,
        } => extractdvd(input, output, *force)?,
    }
    Ok(())
}
