//! CD-ROM CHDs — chdman `createcd` / `extractcd` parity.
//!
//! A CD CHD stores the disc as fixed **2448-byte frames** (`2352` sector + `96` subcode), eight
//! frames to a `19584`-byte hunk, compressed with the CD wrapper codecs (`cdlz`/`cdzl`/`cdfl`/`cdzs`
//! — see [`compression::cdrom`](crate::compression)). Each track gets a `CHT2`
//! ([`CDROM_TRACK_METADATA2`](crate::metadata::KnownMetadata::CdRomTrack2)) record describing its
//! type/subtype/frame-count.
//!
//! Creation is a pure-Rust port of chdman's `do_create_cd` (`chdman.cpp:2162`): parse the input TOC
//! (a CUE sheet via [`create_from_cue`] or a flat image via [`create_from_iso`]), assemble the
//! logical frame image (port of `chd_cd_compressor::read_data`), build the per-track `CHT2`
//! metadata, then reuse the shared V5 writer ([`write::write_create`](crate::write)). Output is
//! **byte-identical to `chdman createcd`** (verified for `-c cdzl`/`cdlz`).
//!
//! [`extract_to_cue`] reverses this (chdman `extractcd`, byte-identical), [`list_tracks`] reads the
//! track table back, and [`extract_to_iso`] / [`CdCookedReader`] expose a single MODE1 track's
//! cooked 2048-byte user data.
//!
//! [`create_from_gdi`] handles Sega Dreamcast `.gdi` indices (GD-ROM, `CHGD` metadata).
//!
//! Matches libchdman-rs's `cd` module. All four CD codecs (`cdlz`/`cdzl`/`cdzs`/`cdfl`) encode.
//! Nero (`.nrg`) TOC parsing and `.gdi`/split-bin extraction are not yet implemented.

use crate::error::{Error, Result};
use crate::metadata::Metadata;
use crate::read::ChdReader;
use crate::{
    write, Chd, CompressionProgress, CHD_CODEC_CD_FLAC, CHD_CODEC_CD_LZMA, CHD_CODEC_CD_ZLIB,
};
use std::convert::TryInto;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Cooked MODE1/MODE2-Form1 user-data size (`2048`).
const COOKED_SECTOR: usize = 2048;

/// Bytes of sector data in a CD frame (`2352`).
pub const CD_MAX_SECTOR_DATA: u32 = crate::cdrom::CD_MAX_SECTOR_DATA;
/// Bytes of subcode data in a CD frame (`96`).
pub const CD_MAX_SUBCODE_DATA: u32 = crate::cdrom::CD_MAX_SUBCODE_DATA;
/// Total CD frame size in bytes (`2352 + 96 = 2448`). This is the CD CHD's unit size.
pub const CD_FRAME_SIZE: u32 = crate::cdrom::CD_FRAME_SIZE;
/// CD frames per hunk (`8`).
pub const FRAMES_PER_HUNK: u32 = 8;
/// chdman's default CD hunk size (`8 * 2448 = 19584`).
pub const DEFAULT_HUNK_SIZE: u32 = FRAMES_PER_HUNK * CD_FRAME_SIZE;
/// Tracks are padded to a multiple of this many frames (`4`).
pub const TRACK_PADDING: u32 = 4;

/// CD-ROM track type. Mirrors MAME's `CD_TRACK_*` enum (`cdrom.h`); the discriminant order matches
/// so the `MODE1`-valued default (`pgtype`) and the metadata strings line up with chdman.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TrackType {
    /// `MODE1`, 2048 data bytes/sector (cooked).
    Mode1 = 0,
    /// `MODE1_RAW`, 2352 data bytes/sector.
    Mode1Raw,
    /// `MODE2`, 2336 data bytes/sector.
    Mode2,
    /// `MODE2_FORM1`, 2048 data bytes/sector.
    Mode2Form1,
    /// `MODE2_FORM2`, 2324 data bytes/sector.
    Mode2Form2,
    /// `MODE2_FORM_MIX`, 2336 data bytes/sector.
    Mode2FormMix,
    /// `MODE2_RAW`, 2352 data bytes/sector.
    Mode2Raw,
    /// `AUDIO`, 2352 bytes/sector (Red Book).
    Audio,
}

impl TrackType {
    /// The metadata string for this type (`get_type_string`, `cdrom.cpp:835`).
    pub fn type_string(self) -> &'static str {
        match self {
            TrackType::Mode1 => "MODE1",
            TrackType::Mode1Raw => "MODE1_RAW",
            TrackType::Mode2 => "MODE2",
            TrackType::Mode2Form1 => "MODE2_FORM1",
            TrackType::Mode2Form2 => "MODE2_FORM2",
            TrackType::Mode2FormMix => "MODE2_FORM_MIX",
            TrackType::Mode2Raw => "MODE2_RAW",
            TrackType::Audio => "AUDIO",
        }
    }
}

/// CD-ROM subcode type. Mirrors MAME's `CD_SUB_*` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SubcodeType {
    /// `RW`, cooked 96 bytes/sector.
    Normal = 0,
    /// `RW_RAW`, raw uninterleaved 96 bytes/sector.
    Raw,
    /// `NONE`, no subcode stored.
    None,
}

impl SubcodeType {
    /// The metadata string for this subtype (`get_subtype_string`, `cdrom.cpp:867`).
    pub fn subtype_string(self) -> &'static str {
        match self {
            SubcodeType::Normal => "RW",
            SubcodeType::Raw => "RW_RAW",
            SubcodeType::None => "NONE",
        }
    }
}

/// Per-track summary read back from a CD CHD's metadata (or a parsed TOC). Matches libchdman-rs's
/// `TrackInfo`. Returned by [`list_tracks`].
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// 1-based track number.
    pub track_num: u32,
    /// Track data type.
    pub track_type: TrackType,
    /// Subcode type.
    pub subcode_type: SubcodeType,
    /// Number of frames (sectors) in the track.
    pub frames: u32,
    /// Pregap length in frames.
    pub pregap: u32,
    /// Postgap length in frames.
    pub postgap: u32,
    /// Pregap sector type (`MODE1` unless the pregap carries data).
    pub pregap_type: TrackType,
    /// Pregap subcode type.
    pub pregap_subcode: SubcodeType,
}

