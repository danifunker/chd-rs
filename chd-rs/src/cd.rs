//! CD-ROM CHDs — chdman `createcd` parity.
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
//! Matches libchdman-rs's `cd` module. Extraction (`extractcd`), GDI/Nero parsing, `list_tracks`,
//! and the `cdfl` encoder are not yet implemented.

use crate::error::{Error, Result};
use crate::{write, CompressionProgress, CHD_CODEC_CD_FLAC, CHD_CODEC_CD_LZMA, CHD_CODEC_CD_ZLIB};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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
        if t.frames > 0 {
            let mut f = File::open(&t.fname)?;
            f.seek(SeekFrom::Start(t.offset))?;
            let mut block = vec![0u8; t.frames as usize * bpf];
            f.read_exact(&mut block)?;
            for fr in 0..t.frames as usize {
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

/// Build the per-track `CHT2` metadata payloads (`CDROM_TRACK_METADATA2_FORMAT`, `chd.cpp:39` +
/// `write_metadata`, `cdrom.cpp:1065`). Each payload is the formatted C string **plus its NUL
/// terminator** (chdman stores `string.length() + 1` bytes). `PGTYPE` is the pregap type string,
/// prefixed with `V` when the pregap carries data (`pgdatasize > 0`).
fn build_cht2_payloads(tracks: &[CdTrack]) -> Vec<Vec<u8>> {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let submode = if t.pgdatasize > 0 {
                format!("V{}", t.pgtype.type_string())
            } else {
                t.pgtype.type_string().to_string()
            };
            let s = format!(
                "TRACK:{} TYPE:{} SUBTYPE:{} FRAMES:{} PREGAP:{} PGTYPE:{} PGSUB:{} POSTGAP:{}",
                i + 1,
                t.trktype.type_string(),
                t.subtype.subtype_string(),
                t.frames,
                t.pregap,
                submode,
                t.pgsub.subtype_string(),
                t.postgap,
            );
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
    /// **Note:** the `cdfl` encoder is not yet implemented, so the default set currently fails in
    /// [`create_from_cue`]/[`create_from_iso`]; pass an explicit working set such as
    /// `[CHD_CODEC_CD_LZMA, CHD_CODEC_CD_ZLIB, 0, 0]` until `cdfl` lands.
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

/// Shared back end for [`create_from_cue`]/[`create_from_iso`]: assemble the logical image + `CHT2`
/// metadata from `tracks` and hand off to the V5 writer, **byte-identical to `chdman createcd`**.
fn build_cd<W: Write + Seek>(
    mut tracks: Vec<CdTrack>,
    out: &mut W,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    if opts.hunk_size == 0 || opts.hunk_size % CD_FRAME_SIZE != 0 {
        return Err(Error::InvalidParameter);
    }
    let logical = assemble_logical(&mut tracks)?;
    let payloads = build_cht2_payloads(&tracks);
    let entries: Vec<write::MetaEntry> = payloads
        .iter()
        .map(|p| write::MetaEntry {
            tag: crate::make_tag(b"CHT2"),
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
    create_to_path(tracks, out_path, opts, progress, cancel)
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
    create_to_path(tracks, out_path, opts, progress, cancel)
}

fn create_to_path(
    tracks: Vec<CdTrack>,
    out_path: &Path,
    opts: CdCreateOptions,
    progress: &mut dyn FnMut(CompressionProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let mut out = File::create(out_path)?;
    let res = build_cd(tracks, &mut out, opts, progress, cancel);
    if res.is_err() {
        drop(out);
        let _ = std::fs::remove_file(out_path);
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