/// Map a subcode-type string to its [`SubcodeType`] (`RW`/`RW_RAW`, else `NONE`), as chdman's
/// `convert_subtype_string_to_track_info` (`cdrom.cpp:777`).
fn subtype_from_string(s: &str) -> SubcodeType {
    match s {
        "RW" => SubcodeType::Normal,
        "RW_RAW" => SubcodeType::Raw,
        _ => SubcodeType::None,
    }
}

/// Map a CUE/TOC track-type string to `(type, data size)`, exactly as chdman's
/// `get_info_from_type_string` (`cdrom.cpp:643`).
fn type_from_string(s: &str) -> Option<(TrackType, u32)> {
    Some(match s {
        "MODE1" | "MODE1/2048" => (TrackType::Mode1, 2048),
        "MODE1_RAW" | "MODE1/2352" => (TrackType::Mode1Raw, 2352),
        "MODE2" | "MODE2/2336" => (TrackType::Mode2, 2336),
        "MODE2_FORM1" | "MODE2/2048" => (TrackType::Mode2Form1, 2048),
        "MODE2_FORM2" | "MODE2/2324" => (TrackType::Mode2Form2, 2324),
        "MODE2_FORM_MIX" => (TrackType::Mode2FormMix, 2336),
        "MODE2_RAW" | "MODE2/2352" | "CDI/2352" => (TrackType::Mode2Raw, 2352),
        "AUDIO" => (TrackType::Audio, 2352),
        _ => return None,
    })
}

/// One parsed TOC track plus the input-file binding needed to assemble its frames. Combines the
/// fields chdman keeps split across `cdrom_file::track_info` + `track_input_entry`.
#[derive(Debug, Clone)]
struct CdTrack {
    trktype: TrackType,
    subtype: SubcodeType,
    datasize: u32,
    subsize: u32,
    frames: u32,
    extraframes: u32,
    /// Trailing zero-padding frames inside `frames` (GDI area-gap fill); not read from the source.
    padframes: u32,
    pregap: u32,
    postgap: u32,
    pgtype: TrackType,
    pgsub: SubcodeType,
    pgdatasize: u32,
    // input binding
    fname: PathBuf,
    offset: u64,
    swap: bool,
    idx0: i64,
    idx1: i64,
}

impl CdTrack {
    fn new(trktype: TrackType, datasize: u32, fname: PathBuf, swap: bool) -> Self {
        CdTrack {
            trktype,
            subtype: SubcodeType::None,
            datasize,
            subsize: 0,
            frames: 0,
            extraframes: 0,
            padframes: 0,
            pregap: 0,
            postgap: 0,
            // pgtype defaults to MODE1 (chdman's zero-initialised `pgtype`), pgsub to NONE.
            pgtype: TrackType::Mode1,
            pgsub: SubcodeType::None,
            pgdatasize: 0,
            fname,
            offset: 0,
            swap,
            idx0: -1,
            idx1: -1,
        }
    }
}

/// Split a CUE line into tokens, honouring single/double quotes (port of `cdrom_file::tokenize`,
/// `cdrom.cpp:1524`): leading whitespace is skipped, quote characters toggle quoting and are
/// dropped, and unquoted whitespace ends a token. Assumes ASCII (CUE keywords + paths).
fn tokenize_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let mut tok = String::new();
        let mut singlequote = false;
        let mut doublequote = false;
        while i < n {
            let c = bytes[i];
            if !singlequote && c == b'"' {
                doublequote = !doublequote;
            } else if !doublequote && c == b'\'' {
                singlequote = !singlequote;
            } else if !singlequote && !doublequote && c.is_ascii_whitespace() {
                break;
            } else {
                tok.push(c as char);
            }
            i += 1;
        }
        tokens.push(tok);
    }
    tokens
}

/// Convert an `m:s:f` (or bare-frame) token to a frame count (port of `msf_to_frames`,
/// `cdrom.cpp:1578`). A token with no `:` is taken as a raw frame count; otherwise it is
/// `(m*60 + s)*75 + f`.
fn msf_to_frames(token: &str) -> i64 {
    let parts: Vec<&str> = token.split(':').collect();
    let m: i64 = parts
        .first()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if parts.len() == 1 {
        return m;
    }
    let s: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let f: i64 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    (m * 60 + s) * 75 + f
}

/// Parse a BIN/CUE sheet into a track list, a pure-Rust port of `cdrom_file::parse_cue`
/// (`cdrom.cpp:2336`) covering the common single- and multi-file cases (`FILE`/`TRACK`/`INDEX`/
/// `PREGAP`/`POSTGAP`/`FLAGS`). GD-ROM (`is_gdicue`), multisession, lead-in/out, and `WAVE` inputs
/// are not supported.
fn parse_cue(cue_path: &Path) -> Result<Vec<CdTrack>> {
    let text = std::fs::read_to_string(cue_path)?;
    let dir = cue_path.parent().unwrap_or_else(|| Path::new(""));

    let mut tracks: Vec<CdTrack> = Vec::new();
    let mut lastfname: Option<PathBuf> = None;
    // The FILE type sets the byte-swap flag of the *next* track only (chdman's
    // `outinfo.track[trknum+1].swap`); consumed when that track is defined.
    let mut next_swap: Option<bool> = None;

    for line in text.lines() {
        let tokens = tokenize_line(line);
        let Some(cmd) = tokens.first() else { continue };
        match cmd.as_str() {
            "FILE" => {
                let name = tokens.get(1).ok_or(Error::InvalidData)?;
                lastfname = Some(dir.join(name));
                match tokens.get(2).map(String::as_str).unwrap_or("") {
                    "BINARY" => next_swap = Some(false),
                    "MOTOROLA" => next_swap = Some(true),
                    // WAVE needs sample parsing; other types are unknown.
                    _ => return Err(Error::UnsupportedFormat),
                }
            }
            "TRACK" => {
                let typestr = tokens.get(2).ok_or(Error::InvalidData)?;
                let (trktype, datasize) =
                    type_from_string(typestr).ok_or(Error::UnsupportedFormat)?;
                let fname = lastfname.clone().ok_or(Error::InvalidData)?;
                let swap = next_swap.take().unwrap_or(false);
                let mut t = CdTrack::new(trktype, datasize, fname, swap);
                // optional subtype token (RW / RW_RAW); anything else leaves SUBTYPE:NONE.
                if let Some(sub) = tokens.get(3) {
                    match sub.as_str() {
                        "RW" => {
                            t.subtype = SubcodeType::Normal;
                            t.subsize = 96;
                        }
                        "RW_RAW" => {
                            t.subtype = SubcodeType::Raw;
                            t.subsize = 96;
                        }
                        _ => {}
                    }
                }
                tracks.push(t);
            }
            "INDEX" => {
                let t = tracks.last_mut().ok_or(Error::InvalidData)?;
                let idx: i64 = tokens
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .ok_or(Error::InvalidData)?;
                let frames = msf_to_frames(tokens.get(2).ok_or(Error::InvalidData)?);
                if idx == 0 {
                    t.idx0 = frames;
                } else if idx == 1 {
                    t.idx1 = frames;
                }
                if idx == 1 {
                    if t.pregap == 0 && t.idx0 != -1 {
                        t.pregap = (t.idx1 - t.idx0) as u32;
                        t.pgtype = t.trktype;
                        t.pgdatasize = t.datasize;
                    } else if t.idx0 == -1 {
                        // pregap not physically present; idx 0 is used for the length calc.
                        t.idx0 = frames;
                    }
                }
            }
            "PREGAP" => {
                let t = tracks.last_mut().ok_or(Error::InvalidData)?;
                t.pregap = msf_to_frames(tokens.get(1).ok_or(Error::InvalidData)?) as u32;
            }
            "POSTGAP" => {
                let t = tracks.last_mut().ok_or(Error::InvalidData)?;
                t.postgap = msf_to_frames(tokens.get(1).ok_or(Error::InvalidData)?) as u32;
            }
            // FLAGS/REM and anything else: no effect on the created CHD.
            _ => {}
        }
    }

    if tracks.is_empty() {
        return Err(Error::InvalidData);
    }

    compute_track_lengths(&mut tracks)?;
    Ok(tracks)
}

/// Second pass of `parse_cue` (`cdrom.cpp:2656`): fill in each track's `frames` and source-file
/// `offset` from the index markers and the bin file sizes.
fn compute_track_lengths(tracks: &mut [CdTrack]) -> Result<()> {
    let numtrks = tracks.len();
    for i in 0..numtrks {
        if tracks[i].idx1 == -1 {
            // INDEX 01 is required.
            return Err(Error::InvalidData);
        }
        if tracks[i].trktype == TrackType::Audio {
            tracks[i].swap = true;
        }
        if tracks[i].offset != 0 {
            continue;
        }

        // Snapshot neighbour values (already finalised for i-1; parse-time for i+1).
        let prev_same = i > 0 && tracks[i].fname == tracks[i - 1].fname;
        let next_same = i + 1 < numtrks && tracks[i].fname == tracks[i + 1].fname;
        let prev_offset = if i > 0 { tracks[i - 1].offset } else { 0 };
        let prev_frames = if i > 0 { tracks[i - 1].frames } else { 0 };
        let prev_bpf = if i > 0 {
            (tracks[i - 1].datasize + tracks[i - 1].subsize) as u64
        } else {
            0
        };
        let next_idx0 = if i + 1 < numtrks {
            tracks[i + 1].idx0
        } else {
            0
        };
        let this_idx0 = tracks[i].idx0;
        let bpf = (tracks[i].datasize + tracks[i].subsize) as u64;

        if i + 1 >= numtrks && i > 0 && prev_same {
            // last track sharing the previous track's file
            let tlen = file_size(&tracks[i].fname)?;
            let offset = prev_offset + prev_frames as u64 * prev_bpf;
            tracks[i].offset = offset;
            tracks[i].frames = ((tlen - offset) / bpf) as u32;
        } else if next_same {
            // same file as the next track: length is the gap between the two index-0 markers
            let frames = next_idx0 - this_idx0;
            if frames <= 0 {
                return Err(Error::InvalidData);
            }
            tracks[i].frames = frames as u32;
            if i > 0 {
                tracks[i].offset = prev_offset + prev_frames as u64 * prev_bpf;
            }
        } else if tracks[i].frames == 0 {
            // a file of its own
            let tlen = file_size(&tracks[i].fname)?;
            tracks[i].frames = (tlen / bpf) as u32;
            tracks[i].offset = 0;
        }
    }
    Ok(())
}

/// Parse a flat sector image (`.iso`/`.bin`) into a single-track TOC, a port of
/// `cdrom_file::parse_iso` (`cdrom.cpp:2027`): the track type is inferred from the file size's
/// divisibility (2048 → `MODE1`, 2336 → `MODE2`, 2352 → `MODE2_RAW`).
fn parse_iso(iso_path: &Path) -> Result<Vec<CdTrack>> {
    let size = file_size(iso_path)?;
    let (trktype, datasize) = if size % 2048 == 0 {
        (TrackType::Mode1, 2048)
    } else if size % 2336 == 0 {
        (TrackType::Mode2, 2336)
    } else if size % 2352 == 0 {
        (TrackType::Mode2Raw, 2352)
    } else {
        return Err(Error::UnsupportedFormat);
    };
    let mut t = CdTrack::new(trktype, datasize, iso_path.to_path_buf(), false);
    t.frames = (size / datasize as u64) as u32;
    t.idx0 = 0;
    t.idx1 = 0;
    Ok(vec![t])
}

fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Assemble the logical frame image from the parsed TOC — a port of `chd_cd_compressor::read_data`
/// (`chdman.cpp:437`). Each track occupies `(frames + extraframes) * 2448` bytes; for every one of
/// its `frames` data frames, the source's `datasize + subsize` bytes are read contiguously from
/// `offset` and placed at the start of the 2448-byte frame (the remainder staying zero), byte-pair
/// swapped over the first 2352 bytes when the track is flagged `swap` (Red Book audio / `MOTOROLA`).
/// The `extraframes` padding frames are left zero. Returns the image plus the per-track frame
/// counts (`frames`, used for the `CHT2` records).
fn assemble_logical(tracks: &mut [CdTrack]) -> Result<Vec<u8>> {
    let mut total_frames: u64 = 0;
    for t in tracks.iter_mut() {
        let padded = t.frames.div_ceil(TRACK_PADDING) * TRACK_PADDING;
        t.extraframes = padded - t.frames;
        total_frames += (t.frames + t.extraframes) as u64;
    }

    let frame_size = CD_FRAME_SIZE as usize;
    let sector_data = CD_MAX_SECTOR_DATA as usize;
    let mut logical = vec![0u8; total_frames as usize * frame_size];

    let mut dest_frame: u64 = 0;
    for t in tracks.iter() {
        let bpf = (t.datasize + t.subsize) as usize;
        // The trailing `padframes` (GDI area-gap fill) and `extraframes` (4-frame alignment) have no
        // source data — only `frames - padframes` real sectors are read; the rest stay zero.
        let real = t.frames.saturating_sub(t.padframes) as usize;
        if real > 0 {
            let mut f = File::open(&t.fname)?;
            f.seek(SeekFrom::Start(t.offset))?;
            let mut block = vec![0u8; real * bpf];
            f.read_exact(&mut block)?;
            for fr in 0..real {
                let dst = (dest_frame as usize + fr) * frame_size;
                logical[dst..dst + bpf].copy_from_slice(&block[fr * bpf..(fr + 1) * bpf]);
                if t.swap {
                    let sector = &mut logical[dst..dst + sector_data];
                    let mut k = 0;
                    while k + 1 < sector_data {
                        sector.swap(k, k + 1);
                        k += 2;
                    }
                }
            }
        }
        dest_frame += (t.frames + t.extraframes) as u64;
    }
    Ok(logical)
}

/// Build the per-track metadata payloads (`write_metadata`, `cdrom.cpp:1065`). For a CD this is the
/// `CHT2` `CDROM_TRACK_METADATA2_FORMAT` (`chd.cpp:39`) where `PGTYPE` is `V`-prefixed when the
/// pregap carries data; for a GD-ROM (`gdrom`) it's the `CHGD` `GDROM_TRACK_METADATA_FORMAT`
/// (`chd.cpp:40`) which adds a `PAD:` field and uses the plain pregap type string. Each payload is
/// the formatted C string **plus its NUL terminator** (chdman stores `string.length() + 1` bytes).
fn build_track_metadata(tracks: &[CdTrack], gdrom: bool) -> Vec<Vec<u8>> {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let s = if gdrom {
                format!(
                    "TRACK:{} TYPE:{} SUBTYPE:{} FRAMES:{} PAD:{} PREGAP:{} PGTYPE:{} PGSUB:{} POSTGAP:{}",
                    i + 1,
                    t.trktype.type_string(),
                    t.subtype.subtype_string(),
                    t.frames,
                    t.padframes,
                    t.pregap,
                    t.pgtype.type_string(),
                    t.pgsub.subtype_string(),
                    t.postgap,
                )
            } else {
                let submode = if t.pgdatasize > 0 {
                    format!("V{}", t.pgtype.type_string())
                } else {
                    t.pgtype.type_string().to_string()
                };
                format!(
                    "TRACK:{} TYPE:{} SUBTYPE:{} FRAMES:{} PREGAP:{} PGTYPE:{} PGSUB:{} POSTGAP:{}",
                    i + 1,
                    t.trktype.type_string(),
                    t.subtype.subtype_string(),
                    t.frames,
                    t.pregap,
                    submode,
                    t.pgsub.subtype_string(),
                    t.postgap,
                )
            };
            let mut bytes = s.into_bytes();
            bytes.push(0); // C-string NUL terminator, included in the stored length
            bytes
        })
        .collect()
}

/// Options for CD CHD creation. Matches libchdman-rs's `CdCreateOptions`.
#[derive(Debug, Clone)]
pub struct CdCreateOptions {
    /// Hunk size in bytes. Default `19584` (`8 * 2448`). Must be a non-zero multiple of `2448`.
    pub hunk_size: u32,
    /// Codec slots. Default `[cdlz, cdzl, cdfl, 0]` (chdman's `s_default_cd_compression`).
    ///
    /// All three are implemented and byte-identical to chdman, with one caveat: `cdfl`'s FLAC stream
    /// is **libm-gated** (byte-identical to a *glibc* chdman, round-trip-correct against any build —
    /// see [`CdFlacEncoder`](crate::compression)). The `cdlz`/`cdzl` hunks are always byte-identical.
    pub codecs: [u32; 4],
}

impl Default for CdCreateOptions {
    fn default() -> Self {
        Self {
            hunk_size: DEFAULT_HUNK_SIZE,
            codecs: [CHD_CODEC_CD_LZMA, CHD_CODEC_CD_ZLIB, CHD_CODEC_CD_FLAC, 0],
        }
    }
}

/// Shared back end for the create paths: assemble the logical image + per-track metadata from
/// `tracks` and hand off to the V5 writer, **byte-identical to `chdman createcd`**. `gdrom` selects
/// the `CHGD` (GD-ROM) vs `CHT2` (CD) metadata record.
fn build_cd<W: Write + Seek>(
    mut tracks: Vec<CdTrack>,
    gdrom: bool,
    out: &mut W,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    if opts.hunk_size == 0 || opts.hunk_size % CD_FRAME_SIZE != 0 {
        return Err(Error::InvalidParameter);
    }
    let logical = assemble_logical(&mut tracks)?;
    let payloads = build_track_metadata(&tracks, gdrom);
    let tag = if gdrom {
        crate::make_tag(b"CHGD")
    } else {
        crate::make_tag(b"CHT2")
    };
    let entries: Vec<write::MetaEntry> = payloads
        .iter()
        .map(|p| write::MetaEntry {
            tag,
            flags: write::CHD_MDFLAGS_CHECKSUM,
            payload: p,
        })
        .collect();

    let codecs = write::resolve_codecs(&opts.codecs)?;
    write::write_create(
        out,
        &logical,
        opts.hunk_size,
        CD_FRAME_SIZE,
        &codecs,
        &entries,
        progress,
        cancel,
    )
}

/// Create a CD CHD from a BIN/CUE sheet at `cue_path`, **byte-identical to `chdman createcd`**.
///
/// Parses the CUE (single- or multi-file BIN, the common `MODE1`/`MODE2`/`AUDIO` track types),
/// assembles the 2448-byte-frame logical image (padding each track to a 4-frame boundary), writes a
/// `CHT2` record per track, and compresses with `opts.codecs`. `progress`/`cancel` follow the usual
/// convention (cancel → [`Error::Cancelled`] before any bytes hit `out`). On any error the partial
/// `out_path` is removed.
///
/// GD-ROM cue sheets, multisession discs, and `WAVE` track inputs are not supported.
pub fn create_from_cue(
    cue_path: &Path,
    out_path: &Path,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let tracks = parse_cue(cue_path)?;
    create_to_path(tracks, false, out_path, opts, progress, cancel)
}

/// Create a CD CHD from a flat sector image (`.iso`/`.bin`) at `iso_path`, **byte-identical to
/// `chdman createcd`**. The single track's type is inferred from the file size (port of
/// `parse_iso`): 2048 → `MODE1`, 2336 → `MODE2`, 2352 → `MODE2_RAW`. See [`create_from_cue`] for
/// the callback/cleanup behaviour.
pub fn create_from_iso(
    iso_path: &Path,
    out_path: &Path,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let tracks = parse_iso(iso_path)?;
    create_to_path(tracks, false, out_path, opts, progress, cancel)
}

/// Create a GD-ROM CHD from a Sega Dreamcast `.gdi` index at `gdi_path`, **byte-identical to
/// `chdman createcd`**. Port of `parse_gdi` (`cdrom.cpp:2115`): each track's frame count comes from
/// its file size, and the gap up to the next track's LBA becomes trailing `padframes` on the
/// previous track (the high-density area split). Writes `CHGD` (GD-ROM) metadata. See
/// [`create_from_cue`] for callback/cleanup behaviour.
pub fn create_from_gdi(
    gdi_path: &Path,
    out_path: &Path,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let tracks = parse_gdi(gdi_path)?;
    create_to_path(tracks, true, out_path, opts, progress, cancel)
}

fn create_to_path(
    tracks: Vec<CdTrack>,
    gdrom: bool,
    out_path: &Path,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let mut out = File::create(out_path)?;
    let res = build_cd(tracks, gdrom, &mut out, opts, progress, cancel);
    if res.is_err() {
        drop(out);
        let _ = std::fs::remove_file(out_path);
    }
    res
}

/// Parse a Sega Dreamcast `.gdi` index into a track list (port of `cdrom_file::parse_gdi`,
/// `cdrom.cpp:2115`). First line is the track count; each subsequent line is
/// `tracknum lba type sectorsize "file" offset` (`type` 4 = data, 0 = audio). Each track's frames
/// come from its file size; the LBA gap to the next track becomes the previous track's `padframes`.
fn parse_gdi(gdi_path: &Path) -> Result<Vec<CdTrack>> {
    let text = std::fs::read_to_string(gdi_path)?;
    let dir = gdi_path.parent().unwrap_or_else(|| Path::new(""));

    let mut lines = text.lines();
    let numtracks: usize = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .and_then(|t| t.parse().ok())
        .ok_or(Error::InvalidData)?;
    if numtracks == 0 {
        return Err(Error::InvalidData);
    }

    let mut slots: Vec<Option<CdTrack>> = (0..numtracks).map(|_| None).collect();
    let mut physframeofs = vec![0u32; numtracks];
    for line in lines {
        let toks = tokenize_line(line);
        if toks.is_empty() {
            continue;
        }
        if toks.len() != 6 {
            return Err(Error::InvalidData);
        }
        let trknum = toks[0]
            .parse::<i64>()
            .ok()
            .filter(|&n| n >= 1)
            .ok_or(Error::InvalidData)? as usize
            - 1;
        if trknum >= numtracks {
            return Err(Error::InvalidData);
        }
        let pfo: u32 = toks[1].parse().map_err(|_| Error::InvalidData)?;
        let trktype: u32 = toks[2].parse().map_err(|_| Error::InvalidData)?;
        let trksize: u32 = toks[3].parse().map_err(|_| Error::InvalidData)?;
        if trksize == 0 {
            return Err(Error::InvalidData);
        }
        let (tt, datasize, swap) = match (trktype, trksize) {
            (4, 2352) => (TrackType::Mode1Raw, 2352, false),
            (4, 2048) => (TrackType::Mode1, 2048, false),
            (0, _) => (TrackType::Audio, 2352, true),
            _ => return Err(Error::UnsupportedFormat),
        };
        let fname = dir.join(&toks[4]);
        let sz = file_size(&fname)?;
        let mut t = CdTrack::new(tt, datasize, fname, swap);
        t.frames = (sz / trksize as u64) as u32;
        physframeofs[trknum] = pfo;
        slots[trknum] = Some(t);
    }

    let mut tracks: Vec<CdTrack> = slots
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(Error::InvalidData)?;

    // The gap between a track's LBA and the previous track's end is padding appended to the
    // previous track (chdman's `frames[trk-1] += dif; padframes[trk-1] = dif`).
    for trk in 1..numtracks {
        let prev_end = tracks[trk - 1].frames as i64 + physframeofs[trk - 1] as i64;
        let dif = physframeofs[trk] as i64 - prev_end;
        if dif < 0 {
            return Err(Error::InvalidData);
        }
        tracks[trk - 1].frames += dif as u32;
        tracks[trk - 1].padframes = dif as u32;
    }

    Ok(tracks)
}

// ---------------------------------------------------------------------------
// extractcd: read a CD CHD's track metadata + reconstruct cue/bin
// ---------------------------------------------------------------------------

/// A track parsed back from a CD CHD's `CHT2`/`CHTR` metadata, with the derived sizes the extractor
/// needs (the read-side analogue of [`CdTrack`]). Port of `cdrom_file::parse_metadata`
/// (`cdrom.cpp:899`) for the modern (`CHT2`) and legacy (`CHTR`) records.
struct TrackMeta {
    trktype: TrackType,
    datasize: u32,
    subtype: SubcodeType,
    frames: u32,
    extraframes: u32,
    pregap: u32,
    postgap: u32,
    pgtype: TrackType,
    pgsub: SubcodeType,
    pgdatasize: u32,
}

/// Parse a `CHT2` (`"TRACK:.. TYPE:.. SUBTYPE:.. FRAMES:.. PREGAP:.. PGTYPE:.. PGSUB:.. POSTGAP:.."`)
/// or legacy `CHTR` (`"TRACK:.. TYPE:.. SUBTYPE:.. FRAMES:.."`) payload. Mirrors
/// `parse_metadata`: the pregap type/subcode are applied only when `pregap > 0` (and `PGTYPE` is
/// `V`-prefixed for a data-bearing pregap); the 4-frame `extraframes` padding is recomputed here.
fn parse_track_metadata(payload: &[u8]) -> Option<TrackMeta> {
    let s = std::str::from_utf8(payload).ok()?;
    let s = s.trim_end_matches('\0');

    let (mut typ, mut subtype, mut pgtype_s, mut pgsub_s) = ("", "", "", "");
    let (mut frames, mut pregap, mut postgap) = (None::<u32>, 0u32, 0u32);
    for tok in s.split_whitespace() {
        let (k, v) = tok.split_once(':')?;
        match k {
            "TYPE" => typ = v,
            "SUBTYPE" => subtype = v,
            "FRAMES" => frames = Some(v.parse().ok()?),
            "PREGAP" => pregap = v.parse().ok()?,
            "PGTYPE" => pgtype_s = v,
            "PGSUB" => pgsub_s = v,
            "POSTGAP" => postgap = v.parse().ok()?,
            _ => {} // TRACK: (we rely on stored order) and anything else
        }
    }
    let frames = frames?;
    let (trktype, datasize) = type_from_string(typ)?;
    let subtype = subtype_from_string(subtype);
    let extraframes = frames.div_ceil(TRACK_PADDING) * TRACK_PADDING - frames;

    // pregap defaults to MODE1/NONE; only a non-zero pregap reads PGTYPE/PGSUB (as parse_metadata).
    let (mut pgtype, mut pgsub, mut pgdatasize) = (TrackType::Mode1, SubcodeType::None, 0u32);
    if pregap > 0 {
        if let Some(stripped) = pgtype_s.strip_prefix('V') {
            if let Some((t, ds)) = type_from_string(stripped) {
                pgtype = t;
                pgdatasize = ds;
            }
        }
        pgsub = subtype_from_string(pgsub_s);
    }

    Some(TrackMeta {
        trktype,
        datasize,
        subtype,
        frames,
        extraframes,
        pregap,
        postgap,
        pgtype,
        pgsub,
        pgdatasize,
    })
}

/// Read all `CHT2`/`CHTR` track records from a CD CHD in stored (track) order.
fn read_track_metas<F: Read + Seek>(chd: &mut Chd<F>) -> Result<Vec<TrackMeta>> {
    let cht2 = crate::make_tag(b"CHT2");
    let chtr = crate::make_tag(b"CHTR");
    let metas: Vec<Metadata> = chd.metadata_refs().try_into()?;
    let mut out = Vec::new();
    for m in &metas {
        if m.metatag == cht2 || m.metatag == chtr {
            out.push(parse_track_metadata(&m.value).ok_or(Error::InvalidData)?);
        }
    }
    if out.is_empty() {
        return Err(Error::UnsupportedFormat);
    }
    Ok(out)
}

/// List a CD CHD's tracks (chdman's `cdrom_file::parse_metadata`), reading the `CHT2`/`CHTR`
/// metadata. Takes `&mut Chd` (chd-rs reads metadata through a mutable borrow) rather than
/// libchdman-rs's `&Chd`.
pub fn list_tracks<F: Read + Seek>(chd: &mut Chd<F>) -> Result<Vec<TrackInfo>> {
    let metas = read_track_metas(chd)?;
    Ok(metas
        .iter()
        .enumerate()
        .map(|(i, t)| TrackInfo {
            track_num: i as u32 + 1,
            track_type: t.trktype,
            subcode_type: t.subtype,
            frames: t.frames,
            pregap: t.pregap,
            postgap: t.postgap,
            pregap_type: t.pgtype,
            pregap_subcode: t.pgsub,
        })
        .collect())
}

/// MAME's `msf_string_from_frames` (`chdman.cpp:1072`): `"%02d:%02d:%02d"` (minutes:seconds:frames,
/// 75 frames/second).
fn msf_string(frames: u32) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        frames / (75 * 60),
        (frames / 75) % 60,
        frames % 75
    )
}

/// Append one track's CUE lines (port of `output_track_metadata`'s `MODE_CUEBIN` branch,
/// `chdman.cpp:1534`). `frameoffs` is the track's disc LBA (cumulative frames, no padding);
/// `outputoffs` is the byte offset in the bin file (the `FILE` line is emitted only at offset 0).
fn append_cue_track(
    cue: &mut String,
    idx: usize,
    t: &TrackMeta,
    frameoffs: u32,
    bin_name: &str,
    outputoffs: u64,
) {
    use std::fmt::Write;
    if outputoffs == 0 {
        let _ = writeln!(cue, "FILE \"{bin_name}\" BINARY");
    }
    let typestr = match t.trktype {
        TrackType::Mode1 | TrackType::Mode1Raw => format!("MODE1/{:04}", t.datasize),
        TrackType::Mode2
        | TrackType::Mode2Form1
        | TrackType::Mode2Form2
        | TrackType::Mode2FormMix
        | TrackType::Mode2Raw => format!("MODE2/{:04}", t.datasize),
        TrackType::Audio => "AUDIO".to_string(),
    };
    let _ = writeln!(cue, "  TRACK {:02} {typestr}", idx + 1);
    if t.pregap > 0 && t.pgdatasize == 0 {
        let _ = writeln!(cue, "    PREGAP {}", msf_string(t.pregap));
        let _ = writeln!(cue, "    INDEX 01 {}", msf_string(frameoffs));
    } else if t.pregap > 0 && t.pgdatasize > 0 {
        let _ = writeln!(cue, "    INDEX 00 {}", msf_string(frameoffs));
        let _ = writeln!(cue, "    INDEX 01 {}", msf_string(frameoffs + t.pregap));
    }
    if t.pregap == 0 {
        let _ = writeln!(cue, "    INDEX 01 {}", msf_string(frameoffs));
    }
    if t.postgap > 0 {
        let _ = writeln!(cue, "    POSTGAP {}", msf_string(t.postgap));
    }
}

/// Extract a CD CHD to a single CUE sheet + BIN (chdman `extractcd`, the `MODE_CUEBIN` non-split
/// path), **byte-identical to `chdman extractcd -o <cue> -ob <bin>`**.
///
/// Reconstructs the combined BIN (each track's `frames` sectors of `datasize` bytes, audio tracks
/// byte-pair-swapped back to little-endian, the 4-frame padding between tracks dropped, subcode
/// omitted) and the CUE (one `FILE` line + per-track `TRACK`/`INDEX`/`PREGAP`/`POSTGAP`, exactly as
/// `output_track_metadata`). `progress` is called with the running BIN byte count.
///
/// GD-ROM, split-bin, and `.gdi`/`.toc` outputs are not yet supported; a track that stored subcode
/// has it silently dropped (as bin/cue cannot represent it).
pub fn extract_to_cue(
    chd_path: &Path,
    cue_path: &Path,
    bin_path: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let f = BufReader::new(File::open(chd_path).map_err(Error::from)?);
    let mut chd = Chd::open(f, None)?;
    let tracks = read_track_metas(&mut chd)?;

    let bin_name = bin_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(Error::InvalidParameter)?
        .to_string();

    let res = (|| -> Result<()> {
        let mut bin = BufWriter::new(File::create(bin_path).map_err(Error::from)?);
        let mut cue = String::new();
        let mut reader = ChdReader::new(chd);
        let mut frame = vec![0u8; CD_FRAME_SIZE as usize];
        let mut discoffs = 0u32; // disc LBA (cumulative frames, no padding) — for the cue
        let mut written = 0u64; // bin byte offset

        for (i, t) in tracks.iter().enumerate() {
            append_cue_track(&mut cue, i, t, discoffs, &bin_name, written);

            let ds = t.datasize as usize;
            for _ in 0..t.frames {
                reader.read_exact(&mut frame).map_err(Error::from)?;
                if t.trktype == TrackType::Audio {
                    let mut k = 0;
                    while k + 1 < ds {
                        frame.swap(k, k + 1);
                        k += 2;
                    }
                }
                bin.write_all(&frame[..ds]).map_err(Error::from)?;
                written += ds as u64;
            }
            // consume (drop) the track's 4-frame-boundary padding so the next track aligns
            for _ in 0..t.extraframes {
                reader.read_exact(&mut frame).map_err(Error::from)?;
            }
            discoffs += t.frames;
            progress(written);
        }

        bin.flush().map_err(Error::from)?;
        // chdman writes the TOC in text mode, so its line endings follow the host platform (CRLF on
        // Windows, LF elsewhere). Match the same-platform chdman for byte-identity.
        #[cfg(windows)]
        let cue = cue.replace('\n', "\r\n");
        std::fs::write(cue_path, cue.as_bytes()).map_err(Error::from)?;
        Ok(())
    })();

    if res.is_err() {
        let _ = std::fs::remove_file(bin_path);
        let _ = std::fs::remove_file(cue_path);
    }
    res
}

/// The cooked-user-data byte offset within a decoded 2448-byte frame for a MODE1 track: `0` for a
/// cooked `MODE1` (2048) track, `16` for a raw `MODE1_RAW` (skip the 12-byte sync + 4-byte header).
/// Other track types have no 2048-byte cooked representation here.
fn cooked_offset(trktype: TrackType) -> Option<usize> {
    match trktype {
        TrackType::Mode1 => Some(0),
        TrackType::Mode1Raw => Some(16),
        _ => None,
    }
}

/// A `Read + Seek` stream over a MODE1 track's **cooked 2048-byte sectors**, so an ISO9660/UDF
/// parser can consume a CD CHD directly without extracting to a `.iso` first. The sync header,
/// address, and ECC/EDC of raw (`MODE1_RAW`) sectors are stripped on the fly, so the stream length
/// is always `frames * 2048` regardless of how the track was stored.
///
/// Matches libchdman-rs's `CdCookedReader`, but wraps chd-rs's owned [`Chd`] (via [`ChdReader`])
/// and currently supports only `MODE1`/`MODE1_RAW` tracks (other types →
/// [`Error::UnsupportedFormat`]).
pub struct CdCookedReader<F: Read + Seek> {
    reader: ChdReader<F>,
    chd_frame_start: u64,
    total_frames: u32,
    cooked_offset: usize,
    pos: u64,
    cache_frame: Option<u32>,
    cache: [u8; COOKED_SECTOR],
}

impl<F: Read + Seek> CdCookedReader<F> {
    /// Open a **single-track** CD CHD as a cooked sector stream. Errors with
    /// [`Error::UnsupportedFormat`] if there is more than one track (use [`open_track`] for those)
    /// or the track is not MODE1.
    ///
    /// [`open_track`]: CdCookedReader::open_track
    pub fn open(mut chd: Chd<F>) -> Result<Self> {
        if read_track_metas(&mut chd)?.len() != 1 {
            return Err(Error::UnsupportedFormat);
        }
        Self::open_track(chd, 0)
    }

    /// Open a specific (0-based) track of a CD CHD as a cooked sector stream. Position 0 is the
    /// start of that track's user data. The track must be `MODE1`/`MODE1_RAW`.
    pub fn open_track(mut chd: Chd<F>, track_index: usize) -> Result<Self> {
        let metas = read_track_metas(&mut chd)?;
        if track_index >= metas.len() {
            return Err(Error::InvalidParameter);
        }
        let cooked_offset =
            cooked_offset(metas[track_index].trktype).ok_or(Error::UnsupportedFormat)?;
        let total_frames = metas[track_index].frames;
        // logical-image frame where this track's data starts (cumulative frames + 4-frame padding)
        let chd_frame_start: u64 = metas[..track_index]
            .iter()
            .map(|m| (m.frames + m.extraframes) as u64)
            .sum();
        Ok(CdCookedReader {
            reader: ChdReader::new(chd),
            chd_frame_start,
            total_frames,
            cooked_offset,
            pos: 0,
            cache_frame: None,
            cache: [0u8; COOKED_SECTOR],
        })
    }

    /// Total length of the cooked stream in bytes (`frames * 2048`).
    pub fn len(&self) -> u64 {
        self.total_frames as u64 * COOKED_SECTOR as u64
    }

    /// Whether the track has no sectors.
    pub fn is_empty(&self) -> bool {
        self.total_frames == 0
    }

    fn load_frame(&mut self, frame: u32) -> io::Result<()> {
        if self.cache_frame == Some(frame) {
            return Ok(());
        }
        let off = (self.chd_frame_start + frame as u64) * CD_FRAME_SIZE as u64;
        self.reader.seek(SeekFrom::Start(off))?;
        let mut full = [0u8; CD_FRAME_SIZE as usize];
        self.reader.read_exact(&mut full)?;
        self.cache
            .copy_from_slice(&full[self.cooked_offset..self.cooked_offset + COOKED_SECTOR]);
        self.cache_frame = Some(frame);
        Ok(())
    }
}

impl<F: Read + Seek> Read for CdCookedReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let total = self.len();
        if self.pos >= total || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(total - self.pos) as usize;
        let frame = (self.pos / COOKED_SECTOR as u64) as u32;
        let off = (self.pos % COOKED_SECTOR as u64) as usize;
        let n = want.min(COOKED_SECTOR - off);
        self.load_frame(frame)?;
        buf[..n].copy_from_slice(&self.cache[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F: Read + Seek> Seek for CdCookedReader<F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let total = self.len() as i128;
        let new_pos: i128 = match pos {
            SeekFrom::Start(v) => v as i128,
            SeekFrom::End(v) => total + v as i128,
            SeekFrom::Current(v) => self.pos as i128 + v as i128,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of cooked stream",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

/// Extract a **single-track** MODE1 CD CHD to a raw `.iso` (2048 cooked bytes per sector), matching
/// libchdman-rs's `extract_to_iso`. Rejects multi-track or non-MODE1 CHDs with
/// [`Error::UnsupportedFormat`]. `progress` is called with the running byte count. On error the
/// partial `iso_path` is removed.
///
/// (chdman has no direct CD→iso command — `extractcd` emits cue/bin or gdi — so this is a chd-rs /
/// libchdman convenience verified by round-trip, not byte-identity.)
pub fn extract_to_iso(
    chd_path: &Path,
    iso_path: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let f = BufReader::new(File::open(chd_path).map_err(Error::from)?);
    let chd = Chd::open(f, None)?;
    let mut reader = CdCookedReader::open(chd)?;

    let res = (|| -> Result<()> {
        let mut out = BufWriter::new(File::create(iso_path).map_err(Error::from)?);
        let mut buf = vec![0u8; COOKED_SECTOR * 16];
        let mut written = 0u64;
        loop {
            let n = reader.read(&mut buf).map_err(Error::from)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(Error::from)?;
            written += n as u64;
            progress(written);
        }
        out.flush().map_err(Error::from)?;
        Ok(())
    })();

    if res.is_err() {
        let _ = std::fs::remove_file(iso_path);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msf_parsing() {
        assert_eq!(msf_to_frames("00:00:00"), 0);
        assert_eq!(msf_to_frames("00:02:00"), 150);
        assert_eq!(msf_to_frames("00:04:00"), 300);
        assert_eq!(msf_to_frames("01:00:00"), 4500);
        assert_eq!(msf_to_frames("150"), 150); // bare frame count
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(
            tokenize_line("  FILE \"my disc.bin\" BINARY"),
            vec!["FILE", "my disc.bin", "BINARY"]
        );
        assert_eq!(
            tokenize_line("    TRACK 01 MODE1/2352"),
            vec!["TRACK", "01", "MODE1/2352"]
        );
    }

    #[test]
    fn type_strings_round_trip() {
        assert_eq!(
            type_from_string("MODE1/2352").unwrap().0,
            TrackType::Mode1Raw
        );
        assert_eq!(TrackType::Mode1Raw.type_string(), "MODE1_RAW");
        assert_eq!(TrackType::Mode1.type_string(), "MODE1"); // default pgtype
        assert_eq!(TrackType::Audio.type_string(), "AUDIO");
        assert_eq!(SubcodeType::None.subtype_string(), "NONE");
    }
}
