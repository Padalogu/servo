/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pure-Rust Media Source Extensions (MSE) streaming segment parsers,
//! W3C `MseTrackBuffer`, coded frame removal/eviction, and RAM quota tracking.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Default RAM quota for a video `SourceBuffer` (100 MiB).
pub const DEFAULT_VIDEO_QUOTA_BYTES: usize = 100 * 1024 * 1024;
/// Default RAM quota for an audio `SourceBuffer` (16 MiB).
pub const DEFAULT_AUDIO_QUOTA_BYTES: usize = 16 * 1024 * 1024;
/// Spec-standard adjacency tolerance (seconds) when merging buffered ranges.
pub const BUFFERED_ADJACENCY_TOLERANCE_SECS: f64 = 0.1;
/// Seconds before current playback position protected from eviction.
pub const EVICTION_PROTECT_BEFORE_SECS: f64 = 2.0;
/// Seconds after current playback position protected from eviction.
pub const EVICTION_PROTECT_AFTER_SECS: f64 = 30.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MseError {
    /// More bytes are required to finish parsing the current segment or frame.
    NeedMoreData,
    /// Malformed or unsupported container/bitstream bytes.
    InvalidData(String),
    /// A media segment arrived before an initialization segment was parsed.
    MissingInitSegment,
    /// SourceBuffer memory quota exceeded even after evicting non-critical frames.
    QuotaExceeded {
        requested: usize,
        used: usize,
        quota: usize,
    },
    /// Invalid API argument (e.g., negative/NaN timestamp or inverted range).
    InvalidArgument(String),
}

impl fmt::Display for MseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedMoreData => write!(f, "need more data"),
            Self::InvalidData(msg) => write!(f, "invalid MSE data: {msg}"),
            Self::MissingInitSegment => write!(f, "missing initialization segment"),
            Self::QuotaExceeded {
                requested,
                used,
                quota,
            } => write!(
                f,
                "MSE quota exceeded (requested={requested}, used={used}, quota={quota})"
            ),
            Self::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl std::error::Error for MseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MseContainer {
    IsoBmff,
    WebM,
    MpegAudio,
    AdtsAac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MseTrackType {
    Video,
    Audio,
    Text,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MseTrackInfo {
    pub track_id: u64,
    pub track_type: MseTrackType,
    pub timescale: u64,
    pub codec: Option<String>,
    pub default_duration_ns: Option<u64>,
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}

#[derive(Clone, Debug)]
pub struct InitSegment {
    pub id: u64,
    pub bytes: Arc<Vec<u8>>,
    pub container: MseContainer,
    pub tracks: Vec<MseTrackInfo>,
    pub init_data_cenc: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct MediaSegment {
    pub id: u64,
    pub init_id: u64,
    pub start_pts: f64,
    pub end_pts: f64,
    pub is_keyframe: bool,
    pub keyframe_pts: Option<f64>,
    pub last_decode_end_pts: f64,
    pub bytes: Arc<Vec<u8>>,
    pub byte_len: usize,
    pub init_data_cenc: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendMode {
    Segments,
    Sequence,
}

#[derive(Clone, Debug, Default)]
pub struct AppendResult {
    pub init_segments_parsed: usize,
    pub media_segments_parsed: usize,
    pub evicted_bytes: usize,
    pub encrypted_init_data: Vec<(String, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// Bounds-checked binary cursor
// ---------------------------------------------------------------------------

struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], MseError> {
        let end = self.pos.checked_add(len).ok_or(MseError::NeedMoreData)?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or(MseError::NeedMoreData)?;
        self.pos = end;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<(), MseError> {
        let end = self.pos.checked_add(len).ok_or(MseError::NeedMoreData)?;
        if end > self.data.len() {
            return Err(MseError::NeedMoreData);
        }
        self.pos = end;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, MseError> {
        let b = *self.data.get(self.pos).ok_or(MseError::NeedMoreData)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_u16_be(&mut self) -> Result<u16, MseError> {
        let b = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_i16_be(&mut self) -> Result<i16, MseError> {
        let b = self.read_bytes(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn read_u24_be(&mut self) -> Result<u32, MseError> {
        let b = self.read_bytes(3)?;
        Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32))
    }

    fn read_u32_be(&mut self) -> Result<u32, MseError> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i32_be(&mut self) -> Result<i32, MseError> {
        let b = self.read_bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u64_be(&mut self) -> Result<u64, MseError> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_uint_be(&mut self, len: usize) -> Result<u64, MseError> {
        if len == 0 || len > 8 {
            return Err(MseError::InvalidData(format!(
                "invalid big-endian uint width: {len}"
            )));
        }
        let b = self.read_bytes(len)?;
        let mut val = 0u64;
        for &byte in b {
            val = (val << 8) | (byte as u64);
        }
        Ok(val)
    }

    fn read_f32_be(&mut self) -> Result<f32, MseError> {
        let b = self.read_bytes(4)?;
        Ok(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_f64_be(&mut self) -> Result<f64, MseError> {
        let b = self.read_bytes(8)?;
        Ok(f64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_fourcc(&mut self) -> Result<[u8; 4], MseError> {
        let b = self.read_bytes(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }
}

// ---------------------------------------------------------------------------
// ISO-BMFF (fMP4) Parser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct IsoBoxHeader {
    box_type: [u8; 4],
    total_size: u64,
    header_size: usize,
}

fn read_iso_box_header(cursor: &mut ByteCursor<'_>, container_end: Option<usize>) -> Result<IsoBoxHeader, MseError> {
    let start = cursor.position();
    let size32 = cursor.read_u32_be()?;
    let box_type = cursor.read_fourcc()?;
    let (total_size, header_size) = if size32 == 1 {
        let largesize = cursor.read_u64_be()?;
        if largesize < 16 {
            return Err(MseError::InvalidData(
                "ISO-BMFF 64-bit box largesize < 16".into(),
            ));
        }
        (largesize, 16)
    } else if size32 == 0 {
        if let Some(end) = container_end {
            let rem = end.saturating_sub(start) as u64;
            if rem < 8 {
                return Err(MseError::InvalidData("ISO-BMFF box smaller than header".into()));
            }
            (rem, 8)
        } else {
            return Err(MseError::InvalidData(
                "unbounded top-level ISO-BMFF box (size == 0) not supported in streaming mode"
                    .into(),
            ));
        }
    } else {
        if size32 < 8 {
            return Err(MseError::InvalidData(
                "ISO-BMFF box size < 8".into(),
            ));
        }
        (size32 as u64, 8)
    };

    Ok(IsoBoxHeader {
        box_type,
        total_size,
        header_size,
    })
}

fn iso_box_payload<'a>(data: &'a [u8], start: usize, header: IsoBoxHeader) -> Result<(&'a [u8], usize), MseError> {
    let payload_len = usize::try_from(header.total_size - header.header_size as u64)
        .map_err(|_| MseError::InvalidData("ISO-BMFF box size overflow".into()))?;
    let payload_start = start
        .checked_add(header.header_size)
        .ok_or(MseError::InvalidData("ISO-BMFF offset overflow".into()))?;
    let box_end = payload_start
        .checked_add(payload_len)
        .ok_or(MseError::InvalidData("ISO-BMFF box end overflow".into()))?;
    let payload = data
        .get(payload_start..box_end)
        .ok_or(MseError::NeedMoreData)?;
    Ok((payload, box_end))
}

fn parse_mvhd_timescale(payload: &[u8]) -> Result<u64, MseError> {
    let mut c = ByteCursor::new(payload);
    let version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    if version == 0 {
        c.skip(8)?; // creation_time + modification_time
    } else if version == 1 {
        c.skip(16)?;
    } else {
        return Err(MseError::InvalidData(format!("unsupported mvhd version {version}")));
    }
    let timescale = c.read_u32_be()? as u64;
    Ok(timescale)
}

fn parse_tkhd_track_id(payload: &[u8]) -> Result<u32, MseError> {
    let mut c = ByteCursor::new(payload);
    let version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    if version == 0 {
        c.skip(8)?; // creation + modification
    } else if version == 1 {
        c.skip(16)?;
    } else {
        return Err(MseError::InvalidData(format!("unsupported tkhd version {version}")));
    }
    c.read_u32_be()
}

fn parse_mdhd_timescale(payload: &[u8]) -> Result<u64, MseError> {
    let mut c = ByteCursor::new(payload);
    let version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    if version == 0 {
        c.skip(8)?;
    } else if version == 1 {
        c.skip(16)?;
    } else {
        return Err(MseError::InvalidData(format!("unsupported mdhd version {version}")));
    }
    Ok(c.read_u32_be()? as u64)
}

fn parse_hdlr_type(payload: &[u8]) -> Result<MseTrackType, MseError> {
    let mut c = ByteCursor::new(payload);
    let _version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    let _pre_defined = c.read_u32_be()?;
    let handler = c.read_fourcc()?;
    Ok(match &handler {
        b"vide" => MseTrackType::Video,
        b"soun" => MseTrackType::Audio,
        b"subt" | b"text" | b"sbtl" => MseTrackType::Text,
        _ => MseTrackType::Unknown,
    })
}

fn parse_sinf_frma_codec(entry_children: &[u8]) -> Option<String> {
    let mut off = 0usize;
    while off + 8 <= entry_children.len() {
        let mut c = ByteCursor::new(&entry_children[off..]);
        let Ok(h) = read_iso_box_header(&mut c, Some(entry_children.len() - off)) else {
            break;
        };
        let Ok((payload, end)) = iso_box_payload(entry_children, off, h) else {
            break;
        };
        if &h.box_type == b"sinf" {
            let mut soff = 0usize;
            while soff + 8 <= payload.len() {
                let mut sc = ByteCursor::new(&payload[soff..]);
                let Ok(sh) = read_iso_box_header(&mut sc, Some(payload.len() - soff)) else {
                    break;
                };
                let Ok((sp, send)) = iso_box_payload(payload, soff, sh) else {
                    break;
                };
                if &sh.box_type == b"frma" && sp.len() >= 4 {
                    let orig = String::from_utf8_lossy(&sp[..4]).trim().to_string();
                    if !orig.is_empty() {
                        return Some(orig);
                    }
                }
                soff = send;
            }
        }
        off = end;
    }
    None
}

fn parse_stsd_codec(payload: &[u8]) -> Result<Option<String>, MseError> {
    let mut c = ByteCursor::new(payload);
    let _version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    let entry_count = c.read_u32_be()?;
    if entry_count == 0 || c.remaining() < 8 {
        return Ok(None);
    }
    let entry_start = c.position();
    let header = read_iso_box_header(&mut c, Some(payload.len() - entry_start))?;
    if matches!(&header.box_type, b"encv" | b"enca" | b"enct" | b"encs") {
        if let Ok((entry_payload, _)) = iso_box_payload(payload, entry_start, header) {
            let skip = match &header.box_type {
                b"encv" => 78usize,
                b"enca" => {
                    if entry_payload.len() >= 10 {
                        match u16::from_be_bytes([entry_payload[8], entry_payload[9]]) {
                            1 => 44,
                            2 => 64,
                            _ => 28,
                        }
                    } else {
                        28
                    }
                },
                _ => 8usize,
            };
            if entry_payload.len() > skip {
                if let Some(orig) = parse_sinf_frma_codec(&entry_payload[skip..]) {
                    return Ok(Some(orig));
                }
            }
        }
    }
    let codec = String::from_utf8_lossy(&header.box_type).trim().to_string();
    Ok(if codec.is_empty() { None } else { Some(codec) })
}

fn parse_stbl(payload: &[u8]) -> Result<Option<String>, MseError> {
    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        if &header.box_type == b"stsd" {
            return parse_stsd_codec(box_payload);
        }
        offset = box_end;
    }
    Ok(None)
}

fn parse_minf(payload: &[u8]) -> Result<Option<String>, MseError> {
    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        if &header.box_type == b"stbl" {
            return parse_stbl(box_payload);
        }
        offset = box_end;
    }
    Ok(None)
}

fn parse_mdia(payload: &[u8]) -> Result<(u64, MseTrackType, Option<String>), MseError> {
    let mut timescale = 0u64;
    let mut track_type = MseTrackType::Unknown;
    let mut codec = None;
    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        match &header.box_type {
            b"mdhd" => timescale = parse_mdhd_timescale(box_payload)?,
            b"hdlr" => track_type = parse_hdlr_type(box_payload)?,
            b"minf" => {
                if codec.is_none() {
                    codec = parse_minf(box_payload)?;
                }
            },
            _ => {},
        }
        offset = box_end;
    }
    Ok((timescale, track_type, codec))
}

fn parse_trak(payload: &[u8], movie_timescale: u64) -> Result<Option<MseTrackInfo>, MseError> {
    let mut track_id: Option<u32> = None;
    let mut timescale = 0u64;
    let mut track_type = MseTrackType::Unknown;
    let mut codec = None;
    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        match &header.box_type {
            b"tkhd" => track_id = Some(parse_tkhd_track_id(box_payload)?),
            b"mdia" => {
                let (mdhd_ts, hdlr_ty, stsd_codec) = parse_mdia(box_payload)?;
                timescale = mdhd_ts;
                track_type = hdlr_ty;
                codec = stsd_codec;
            },
            _ => {},
        }
        offset = box_end;
    }
    let Some(track_id) = track_id else {
        return Ok(None);
    };
    let effective_timescale = if timescale != 0 {
        timescale
    } else if movie_timescale != 0 {
        movie_timescale
    } else {
        1
    };
    Ok(Some(MseTrackInfo {
        track_id: track_id as u64,
        track_type,
        timescale: effective_timescale,
        codec,
        default_duration_ns: None,
        default_sample_duration: 0,
        default_sample_size: 0,
        default_sample_flags: 0,
    }))
}

#[derive(Clone, Copy, Debug)]
struct TrexDefaults {
    track_id: u32,
    default_sample_duration: u32,
    default_sample_size: u32,
    default_sample_flags: u32,
}

fn parse_trex(payload: &[u8]) -> Result<TrexDefaults, MseError> {
    let mut c = ByteCursor::new(payload);
    let _version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    let track_id = c.read_u32_be()?;
    let _default_sample_description_index = c.read_u32_be()?;
    let default_sample_duration = c.read_u32_be()?;
    let default_sample_size = c.read_u32_be()?;
    let default_sample_flags = c.read_u32_be()?;
    Ok(TrexDefaults {
        track_id,
        default_sample_duration,
        default_sample_size,
        default_sample_flags,
    })
}

fn parse_mvex(payload: &[u8]) -> Result<Vec<TrexDefaults>, MseError> {
    let mut defaults = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        if &header.box_type == b"trex" {
            defaults.push(parse_trex(box_payload)?);
        }
        offset = box_end;
    }
    Ok(defaults)
}

fn parse_moov(payload: &[u8]) -> Result<Vec<MseTrackInfo>, MseError> {
    let mut movie_timescale = 0u64;
    let mut traks = Vec::new();
    let mut trex_list = Vec::new();

    // First pass for mvhd timescale in case mvhd isn't first (though normally it is).
    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        if &header.box_type == b"mvhd" {
            movie_timescale = parse_mvhd_timescale(box_payload)?;
        }
        offset = box_end;
    }

    offset = 0;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        match &header.box_type {
            b"trak" => {
                if let Some(info) = parse_trak(box_payload, movie_timescale)? {
                    traks.push(info);
                }
            },
            b"mvex" => {
                trex_list.extend(parse_mvex(box_payload)?);
            },
            _ => {},
        }
        offset = box_end;
    }

    for trex in trex_list {
        if let Some(track) = traks.iter_mut().find(|t| t.track_id == trex.track_id as u64) {
            track.default_sample_duration = trex.default_sample_duration;
            track.default_sample_size = trex.default_sample_size;
            track.default_sample_flags = trex.default_sample_flags;
        }
    }

    if traks.is_empty() {
        return Err(MseError::InvalidData("moov contains no valid tracks".into()));
    }
    Ok(traks)
}

fn validate_sidx(payload: &[u8]) -> Result<(), MseError> {
    let mut c = ByteCursor::new(payload);
    let version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    let _reference_id = c.read_u32_be()?;
    let timescale = c.read_u32_be()?;
    if timescale == 0 {
        return Err(MseError::InvalidData("sidx timescale is zero".into()));
    }
    if version == 0 {
        c.skip(8)?; // earliest_presentation_time + first_offset
    } else if version == 1 {
        c.skip(16)?;
    } else {
        return Err(MseError::InvalidData(format!("unsupported sidx version {version}")));
    }
    let _reserved = c.read_u16_be()?;
    let reference_count = c.read_u16_be()? as usize;
    let needed = reference_count
        .checked_mul(12)
        .ok_or_else(|| MseError::InvalidData("sidx reference_count overflow".into()))?;
    if c.remaining() < needed {
        return Err(MseError::InvalidData("truncated sidx reference table".into()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct TfhdData {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_sample_duration: Option<u32>,
    default_sample_size: Option<u32>,
    default_sample_flags: Option<u32>,
}

fn parse_tfhd(payload: &[u8]) -> Result<TfhdData, MseError> {
    let mut c = ByteCursor::new(payload);
    let _version = c.read_u8()?;
    let tf_flags = c.read_u24_be()?;
    let track_id = c.read_u32_be()?;
    let base_data_offset = if tf_flags & 0x000001 != 0 {
        Some(c.read_u64_be()?)
    } else {
        None
    };
    if tf_flags & 0x000002 != 0 {
        let _sample_description_index = c.read_u32_be()?;
    }
    let default_sample_duration = if tf_flags & 0x000008 != 0 {
        Some(c.read_u32_be()?)
    } else {
        None
    };
    let default_sample_size = if tf_flags & 0x000010 != 0 {
        Some(c.read_u32_be()?)
    } else {
        None
    };
    let default_sample_flags = if tf_flags & 0x000020 != 0 {
        Some(c.read_u32_be()?)
    } else {
        None
    };
    Ok(TfhdData {
        track_id,
        base_data_offset,
        default_sample_duration,
        default_sample_size,
        default_sample_flags,
    })
}

fn parse_tfdt(payload: &[u8]) -> Result<u64, MseError> {
    let mut c = ByteCursor::new(payload);
    let version = c.read_u8()?;
    let _flags = c.read_u24_be()?;
    if version == 0 {
        Ok(c.read_u32_be()? as u64)
    } else if version == 1 {
        c.read_u64_be()
    } else {
        Err(MseError::InvalidData(format!("unsupported tfdt version {version}")))
    }
}

#[inline]
fn is_iso_sync_sample(sample_flags: u32) -> bool {
    // ISO/IEC 14496-12: bit 16 (0x0001_0000) is sample_is_non_sync_sample;
    // bits 25..24 (sample_depends_on) == 1 means sample depends on another sample.
    let is_non_sync = (sample_flags & 0x0001_0000) != 0;
    let depends_on_other = ((sample_flags >> 24) & 0x03) == 1;
    !is_non_sync && !depends_on_other
}

#[derive(Clone, Debug)]
struct MoofSummary {
    start_pts: f64,
    end_pts: f64,
    is_keyframe: bool,
    keyframe_pts: Option<f64>,
    last_decode_end_pts: f64,
    init_data_cenc: Option<Vec<u8>>,
}

fn parse_trun_samples(
    payload: &[u8],
    tfhd: &TfhdData,
    track: &MseTrackInfo,
    decode_time: &mut u64,
) -> Result<(Option<f64>, Option<f64>, Option<bool>, Option<f64>), MseError> {
    let mut c = ByteCursor::new(payload);
    let version = c.read_u8()?;
    let tr_flags = c.read_u24_be()?;
    let sample_count = c.read_u32_be()?;

    if tr_flags & 0x000001 != 0 {
        let _data_offset = c.read_i32_be()?;
    }
    let first_sample_flags = if tr_flags & 0x000004 != 0 {
        Some(c.read_u32_be()?)
    } else {
        None
    };

    let has_duration = (tr_flags & 0x000100) != 0;
    let has_size = (tr_flags & 0x000200) != 0;
    let has_flags = (tr_flags & 0x000400) != 0;
    let has_cto = (tr_flags & 0x000800) != 0;

    let per_sample_bytes = (has_duration as usize
        + has_size as usize
        + has_flags as usize
        + has_cto as usize)
        * 4;
    let required_bytes = (sample_count as usize)
        .checked_mul(per_sample_bytes)
        .ok_or_else(|| MseError::InvalidData("trun sample_count overflow".into()))?;
    if c.remaining() < required_bytes {
        return Err(MseError::InvalidData(
            "trun sample_count exceeds box payload length".into(),
        ));
    }

    let timescale = track.timescale.max(1) as f64;
    let default_duration = tfhd
        .default_sample_duration
        .unwrap_or(track.default_sample_duration);
    let default_size = tfhd.default_sample_size.unwrap_or(track.default_sample_size);
    let default_flags = tfhd
        .default_sample_flags
        .unwrap_or(track.default_sample_flags);

    let mut min_pts: Option<f64> = None;
    let mut max_end_pts: Option<f64> = None;
    let mut first_is_keyframe: Option<bool> = None;
    let mut first_keyframe_pts: Option<f64> = None;

    for idx in 0..sample_count {
        let duration = if has_duration {
            c.read_u32_be()?
        } else {
            default_duration
        };
        let _size = if has_size {
            c.read_u32_be()?
        } else {
            default_size
        };
        let flags = if has_flags {
            c.read_u32_be()?
        } else if idx == 0 {
            first_sample_flags.unwrap_or(default_flags)
        } else {
            default_flags
        };
        let cto: i64 = if has_cto {
            if version == 0 {
                c.read_u32_be()? as i64
            } else {
                c.read_i32_be()? as i64
            }
        } else {
            0
        };

        let pts_ticks = (*decode_time as i128) + (cto as i128);
        let end_ticks = pts_ticks + (duration as i128);
        let pts = pts_ticks as f64 / timescale;
        let end_pts = end_ticks as f64 / timescale;

        let sync = is_iso_sync_sample(flags);
        if first_is_keyframe.is_none() {
            first_is_keyframe = Some(sync);
        }
        if sync {
            first_keyframe_pts = Some(match first_keyframe_pts {
                Some(prev) => prev.min(pts),
                None => pts,
            });
        }
        min_pts = Some(match min_pts {
            Some(prev) => prev.min(pts),
            None => pts,
        });
        max_end_pts = Some(match max_end_pts {
            Some(prev) => prev.max(end_pts),
            None => end_pts,
        });

        *decode_time = decode_time
            .checked_add(duration as u64)
            .ok_or_else(|| MseError::InvalidData("ISO-BMFF decode time overflow".into()))?;
    }

    Ok((min_pts, max_end_pts, first_is_keyframe, first_keyframe_pts))
}

fn parse_moof(payload: &[u8], init: &InitSegment) -> Result<MoofSummary, MseError> {
    let mut has_mfhd = false;
    let mut start_pts: Option<f64> = None;
    let mut end_pts: Option<f64> = None;
    let mut is_keyframe: Option<bool> = None;
    let mut keyframe_pts: Option<f64> = None;
    let mut last_decode_end_pts = 0.0f64;

    let mut offset = 0usize;
    while offset < payload.len() {
        let mut c = ByteCursor::new(&payload[offset..]);
        let header = read_iso_box_header(&mut c, Some(payload.len() - offset))?;
        let (box_payload, box_end) = iso_box_payload(payload, offset, header)?;
        match &header.box_type {
            b"mfhd" => {
                let mut mc = ByteCursor::new(box_payload);
                let _version = mc.read_u8()?;
                let _flags = mc.read_u24_be()?;
                let _sequence_number = mc.read_u32_be()?;
                has_mfhd = true;
            },
            b"traf" => {
                let mut tfhd: Option<TfhdData> = None;
                let mut base_decode_time = 0u64;
                let mut traf_offset = 0usize;
                let mut trun_payloads = Vec::new();

                while traf_offset < box_payload.len() {
                    let mut tc = ByteCursor::new(&box_payload[traf_offset..]);
                    let th = read_iso_box_header(&mut tc, Some(box_payload.len() - traf_offset))?;
                    let (tp, tend) = iso_box_payload(box_payload, traf_offset, th)?;
                    match &th.box_type {
                        b"tfhd" => tfhd = Some(parse_tfhd(tp)?),
                        b"tfdt" => base_decode_time = parse_tfdt(tp)?,
                        b"trun" => trun_payloads.push(tp),
                        _ => {},
                    }
                    traf_offset = tend;
                }

                let tfhd = tfhd
                    .ok_or_else(|| MseError::InvalidData("traf missing tfhd".into()))?;
                let _ = tfhd.base_data_offset;
                let track = init
                    .tracks
                    .iter()
                    .find(|t| t.track_id == tfhd.track_id as u64)
                    .or_else(|| init.tracks.first())
                    .ok_or_else(|| {
                        MseError::InvalidData(format!(
                            "moof references unknown track_id {}",
                            tfhd.track_id
                        ))
                    })?;

                let mut decode_time = base_decode_time;
                let mut traf_is_keyframe: Option<bool> = None;
                for trun_payload in trun_payloads {
                    let (t_min, t_max, t_kf, t_kf_pts) =
                        parse_trun_samples(trun_payload, &tfhd, track, &mut decode_time)?;
                    if let Some(s) = t_min {
                        start_pts = Some(start_pts.map_or(s, |prev| prev.min(s)));
                    }
                    if let Some(e) = t_max {
                        end_pts = Some(end_pts.map_or(e, |prev| prev.max(e)));
                    }
                    if traf_is_keyframe.is_none() {
                        traf_is_keyframe = t_kf;
                    }
                    if let Some(kp) = t_kf_pts {
                        keyframe_pts = Some(keyframe_pts.map_or(kp, |prev| prev.min(kp)));
                    }
                }
                if let Some(kf) = traf_is_keyframe {
                    if is_keyframe.is_none() || track.track_type == MseTrackType::Video {
                        is_keyframe = Some(kf);
                    }
                }

                let timescale = track.timescale.max(1) as f64;
                let traf_decode_end = decode_time as f64 / timescale;
                last_decode_end_pts = last_decode_end_pts.max(traf_decode_end);

                if start_pts.is_none() {
                    let base_s = base_decode_time as f64 / timescale;
                    start_pts = Some(base_s);
                    end_pts = Some(end_pts.unwrap_or(base_s));
                }
            },
            _ => {},
        }
        offset = box_end;
    }

    if !has_mfhd {
        return Err(MseError::InvalidData("moof missing mfhd".into()));
    }

    let start = start_pts.unwrap_or(0.0);
    let end = end_pts.unwrap_or(start).max(start);
    Ok(MoofSummary {
        start_pts: start,
        end_pts: end,
        is_keyframe: is_keyframe.unwrap_or(false),
        keyframe_pts,
        last_decode_end_pts: last_decode_end_pts.max(end),
        init_data_cenc: None,
    })
}

// ---------------------------------------------------------------------------
// WebM / Matroska (EBML) Parser
// ---------------------------------------------------------------------------

const EBML_ID_HEADER: u32 = 0x1A45_DFA3;
const EBML_ID_SEGMENT: u32 = 0x1853_8067;
const EBML_ID_SEEK_HEAD: u32 = 0x114D_9B74;
const EBML_ID_INFO: u32 = 0x1549_A966;
const EBML_ID_TIMESTAMP_SCALE: u32 = 0x2AD7_B1;
const EBML_ID_DURATION: u32 = 0x4489;
const EBML_ID_TRACKS: u32 = 0x1654_AE6B;
const EBML_ID_TRACK_ENTRY: u32 = 0xAE;
const EBML_ID_TRACK_NUMBER: u32 = 0xD7;
const EBML_ID_TRACK_TYPE: u32 = 0x83;
const EBML_ID_CODEC_ID: u32 = 0x86;
const EBML_ID_DEFAULT_DURATION: u32 = 0x23E3_83;
const EBML_ID_CLUSTER: u32 = 0x1F43_B675;
const EBML_ID_CLUSTER_TIMESTAMP: u32 = 0xE7;
const EBML_ID_SIMPLE_BLOCK: u32 = 0xA3;
const EBML_ID_BLOCK_GROUP: u32 = 0xA0;
const EBML_ID_BLOCK: u32 = 0xA1;
const EBML_ID_BLOCK_DURATION: u32 = 0x9B;
const EBML_ID_REFERENCE_BLOCK: u32 = 0xFB;
const EBML_ID_CUES: u32 = 0x1C53_BB6B;
const EBML_ID_TAGS: u32 = 0x1254_C367;
const EBML_ID_CHAPTERS: u32 = 0x1043_A770;
const EBML_ID_ATTACHMENTS: u32 = 0x1941_A469;

fn read_ebml_id(cursor: &mut ByteCursor<'_>) -> Result<u32, MseError> {
    let first = cursor.read_u8()?;
    if first == 0 {
        return Err(MseError::InvalidData("invalid EBML ID VINT (leading byte 0x00)".into()));
    }
    let width = first.leading_zeros() as usize + 1;
    if width > 4 {
        return Err(MseError::InvalidData(format!(
            "unsupported EBML ID width: {width}"
        )));
    }
    let mut id = first as u32;
    for _ in 1..width {
        id = (id << 8) | (cursor.read_u8()? as u32);
    }
    Ok(id)
}

/// Reads an EBML VINT data size. Returns `Ok(None)` when the size is unknown (all data bits are 1).
fn read_ebml_size(cursor: &mut ByteCursor<'_>) -> Result<Option<u64>, MseError> {
    let first = cursor.read_u8()?;
    if first == 0 {
        return Err(MseError::InvalidData("invalid EBML size VINT (leading byte 0x00)".into()));
    }
    let width = first.leading_zeros() as usize + 1;
    if width > 8 {
        return Err(MseError::InvalidData(format!(
            "unsupported EBML size width: {width}"
        )));
    }
    let marker_mask = 1u8 << (8 - width);
    let mut val = (first & !marker_mask) as u64;
    for _ in 1..width {
        val = (val << 8) | (cursor.read_u8()? as u64);
    }
    let all_ones = (1u64 << (7 * width)) - 1;
    if val == all_ones {
        Ok(None)
    } else {
        Ok(Some(val))
    }
}

fn read_ebml_vint_uint(cursor: &mut ByteCursor<'_>) -> Result<u64, MseError> {
    read_ebml_size(cursor)?.ok_or_else(|| {
        MseError::InvalidData("unexpected unknown-size VINT for track number".into())
    })
}

fn read_ebml_element_header(
    data: &[u8],
    start: usize,
) -> Result<(u32, usize, Option<u64>), MseError> {
    let mut c = ByteCursor::new(&data[start..]);
    let id = read_ebml_id(&mut c)?;
    let size = read_ebml_size(&mut c)?;
    let payload_start = start + c.position();
    Ok((id, payload_start, size))
}

fn read_ebml_element_bounds(
    data: &[u8],
    start: usize,
) -> Result<(u32, usize, Option<usize>), MseError> {
    let (id, payload_start, size) = read_ebml_element_header(data, start)?;
    let end = match size {
        Some(sz) => {
            let sz_usize = usize::try_from(sz)
                .map_err(|_| MseError::InvalidData("EBML element size overflow".into()))?;
            let end = payload_start
                .checked_add(sz_usize)
                .ok_or_else(|| MseError::InvalidData("EBML element end overflow".into()))?;
            if end > data.len() {
                return Err(MseError::NeedMoreData);
            }
            Some(end)
        },
        None => None,
    };
    Ok((id, payload_start, end))
}

fn parse_webm_info(payload: &[u8]) -> Result<(u64, Option<f64>), MseError> {
    let mut timestamp_scale_ns: u64 = 1_000_000;
    let mut duration: Option<f64> = None;
    let mut offset = 0usize;
    while offset < payload.len() {
        let (id, pstart, end_opt) = read_ebml_element_bounds(payload, offset)?;
        let pend = end_opt.ok_or_else(|| {
            MseError::InvalidData("unknown-size element inside EBML Info".into())
        })?;
        let elem = &payload[pstart..pend];
        let mut c = ByteCursor::new(elem);
        match id {
            EBML_ID_TIMESTAMP_SCALE => {
                timestamp_scale_ns = c.read_uint_be(elem.len())?;
                if timestamp_scale_ns == 0 {
                    return Err(MseError::InvalidData("WebM TimestampScale is zero".into()));
                }
            },
            EBML_ID_DURATION => {
                duration = Some(match elem.len() {
                    4 => c.read_f32_be()? as f64,
                    8 => c.read_f64_be()?,
                    _ => {
                        return Err(MseError::InvalidData(format!(
                            "invalid WebM Duration length: {}",
                            elem.len()
                        )));
                    },
                });
            },
            _ => {},
        }
        offset = pend;
    }
    Ok((timestamp_scale_ns, duration))
}

fn parse_webm_track_entry(payload: &[u8], timestamp_scale_ns: u64) -> Result<Option<MseTrackInfo>, MseError> {
    let mut track_number: Option<u64> = None;
    let mut track_type = MseTrackType::Unknown;
    let mut codec: Option<String> = None;
    let mut default_duration_ns: Option<u64> = None;

    let mut offset = 0usize;
    while offset < payload.len() {
        let (id, pstart, end_opt) = read_ebml_element_bounds(payload, offset)?;
        let pend = end_opt.ok_or_else(|| {
            MseError::InvalidData("unknown-size element inside TrackEntry".into())
        })?;
        let elem = &payload[pstart..pend];
        let mut c = ByteCursor::new(elem);
        match id {
            EBML_ID_TRACK_NUMBER => {
                track_number = Some(c.read_uint_be(elem.len())?);
            },
            EBML_ID_TRACK_TYPE => {
                let ty = c.read_uint_be(elem.len())?;
                track_type = match ty {
                    1 => MseTrackType::Video,
                    2 => MseTrackType::Audio,
                    17 => MseTrackType::Text,
                    _ => MseTrackType::Unknown,
                };
            },
            EBML_ID_CODEC_ID => {
                let s = String::from_utf8_lossy(elem)
                    .trim_end_matches('\0')
                    .to_string();
                if !s.is_empty() {
                    codec = Some(s);
                }
            },
            EBML_ID_DEFAULT_DURATION => {
                default_duration_ns = Some(c.read_uint_be(elem.len())?);
            },
            _ => {},
        }
        offset = pend;
    }

    let Some(track_id) = track_number else {
        return Ok(None);
    };
    let timescale = 1_000_000_000u64 / timestamp_scale_ns.max(1);
    Ok(Some(MseTrackInfo {
        track_id,
        track_type,
        timescale: timescale.max(1),
        codec,
        default_duration_ns,
        default_sample_duration: 0,
        default_sample_size: 0,
        default_sample_flags: 0,
    }))
}

fn parse_webm_tracks(payload: &[u8], timestamp_scale_ns: u64) -> Result<Vec<MseTrackInfo>, MseError> {
    let mut tracks = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let (id, pstart, end_opt) = read_ebml_element_bounds(payload, offset)?;
        let pend = end_opt.ok_or_else(|| {
            MseError::InvalidData("unknown-size element inside Tracks".into())
        })?;
        if id == EBML_ID_TRACK_ENTRY {
            if let Some(info) = parse_webm_track_entry(&payload[pstart..pend], timestamp_scale_ns)? {
                tracks.push(info);
            }
        }
        offset = pend;
    }
    if tracks.is_empty() {
        return Err(MseError::InvalidData("WebM Tracks has no TrackEntry".into()));
    }
    Ok(tracks)
}

#[derive(Clone, Copy, Debug)]
struct WebmBlockSample {
    track_number: u64,
    pts_secs: f64,
    explicit_duration_secs: Option<f64>,
    is_keyframe: bool,
}

fn parse_webm_block_header(payload: &[u8]) -> Result<(u64, i16, u8), MseError> {
    let mut c = ByteCursor::new(payload);
    let track_number = read_ebml_vint_uint(&mut c)?;
    let rel_ts = c.read_i16_be()?;
    let flags = c.read_u8()?;
    Ok((track_number, rel_ts, flags))
}

fn parse_webm_block_group(
    payload: &[u8],
    cluster_ts_ticks: u64,
    timestamp_scale_ns: u64,
) -> Result<Option<WebmBlockSample>, MseError> {
    let mut block_header: Option<(u64, i16, u8)> = None;
    let mut block_duration_ticks: Option<u64> = None;
    let mut has_reference_block = false;

    let mut offset = 0usize;
    while offset < payload.len() {
        let (id, pstart, end_opt) = read_ebml_element_bounds(payload, offset)?;
        let pend = end_opt.ok_or_else(|| {
            MseError::InvalidData("unknown-size element inside BlockGroup".into())
        })?;
        let elem = &payload[pstart..pend];
        let mut c = ByteCursor::new(elem);
        match id {
            EBML_ID_BLOCK => {
                block_header = Some(parse_webm_block_header(elem)?);
            },
            EBML_ID_BLOCK_DURATION => {
                block_duration_ticks = Some(c.read_uint_be(elem.len())?);
            },
            EBML_ID_REFERENCE_BLOCK => {
                has_reference_block = true;
            },
            _ => {},
        }
        offset = pend;
    }

    let Some((track_number, rel_ts, _flags)) = block_header else {
        return Ok(None);
    };
    let ticks = (cluster_ts_ticks as i128) + (rel_ts as i128);
    let pts_secs = (ticks * timestamp_scale_ns as i128) as f64 / 1_000_000_000.0;
    let explicit_duration_secs = block_duration_ticks
        .map(|d| (d as u128 * timestamp_scale_ns as u128) as f64 / 1_000_000_000.0);

    Ok(Some(WebmBlockSample {
        track_number,
        pts_secs,
        explicit_duration_secs,
        is_keyframe: !has_reference_block,
    }))
}

fn is_webm_cluster_boundary_element(id: u32) -> bool {
    matches!(
        id,
        EBML_ID_HEADER
            | EBML_ID_SEGMENT
            | EBML_ID_SEEK_HEAD
            | EBML_ID_INFO
            | EBML_ID_TRACKS
            | EBML_ID_CLUSTER
            | EBML_ID_CUES
            | EBML_ID_TAGS
            | EBML_ID_CHAPTERS
            | EBML_ID_ATTACHMENTS
    )
}

fn summarize_webm_cluster_samples(
    samples: &[WebmBlockSample],
    init: &InitSegment,
    last_frame_duration: &mut Option<f64>,
) -> MoofSummary {
    if samples.is_empty() {
        return MoofSummary {
            start_pts: 0.0,
            end_pts: 0.0,
            is_keyframe: false,
            keyframe_pts: None,
            last_decode_end_pts: 0.0,
            init_data_cenc: None,
        };
    }

    // Collect positive inter-block deltas for median estimation of final sample duration.
    let mut deltas: Vec<f64> = Vec::new();
    for pair in samples.windows(2) {
        let d = pair[1].pts_secs - pair[0].pts_secs;
        if d > 0.0 {
            deltas.push(d);
        }
    }
    deltas.sort_by(|a, b| a.total_cmp(b));
    let median_delta = if deltas.is_empty() {
        None
    } else {
        Some(deltas[deltas.len() / 2])
    };

    let mut start_pts = f64::INFINITY;
    let mut end_pts = f64::NEG_INFINITY;
    let first_is_keyframe = samples[0].is_keyframe;
    let mut keyframe_pts: Option<f64> = None;

    for (i, sample) in samples.iter().enumerate() {
        let dur = if let Some(explicit) = sample.explicit_duration_secs {
            explicit
        } else if i + 1 < samples.len() && samples[i + 1].pts_secs > sample.pts_secs {
            samples[i + 1].pts_secs - sample.pts_secs
        } else if let Some(med) = median_delta {
            med
        } else if let Some(track_default_ns) = init
            .tracks
            .iter()
            .find(|t| t.track_id == sample.track_number)
            .and_then(|t| t.default_duration_ns)
            .filter(|&d| d > 0)
        {
            track_default_ns as f64 / 1_000_000_000.0
        } else if let Some(prev_dur) = *last_frame_duration {
            prev_dur
        } else {
            let fallback_scale = init
                .tracks
                .iter()
                .find(|t| t.track_id == sample.track_number)
                .or_else(|| init.tracks.first())
                .map_or(1000, |t| t.timescale.max(1));
            1.0 / fallback_scale as f64
        };

        if dur > 0.0 {
            *last_frame_duration = Some(dur);
        }
        start_pts = start_pts.min(sample.pts_secs);
        end_pts = end_pts.max(sample.pts_secs + dur.max(0.0));
        if sample.is_keyframe {
            keyframe_pts = Some(match keyframe_pts {
                Some(prev) => prev.min(sample.pts_secs),
                None => sample.pts_secs,
            });
        }
    }

    if let Some(med) = median_delta {
        *last_frame_duration = Some(med);
    }

    MoofSummary {
        start_pts,
        end_pts: end_pts.max(start_pts),
        is_keyframe: first_is_keyframe,
        keyframe_pts,
        last_decode_end_pts: end_pts.max(start_pts),
        init_data_cenc: None,
    }
}

fn parse_webm_cluster_children(
    data: &[u8],
    start: usize,
    limit: Option<usize>,
    timestamp_scale_ns: u64,
) -> Result<(Vec<WebmBlockSample>, usize, bool), MseError> {
    let max_end = limit.unwrap_or(data.len());
    let mut offset = start;
    let mut cluster_ts_ticks: u64 = 0;
    let mut samples = Vec::new();
    let mut closed = limit.is_some();

    while offset < max_end {
        let mut peek = ByteCursor::new(&data[offset..max_end]);
        let next_id = match read_ebml_id(&mut peek) {
            Ok(id) => id,
            Err(MseError::NeedMoreData) => {
                if limit.is_some() {
                    return Err(MseError::InvalidData("truncated child inside WebM Cluster".into()));
                }
                return Ok((samples, offset, false));
            },
            Err(e) => return Err(e),
        };

        if limit.is_none() && is_webm_cluster_boundary_element(next_id) {
            closed = true;
            break;
        }

        let (id, pstart, end_opt) = match read_ebml_element_bounds(&data[..max_end], offset) {
            Ok(v) => v,
            Err(MseError::NeedMoreData) if limit.is_none() => {
                return Ok((samples, offset, false));
            },
            Err(e) => return Err(e),
        };
        let pend = end_opt.ok_or_else(|| {
            MseError::InvalidData("unexpected unknown-size element inside WebM Cluster".into())
        })?;
        let elem = &data[pstart..pend];

        match id {
            EBML_ID_CLUSTER_TIMESTAMP => {
                let mut c = ByteCursor::new(elem);
                cluster_ts_ticks = c.read_uint_be(elem.len())?;
            },
            EBML_ID_SIMPLE_BLOCK => {
                let (track_number, rel_ts, flags) = parse_webm_block_header(elem)?;
                let ticks = (cluster_ts_ticks as i128) + (rel_ts as i128);
                let pts_secs = (ticks * timestamp_scale_ns as i128) as f64 / 1_000_000_000.0;
                samples.push(WebmBlockSample {
                    track_number,
                    pts_secs,
                    explicit_duration_secs: None,
                    is_keyframe: (flags & 0x80) != 0,
                });
            },
            EBML_ID_BLOCK_GROUP => {
                if let Some(sample) =
                    parse_webm_block_group(elem, cluster_ts_ticks, timestamp_scale_ns)?
                {
                    samples.push(sample);
                }
            },
            _ => {},
        }
        offset = pend;
    }

    Ok((samples, offset, closed))
}

// ---------------------------------------------------------------------------
// MPEG Audio (MP3 / ADTS AAC) Parser
// ---------------------------------------------------------------------------

const ADTS_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

const MP3_BITRATES_V1_L1: [u32; 16] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];
const MP3_BITRATES_V1_L2: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];
const MP3_BITRATES_V1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const MP3_BITRATES_V2_L1: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];
const MP3_BITRATES_V2_L23: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

fn id3v2_tag_size(data: &[u8]) -> Result<Option<usize>, MseError> {
    if data.len() < 3 {
        return Err(MseError::NeedMoreData);
    }
    if &data[0..3] != b"ID3" {
        return Ok(None);
    }
    if data.len() < 10 {
        return Err(MseError::NeedMoreData);
    }
    let ver_major = data[3];
    let ver_minor = data[4];
    let flags = data[5];
    let s0 = data[6];
    let s1 = data[7];
    let s2 = data[8];
    let s3 = data[9];
    if ver_major == 0xFF || ver_minor == 0xFF || (s0 | s1 | s2 | s3) & 0x80 != 0 {
        return Err(MseError::InvalidData("invalid ID3v2 header".into()));
    }
    let payload_size = ((s0 as usize) << 21)
        | ((s1 as usize) << 14)
        | ((s2 as usize) << 7)
        | (s3 as usize);
    let footer_size = if (flags & 0x10) != 0 { 10 } else { 0 };
    let total = 10usize
        .checked_add(payload_size)
        .and_then(|v| v.checked_add(footer_size))
        .ok_or_else(|| MseError::InvalidData("ID3v2 size overflow".into()))?;
    if data.len() < total {
        return Err(MseError::NeedMoreData);
    }
    Ok(Some(total))
}

#[derive(Clone, Copy, Debug)]
struct AudioFrameInfo {
    container: MseContainer,
    frame_size: usize,
    sample_rate: u32,
    duration_secs: f64,
}

fn parse_audio_frame(data: &[u8]) -> Result<AudioFrameInfo, MseError> {
    if data.len() < 4 {
        return Err(MseError::NeedMoreData);
    }
    if data[0] != 0xFF || (data[1] & 0xE0) != 0xE0 {
        return Err(MseError::InvalidData(
            "missing MPEG audio / ADTS AAC frame syncword".into(),
        ));
    }

    let layer = (data[1] >> 1) & 0x03;
    // ADTS AAC has 12-bit sync 0xFFF and layer == 0.
    if (data[1] & 0xF6) == 0xF0 && layer == 0 {
        if data.len() < 7 {
            return Err(MseError::NeedMoreData);
        }
        let protection_absent = (data[1] & 0x01) != 0;
        let min_header = if protection_absent { 7 } else { 9 };
        let sr_idx = ((data[2] >> 2) & 0x0F) as usize;
        let sample_rate = *ADTS_SAMPLE_RATES
            .get(sr_idx)
            .ok_or_else(|| MseError::InvalidData("invalid ADTS sample rate index".into()))?;
        let frame_length = (((data[3] & 0x03) as usize) << 11)
            | ((data[4] as usize) << 3)
            | (((data[5] >> 5) & 0x07) as usize);
        if frame_length < min_header {
            return Err(MseError::InvalidData(
                "ADTS frame_length smaller than header".into(),
            ));
        }
        if data.len() < frame_length {
            return Err(MseError::NeedMoreData);
        }
        let num_blocks = (data[6] & 0x03) as u32 + 1;
        let duration_secs = (num_blocks as f64 * 1024.0) / sample_rate as f64;
        return Ok(AudioFrameInfo {
            container: MseContainer::AdtsAac,
            frame_size: frame_length,
            sample_rate,
            duration_secs,
        });
    }

    // MPEG Audio Layer I / II / III
    let version_id = (data[1] >> 3) & 0x03;
    if version_id == 1 || layer == 0 {
        return Err(MseError::InvalidData(
            "reserved MPEG audio version or layer".into(),
        ));
    }
    let bitrate_idx = ((data[2] >> 4) & 0x0F) as usize;
    let sr_idx = ((data[2] >> 2) & 0x03) as usize;
    let padding = ((data[2] >> 1) & 0x01) as usize;

    if bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
        return Err(MseError::InvalidData(
            "invalid MPEG audio bitrate or sample rate index".into(),
        ));
    }

    let sample_rate = match (version_id, sr_idx) {
        (3, 0) => 44100,
        (3, 1) => 48000,
        (3, 2) => 32000,
        (2, 0) => 22050,
        (2, 1) => 24000,
        (2, 2) => 16000,
        (0, 0) => 11025,
        (0, 1) => 12000,
        (0, 2) => 8000,
        _ => return Err(MseError::InvalidData("invalid MPEG audio sample rate".into())),
    };

    let bitrate_kbps = match (version_id == 3, layer) {
        (true, 3) => MP3_BITRATES_V1_L1[bitrate_idx],
        (true, 2) => MP3_BITRATES_V1_L2[bitrate_idx],
        (true, 1) => MP3_BITRATES_V1_L3[bitrate_idx],
        (false, 3) => MP3_BITRATES_V2_L1[bitrate_idx],
        (false, 2 | 1) => MP3_BITRATES_V2_L23[bitrate_idx],
        _ => 0,
    };
    if bitrate_kbps == 0 {
        return Err(MseError::InvalidData("zero MPEG audio bitrate".into()));
    }
    let bitrate_bps = bitrate_kbps as usize * 1000;

    let samples_per_frame = match (version_id == 3, layer) {
        (_, 3) => 384u32,
        (_, 2) => 1152,
        (true, 1) => 1152,
        (false, 1) => 576,
        _ => unreachable!(),
    };

    let frame_size = match (version_id == 3, layer) {
        (_, 3) => (12 * bitrate_bps / sample_rate as usize + padding) * 4,
        (true, 2 | 1) | (false, 2) => 144 * bitrate_bps / sample_rate as usize + padding,
        (false, 1) => 72 * bitrate_bps / sample_rate as usize + padding,
        _ => unreachable!(),
    };
    if frame_size < 4 {
        return Err(MseError::InvalidData("computed MP3 frame_size < 4".into()));
    }
    if data.len() < frame_size {
        return Err(MseError::NeedMoreData);
    }

    let duration_secs = samples_per_frame as f64 / sample_rate as f64;
    Ok(AudioFrameInfo {
        container: MseContainer::MpegAudio,
        frame_size,
        sample_rate,
        duration_secs,
    })
}

// ---------------------------------------------------------------------------
// PendingSegmentBuffer (chunked streaming parser across container formats)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum ParsedSegment {
    Init(InitSegment),
    Media(MediaSegment),
}

#[derive(Clone, Debug)]
struct IsoPendingMoof {
    summary: MoofSummary,
    raw_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct WebmParserState {
    in_segment: bool,
    init_bytes: Vec<u8>,
    timestamp_scale_ns: u64,
    parsed_info: bool,
    parsed_tracks: Option<Vec<MseTrackInfo>>,
    init_emitted: bool,
    last_frame_duration: Option<f64>,
}

/// Buffers partial appends across arbitrary chunk boundaries and emits complete
/// `InitSegment` and `MediaSegment` items for ISO-BMFF, WebM, MP3, and ADTS AAC.
#[derive(Clone, Debug)]
pub struct PendingSegmentBuffer {
    buffer: Vec<u8>,
    container: Option<MseContainer>,
    next_init_id: u64,
    next_media_id: u64,
    active_init: Option<InitSegment>,
    // ISO-BMFF state
    iso_init_prefix: Vec<u8>,
    iso_media_prefix: Vec<u8>,
    iso_pending_moof: Option<IsoPendingMoof>,
    // WebM state
    webm: WebmParserState,
    // MPEG Audio state
    audio_next_pts: f64,
}

impl Default for PendingSegmentBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingSegmentBuffer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            container: None,
            next_init_id: 1,
            next_media_id: 1,
            active_init: None,
            iso_init_prefix: Vec::new(),
            iso_media_prefix: Vec::new(),
            iso_pending_moof: None,
            webm: WebmParserState {
                timestamp_scale_ns: 1_000_000,
                ..Default::default()
            },
            audio_next_pts: 0.0,
        }
    }

    pub fn pending_len(&self) -> usize {
        self.buffer.len()
            + self.iso_init_prefix.len()
            + self.iso_media_prefix.len()
            + self
                .iso_pending_moof
                .as_ref()
                .map_or(0, |m| m.raw_bytes.len())
            + self.webm.init_bytes.len()
    }

    pub fn container(&self) -> Option<MseContainer> {
        self.container
    }

    pub fn active_init(&self) -> Option<&InitSegment> {
        self.active_init.as_ref()
    }

    pub fn reset_parser_state(&mut self) {
        self.buffer.clear();
        self.iso_init_prefix.clear();
        self.iso_media_prefix.clear();
        self.iso_pending_moof = None;
        self.webm.init_bytes.clear();
        // Keep active_init, container, IDs, and audio_next_pts intact unless full reset.
    }

    pub fn reset_for_change_type(&mut self) {
        self.buffer.clear();
        self.container = None;
        self.iso_init_prefix.clear();
        self.iso_media_prefix.clear();
        self.iso_pending_moof = None;
        self.webm = WebmParserState {
            timestamp_scale_ns: 1_000_000,
            ..Default::default()
        };
        self.audio_next_pts = 0.0;
    }

    pub fn push_and_parse(&mut self, chunk: &[u8]) -> Result<Vec<ParsedSegment>, MseError> {
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        self.buffer.extend_from_slice(chunk);

        if self.container.is_none() {
            match self.detect_container() {
                Ok(Some(container)) => self.container = Some(container),
                Ok(None) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            }
        }

        match self.container.expect("container detected") {
            MseContainer::IsoBmff => self.parse_iso_bmff(),
            MseContainer::WebM => self.parse_webm(false),
            MseContainer::MpegAudio | MseContainer::AdtsAac => self.parse_mpeg_audio(),
        }
    }

    pub fn flush(&mut self) -> Result<Vec<ParsedSegment>, MseError> {
        let mut out = Vec::new();
        match self.container {
            Some(MseContainer::IsoBmff) => {
                if let Some(pending) = self.iso_pending_moof.take() {
                    if let Some(init) = self.active_init.as_ref() {
                        let id = self.next_media_id;
                        self.next_media_id += 1;
                        let byte_len = pending.raw_bytes.len();
                        out.push(ParsedSegment::Media(MediaSegment {
                            id,
                            init_id: init.id,
                            start_pts: pending.summary.start_pts,
                            end_pts: pending.summary.end_pts,
                            is_keyframe: pending.summary.is_keyframe,
                            keyframe_pts: pending.summary.keyframe_pts,
                            last_decode_end_pts: pending.summary.last_decode_end_pts,
                            bytes: Arc::new(pending.raw_bytes),
                            byte_len,
                            init_data_cenc: pending.summary.init_data_cenc,
                        }));
                    }
                }
            },
            Some(MseContainer::WebM) => {
                out.extend(self.parse_webm(true)?);
            },
            _ => {},
        }
        Ok(out)
    }

    fn detect_container(&mut self) -> Result<Option<MseContainer>, MseError> {
        // Check EBML first (4 bytes)
        if self.buffer.len() < 2 {
            return Ok(None);
        }
        if self.buffer.len() >= 4 && self.buffer[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
            return Ok(Some(MseContainer::WebM));
        }
        if self.buffer.len() >= 3 && &self.buffer[0..3] == b"ID3" {
            // Wait until ID3v2 is skippable or check ADTS/MP3 after
            match id3v2_tag_size(&self.buffer) {
                Ok(Some(skip)) => {
                    self.buffer.drain(..skip);
                    return self.detect_container();
                },
                Ok(None) => {},
                Err(MseError::NeedMoreData) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        if self.buffer[0] == 0xFF && (self.buffer[1] & 0xE0) == 0xE0 {
            let layer = (self.buffer[1] >> 1) & 0x03;
            if (self.buffer[1] & 0xF6) == 0xF0 && layer == 0 {
                return Ok(Some(MseContainer::AdtsAac));
            }
            return Ok(Some(MseContainer::MpegAudio));
        }
        if self.buffer.len() < 8 {
            return Ok(None);
        }
        let fourcc = &self.buffer[4..8];
        if matches!(
            fourcc,
            b"ftyp"
                | b"styp"
                | b"moov"
                | b"moof"
                | b"mdat"
                | b"sidx"
                | b"free"
                | b"skip"
                | b"emsg"
                | b"prft"
                | b"uuid"
        ) {
            return Ok(Some(MseContainer::IsoBmff));
        }
        Err(MseError::InvalidData(
            "unrecognized MSE container format".into(),
        ))
    }

    fn parse_iso_bmff(&mut self) -> Result<Vec<ParsedSegment>, MseError> {
        let mut out = Vec::new();
        let mut offset = 0usize;

        while offset < self.buffer.len() {
            let mut c = ByteCursor::new(&self.buffer[offset..]);
            let header = match read_iso_box_header(&mut c, None) {
                Ok(h) => h,
                Err(MseError::NeedMoreData) => break,
                Err(e) => return Err(e),
            };
            let (payload, box_end) = match iso_box_payload(&self.buffer, offset, header) {
                Ok(v) => v,
                Err(MseError::NeedMoreData) => break,
                Err(e) => return Err(e),
            };
            let full_box = &self.buffer[offset..box_end];

            match &header.box_type {
                b"ftyp" => {
                    self.iso_init_prefix.clear();
                    self.iso_media_prefix.clear();
                    self.iso_init_prefix.extend_from_slice(full_box);
                },
                b"moov" => {
                    let tracks = parse_moov(payload)?;
                    self.iso_media_prefix.clear();
                    let mut init_bytes = std::mem::take(&mut self.iso_init_prefix);
                    init_bytes.extend_from_slice(full_box);
                    let mut cenc_pssh = crate::cenc::extract_cenc_pssh_boxes(&init_bytes);
                    if cenc_pssh.is_empty() {
                        let enc_map = crate::cenc::parse_moov_encryption(&init_bytes);
                        if !enc_map.is_empty() {
                            let mut kids: Vec<[u8; 16]> = Vec::new();
                            for info in enc_map.values() {
                                if info.default_is_protected && !kids.contains(&info.default_kid) {
                                    kids.push(info.default_kid);
                                }
                            }
                            if !kids.is_empty() {
                                cenc_pssh = crate::cenc::synthesize_clearkey_pssh_v1(&kids);
                            }
                        }
                    }
                    let init_data_cenc = if cenc_pssh.is_empty() {
                        None
                    } else {
                        Some(cenc_pssh)
                    };
                    let id = self.next_init_id;
                    self.next_init_id += 1;
                    let init = InitSegment {
                        id,
                        bytes: Arc::new(init_bytes),
                        container: MseContainer::IsoBmff,
                        tracks,
                        init_data_cenc,
                    };
                    self.active_init = Some(init.clone());
                    out.push(ParsedSegment::Init(init));
                },
                b"styp" | b"free" | b"skip" | b"emsg" | b"prft" | b"pssh" => {
                    if self.active_init.is_some() {
                        self.iso_media_prefix.extend_from_slice(full_box);
                    } else {
                        self.iso_init_prefix.extend_from_slice(full_box);
                    }
                },
                b"sidx" => {
                    validate_sidx(payload)?;
                    self.iso_media_prefix.extend_from_slice(full_box);
                },
                b"moof" => {
                    let init = self
                        .active_init
                        .as_ref()
                        .ok_or(MseError::MissingInitSegment)?;
                    if let Some(prev) = self.iso_pending_moof.take() {
                        let id = self.next_media_id;
                        self.next_media_id += 1;
                        let byte_len = prev.raw_bytes.len();
                        out.push(ParsedSegment::Media(MediaSegment {
                            id,
                            init_id: init.id,
                            start_pts: prev.summary.start_pts,
                            end_pts: prev.summary.end_pts,
                            is_keyframe: prev.summary.is_keyframe,
                            keyframe_pts: prev.summary.keyframe_pts,
                            last_decode_end_pts: prev.summary.last_decode_end_pts,
                            bytes: Arc::new(prev.raw_bytes),
                            byte_len,
                            init_data_cenc: prev.summary.init_data_cenc,
                        }));
                    }
                    let mut summary = parse_moof(payload, init)?;
                    let mut raw_bytes = std::mem::take(&mut self.iso_media_prefix);
                    raw_bytes.extend_from_slice(full_box);
                    let moof_pssh = crate::cenc::extract_cenc_pssh_boxes(&raw_bytes);
                    if !moof_pssh.is_empty() {
                        summary.init_data_cenc = Some(moof_pssh);
                    }
                    self.iso_pending_moof = Some(IsoPendingMoof { summary, raw_bytes });
                },
                b"mdat" => {
                    let init = self
                        .active_init
                        .as_ref()
                        .ok_or(MseError::MissingInitSegment)?;
                    if let Some(mut pending) = self.iso_pending_moof.take() {
                        pending.raw_bytes.extend_from_slice(full_box);
                        let id = self.next_media_id;
                        self.next_media_id += 1;
                        let byte_len = pending.raw_bytes.len();
                        out.push(ParsedSegment::Media(MediaSegment {
                            id,
                            init_id: init.id,
                            start_pts: pending.summary.start_pts,
                            end_pts: pending.summary.end_pts,
                            is_keyframe: pending.summary.is_keyframe,
                            keyframe_pts: pending.summary.keyframe_pts,
                            last_decode_end_pts: pending.summary.last_decode_end_pts,
                            bytes: Arc::new(pending.raw_bytes),
                            byte_len,
                            init_data_cenc: pending.summary.init_data_cenc,
                        }));
                    } else {
                        self.iso_media_prefix.extend_from_slice(full_box);
                    }
                },
                _ => {},
            }

            offset = box_end;
        }

        if offset > 0 {
            self.buffer.drain(..offset);
        }
        Ok(out)
    }

    fn maybe_emit_webm_init(&mut self, out: &mut Vec<ParsedSegment>) {
        if !self.webm.init_emitted {
            if let Some(tracks) = self.webm.parsed_tracks.take() {
                let id = self.next_init_id;
                self.next_init_id += 1;
                let init_bytes = std::mem::take(&mut self.webm.init_bytes);
                let init = InitSegment {
                    id,
                    bytes: Arc::new(init_bytes),
                    container: MseContainer::WebM,
                    tracks,
                    init_data_cenc: None,
                };
                self.active_init = Some(init.clone());
                self.webm.init_emitted = true;
                out.push(ParsedSegment::Init(init));
            }
        }
    }

    fn parse_webm(&mut self, flush_open_cluster: bool) -> Result<Vec<ParsedSegment>, MseError> {
        let mut out = Vec::new();
        let mut offset = 0usize;

        while offset < self.buffer.len() {
            if !self.webm.in_segment {
                let (id, pstart, size_opt) = match read_ebml_element_header(&self.buffer, offset) {
                    Ok(v) => v,
                    Err(MseError::NeedMoreData) => break,
                    Err(e) => return Err(e),
                };
                if id == EBML_ID_SEGMENT {
                    // Stream into Segment immediately even when Segment has a large finite size
                    // (e.g. YouTube DASH WebM init segments declare the full-file Segment size).
                    let _ = size_opt;
                    self.webm.init_bytes.extend_from_slice(&self.buffer[offset..pstart]);
                    self.webm.in_segment = true;
                    offset = pstart;
                    continue;
                }
                let sz = size_opt.ok_or_else(|| {
                    MseError::InvalidData("unknown-size non-Segment top-level EBML element".into())
                })?;
                let sz_usize = usize::try_from(sz)
                    .map_err(|_| MseError::InvalidData("EBML element size overflow".into()))?;
                let pend = pstart
                    .checked_add(sz_usize)
                    .ok_or_else(|| MseError::InvalidData("EBML element end overflow".into()))?;
                if pend > self.buffer.len() {
                    break;
                }
                if id == EBML_ID_HEADER {
                    self.webm.init_bytes.clear();
                    self.webm.init_bytes.extend_from_slice(&self.buffer[offset..pend]);
                    self.webm.timestamp_scale_ns = 1_000_000;
                    self.webm.parsed_info = false;
                    self.webm.parsed_tracks = None;
                    self.webm.init_emitted = false;
                    self.webm.last_frame_duration = None;
                }
                offset = pend;
                continue;
            }

            // Inside Segment: check if we see a new top-level EBML/Segment header (e.g. ABR init switch).
            let mut peek = ByteCursor::new(&self.buffer[offset..]);
            let peek_id = match read_ebml_id(&mut peek) {
                Ok(id) => id,
                Err(MseError::NeedMoreData) => break,
                Err(e) => return Err(e),
            };
            if peek_id == EBML_ID_HEADER {
                self.webm.in_segment = false;
                continue;
            }

            let (id, pstart, end_opt) = match read_ebml_element_bounds(&self.buffer, offset) {
                Ok(v) => v,
                Err(MseError::NeedMoreData) => break,
                Err(e) => return Err(e),
            };

            match id {
                EBML_ID_INFO => {
                    let pend = end_opt.ok_or_else(|| {
                        MseError::InvalidData("unknown-size WebM Info".into())
                    })?;
                    let (ts_scale, _dur) = parse_webm_info(&self.buffer[pstart..pend])?;
                    self.webm.timestamp_scale_ns = ts_scale;
                    self.webm.parsed_info = true;
                    if !self.webm.init_emitted {
                        self.webm.init_bytes.extend_from_slice(&self.buffer[offset..pend]);
                    }
                    offset = pend;
                },
                EBML_ID_TRACKS => {
                    let pend = end_opt.ok_or_else(|| {
                        MseError::InvalidData("unknown-size WebM Tracks".into())
                    })?;
                    let tracks = parse_webm_tracks(
                        &self.buffer[pstart..pend],
                        self.webm.timestamp_scale_ns,
                    )?;
                    self.webm.parsed_tracks = Some(tracks);
                    if !self.webm.init_emitted {
                        self.webm.init_bytes.extend_from_slice(&self.buffer[offset..pend]);
                    }
                    offset = pend;
                    // If Info was already parsed (or we're at end of buffer), emit InitSegment now.
                    if self.webm.parsed_info || offset == self.buffer.len() {
                        self.maybe_emit_webm_init(&mut out);
                    }
                },
                EBML_ID_CLUSTER => {
                    self.maybe_emit_webm_init(&mut out);
                    let init = self
                        .active_init
                        .as_ref()
                        .ok_or(MseError::MissingInitSegment)?
                        .clone();
                    let (samples, cluster_end, closed) = parse_webm_cluster_children(
                        &self.buffer,
                        pstart,
                        end_opt,
                        self.webm.timestamp_scale_ns,
                    )?;
                    if !closed && !flush_open_cluster {
                        break;
                    }
                    if samples.is_empty() && !closed && flush_open_cluster {
                        break;
                    }
                    let summary = summarize_webm_cluster_samples(
                        &samples,
                        &init,
                        &mut self.webm.last_frame_duration,
                    );
                    let raw_bytes = self.buffer[offset..cluster_end].to_vec();
                    let byte_len = raw_bytes.len();
                    let media_id = self.next_media_id;
                    self.next_media_id += 1;
                    out.push(ParsedSegment::Media(MediaSegment {
                        id: media_id,
                        init_id: init.id,
                        start_pts: summary.start_pts,
                        end_pts: summary.end_pts,
                        is_keyframe: summary.is_keyframe,
                        keyframe_pts: summary.keyframe_pts,
                        last_decode_end_pts: summary.last_decode_end_pts,
                        bytes: Arc::new(raw_bytes),
                        byte_len,
                        init_data_cenc: None,
                    }));
                    offset = cluster_end;
                },
                _ => {
                    let pend = match end_opt {
                        Some(pend) => pend,
                        None => {
                            return Err(MseError::InvalidData(format!(
                                "unsupported unknown-size WebM element 0x{id:X}"
                            )));
                        },
                    };
                    if !self.webm.init_emitted {
                        self.webm.init_bytes.extend_from_slice(&self.buffer[offset..pend]);
                    }
                    offset = pend;
                },
            }
        }

        if !self.webm.init_emitted && self.webm.parsed_tracks.is_some() {
            self.maybe_emit_webm_init(&mut out);
        }

        if offset > 0 {
            self.buffer.drain(..offset);
        }
        Ok(out)
    }

    fn parse_mpeg_audio(&mut self) -> Result<Vec<ParsedSegment>, MseError> {
        let mut out = Vec::new();
        let mut offset = 0usize;

        while offset < self.buffer.len() {
            if self.buffer[offset..].starts_with(b"ID3") {
                match id3v2_tag_size(&self.buffer[offset..]) {
                    Ok(Some(skip)) => {
                        offset += skip;
                        continue;
                    },
                    Ok(None) => {},
                    Err(MseError::NeedMoreData) => break,
                    Err(e) => return Err(e),
                }
            }

            let frame = match parse_audio_frame(&self.buffer[offset..]) {
                Ok(f) => f,
                Err(MseError::NeedMoreData) => break,
                Err(e) => return Err(e),
            };

            if self.active_init.is_none() {
                let init_id = self.next_init_id;
                self.next_init_id += 1;
                let codec = match frame.container {
                    MseContainer::AdtsAac => Some("mp4a.40.2".to_string()),
                    _ => Some("mp3".to_string()),
                };
                let init = InitSegment {
                    id: init_id,
                    bytes: Arc::new(Vec::new()),
                    container: frame.container,
                    tracks: vec![MseTrackInfo {
                        track_id: 1,
                        track_type: MseTrackType::Audio,
                        timescale: frame.sample_rate as u64,
                        codec,
                        default_duration_ns: None,
                        default_sample_duration: 0,
                        default_sample_size: 0,
                        default_sample_flags: 0,
                    }],
                    init_data_cenc: None,
                };
                self.active_init = Some(init.clone());
                out.push(ParsedSegment::Init(init));
            }

            let init_id = self.active_init.as_ref().expect("init created").id;
            let start_pts = self.audio_next_pts;
            let end_pts = start_pts + frame.duration_secs;
            self.audio_next_pts = end_pts;

            let frame_bytes = self.buffer[offset..offset + frame.frame_size].to_vec();
            let byte_len = frame_bytes.len();
            let media_id = self.next_media_id;
            self.next_media_id += 1;
            out.push(ParsedSegment::Media(MediaSegment {
                id: media_id,
                init_id,
                start_pts,
                end_pts,
                is_keyframe: true,
                keyframe_pts: Some(start_pts),
                last_decode_end_pts: end_pts,
                bytes: Arc::new(frame_bytes),
                byte_len,
                init_data_cenc: None,
            }));

            offset += frame.frame_size;
        }

        if offset > 0 {
            self.buffer.drain(..offset);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// W3C MseTrackBuffer
// ---------------------------------------------------------------------------

pub struct MseTrackBuffer {
    append_mode: AppendMode,
    timestamp_offset: f64,
    group_start_timestamp: Option<f64>,
    group_end_timestamp: f64,
    append_window_start: f64,
    append_window_end: f64,
    highest_end_timestamp: f64,
    quota_bytes: usize,
    used_bytes: usize,
    playback_position: f64,
    pending: PendingSegmentBuffer,
    init_segments: BTreeMap<u64, InitSegment>,
    active_init_id: Option<u64>,
    media_segments: Vec<MediaSegment>,
}

impl MseTrackBuffer {
    pub fn new_video() -> Self {
        Self::with_quota(DEFAULT_VIDEO_QUOTA_BYTES)
    }

    pub fn new_audio() -> Self {
        Self::with_quota(DEFAULT_AUDIO_QUOTA_BYTES)
    }

    pub fn with_quota(quota_bytes: usize) -> Self {
        Self {
            append_mode: AppendMode::Segments,
            timestamp_offset: 0.0,
            group_start_timestamp: None,
            group_end_timestamp: 0.0,
            append_window_start: 0.0,
            append_window_end: f64::INFINITY,
            highest_end_timestamp: 0.0,
            quota_bytes,
            used_bytes: 0,
            playback_position: 0.0,
            pending: PendingSegmentBuffer::new(),
            init_segments: BTreeMap::new(),
            active_init_id: None,
            media_segments: Vec::new(),
        }
    }

    pub fn append_mode(&self) -> AppendMode {
        self.append_mode
    }

    pub fn set_append_mode(&mut self, mode: AppendMode) {
        if self.append_mode != mode {
            self.append_mode = mode;
            if mode == AppendMode::Sequence {
                self.group_start_timestamp = Some(self.group_end_timestamp);
            } else {
                self.group_start_timestamp = None;
            }
        }
    }

    pub fn timestamp_offset(&self) -> f64 {
        self.timestamp_offset
    }

    pub fn set_timestamp_offset(&mut self, offset: f64) -> Result<(), MseError> {
        if !offset.is_finite() {
            return Err(MseError::InvalidArgument(
                "timestamp_offset must be finite".into(),
            ));
        }
        self.timestamp_offset = offset;
        if self.append_mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(offset);
        }
        Ok(())
    }

    pub fn group_start_timestamp(&self) -> Option<f64> {
        self.group_start_timestamp
    }

    pub fn group_end_timestamp(&self) -> f64 {
        self.group_end_timestamp
    }

    pub fn append_window_start(&self) -> f64 {
        self.append_window_start
    }

    pub fn append_window_end(&self) -> f64 {
        self.append_window_end
    }

    pub fn set_append_window(&mut self, start: f64, end: f64) -> Result<(), MseError> {
        if start.is_nan() || start < 0.0 || end.is_nan() || end <= start {
            return Err(MseError::InvalidArgument(format!(
                "invalid append window [{start}, {end}]"
            )));
        }
        self.append_window_start = start;
        self.append_window_end = end;
        Ok(())
    }

    pub fn highest_end_timestamp(&self) -> f64 {
        self.highest_end_timestamp
    }

    pub fn quota_bytes(&self) -> usize {
        self.quota_bytes
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub fn playback_position(&self) -> f64 {
        self.playback_position
    }

    pub fn set_playback_position(&mut self, pos: f64) {
        if pos.is_finite() && pos >= 0.0 {
            self.playback_position = pos;
        }
    }

    pub fn active_init_segment(&self) -> Option<&InitSegment> {
        self.active_init_id.and_then(|id| self.init_segments.get(&id))
    }

    pub fn init_segments(&self) -> &BTreeMap<u64, InitSegment> {
        &self.init_segments
    }

    pub fn media_segments(&self) -> &[MediaSegment] {
        &self.media_segments
    }

    pub fn abort(&mut self) {
        self.pending.reset_parser_state();
        if self.append_mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(self.group_end_timestamp);
        }
    }

    pub fn change_type(&mut self) {
        self.pending.reset_for_change_type();
        if self.append_mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(self.group_end_timestamp);
        }
    }

    pub fn append(&mut self, chunk: &[u8]) -> Result<AppendResult, MseError> {
        let parsed = self.pending.push_and_parse(chunk)?;
        if self.pending.pending_len() > self.quota_bytes {
            // Try freeing space before failing on pending chunk accumulation.
            let needed = self.pending.pending_len().saturating_sub(self.quota_bytes);
            if self.evict_bytes_if_needed(needed).is_err() && self.pending.pending_len() > self.quota_bytes {
                return Err(MseError::QuotaExceeded {
                    requested: chunk.len(),
                    used: self.used_bytes,
                    quota: self.quota_bytes,
                });
            }
        }
        self.process_parsed_segments(parsed)
    }

    pub fn end_of_stream(&mut self) -> Result<AppendResult, MseError> {
        let parsed = self.pending.flush()?;
        self.process_parsed_segments(parsed)
    }

    fn process_parsed_segments(
        &mut self,
        parsed: Vec<ParsedSegment>,
    ) -> Result<AppendResult, MseError> {
        let mut result = AppendResult::default();

        for item in parsed {
            match item {
                ParsedSegment::Init(init) => {
                    if let Some(ref cenc_init) = init.init_data_cenc {
                        result
                            .encrypted_init_data
                            .push(("cenc".to_string(), cenc_init.clone()));
                    }
                    self.active_init_id = Some(init.id);
                    self.init_segments.insert(init.id, init);
                    result.init_segments_parsed += 1;
                },
                ParsedSegment::Media(mut seg) => {
                    if let Some(ref cenc_init) = seg.init_data_cenc {
                        result
                            .encrypted_init_data
                            .push(("cenc".to_string(), cenc_init.clone()));
                    }
                    if self.append_mode == AppendMode::Sequence {
                        if let Some(gst) = self.group_start_timestamp.take() {
                            self.timestamp_offset = gst - seg.start_pts;
                            self.group_end_timestamp = gst;
                        }
                    }

                    let mut pts = seg.start_pts + self.timestamp_offset;
                    let mut end_pts = seg.end_pts + self.timestamp_offset;
                    let decode_end = seg.last_decode_end_pts + self.timestamp_offset;

                    // Apply append window filter.
                    if end_pts <= self.append_window_start || pts >= self.append_window_end {
                        continue;
                    }
                    pts = pts.max(self.append_window_start);
                    end_pts = end_pts.min(self.append_window_end);
                    if end_pts < pts {
                        continue;
                    }

                    seg.start_pts = pts;
                    seg.end_pts = end_pts;
                    seg.last_decode_end_pts = decode_end.max(end_pts);
                    if let Some(kp) = seg.keyframe_pts {
                        seg.keyframe_pts = Some((kp + self.timestamp_offset).clamp(pts, end_pts));
                    }

                    // Enforce per-SourceBuffer RAM quota using W3C Coded Frame Eviction.
                    let evicted = self.evict_bytes_if_needed(seg.byte_len)?;
                    result.evicted_bytes += evicted;

                    self.insert_media_segment(seg);
                    self.group_end_timestamp = self.group_end_timestamp.max(end_pts);
                    self.highest_end_timestamp = self.highest_end_timestamp.max(end_pts);
                    result.media_segments_parsed += 1;
                },
            }
        }

        self.prune_unused_init_segments();
        Ok(result)
    }

    fn insert_media_segment(&mut self, seg: MediaSegment) {
        let new_start = seg.start_pts;
        let new_end = seg.end_pts;

        // Remove any existing segments completely overlapped by the new segment,
        // and trim overlapping edges without breaking decode chains.
        let mut i = 0;
        let mut removed_dependency_end: Option<f64> = None;
        while i < self.media_segments.len() {
            let existing = &mut self.media_segments[i];
            if existing.end_pts <= new_start {
                i += 1;
                continue;
            }
            if existing.start_pts >= new_end {
                if let Some(dep_end) = removed_dependency_end {
                    if !existing.is_keyframe && existing.start_pts <= dep_end + BUFFERED_ADJACENCY_TOLERANCE_SECS {
                        let removed = self.media_segments.remove(i);
                        self.used_bytes = self.used_bytes.saturating_sub(removed.byte_len);
                        removed_dependency_end = Some(removed.end_pts);
                        continue;
                    }
                }
                break;
            }

            if existing.start_pts >= new_start && existing.end_pts <= new_end {
                let removed = self.media_segments.remove(i);
                self.used_bytes = self.used_bytes.saturating_sub(removed.byte_len);
                removed_dependency_end = Some(removed.end_pts);
            } else if existing.start_pts < new_start && existing.end_pts > new_end {
                existing.end_pts = new_start;
                i += 1;
            } else if existing.start_pts < new_start {
                existing.end_pts = new_start;
                i += 1;
            } else {
                // existing starts inside [new_start, new_end) and extends beyond new_end
                if existing.is_keyframe {
                    existing.start_pts = new_end;
                    i += 1;
                } else {
                    let removed = self.media_segments.remove(i);
                    self.used_bytes = self.used_bytes.saturating_sub(removed.byte_len);
                    removed_dependency_end = Some(removed.end_pts);
                }
            }
        }

        self.used_bytes += seg.byte_len;
        let insert_idx = self
            .media_segments
            .partition_point(|s| s.start_pts < seg.start_pts);
        self.media_segments.insert(insert_idx, seg);
    }

    fn coded_frame_group_ranges(&self) -> Vec<Range<usize>> {
        let mut groups = Vec::new();
        let len = self.media_segments.len();
        let mut i = 0;
        while i < len {
            let start = i;
            i += 1;
            while i < len
                && !self.media_segments[i].is_keyframe
                && self.media_segments[i].start_pts
                    <= self.media_segments[i - 1].end_pts + BUFFERED_ADJACENCY_TOLERANCE_SECS
            {
                i += 1;
            }
            groups.push(start..i);
        }
        groups
    }

    /// Evicts earliest non-playback-critical coded frame groups outside
    /// `[playback_pos - 2s, playback_pos + 30s]` until `used_bytes + incoming_bytes <= quota_bytes`.
    fn evict_bytes_if_needed(&mut self, incoming_bytes: usize) -> Result<usize, MseError> {
        if self.used_bytes + incoming_bytes <= self.quota_bytes {
            return Ok(0);
        }
        if incoming_bytes > self.quota_bytes {
            return Err(MseError::QuotaExceeded {
                requested: incoming_bytes,
                used: self.used_bytes,
                quota: self.quota_bytes,
            });
        }

        let protect_start = (self.playback_position - EVICTION_PROTECT_BEFORE_SECS).max(0.0);
        let protect_end = self.playback_position + EVICTION_PROTECT_AFTER_SECS;

        let mut total_evicted = 0usize;

        loop {
            if self.used_bytes + incoming_bytes <= self.quota_bytes {
                return Ok(total_evicted);
            }

            let groups = self.coded_frame_group_ranges();
            let candidate_group = groups.into_iter().find(|group| {
                let g_start = self.media_segments[group.start].start_pts;
                let g_end = self.media_segments[group.end - 1].end_pts;
                // Outside [protect_start, protect_end]
                g_end <= protect_start || g_start >= protect_end
            });

            let Some(group) = candidate_group else {
                return Err(MseError::QuotaExceeded {
                    requested: incoming_bytes,
                    used: self.used_bytes,
                    quota: self.quota_bytes,
                });
            };

            let drained: Vec<MediaSegment> = self.media_segments.drain(group).collect();
            for seg in drained {
                self.used_bytes = self.used_bytes.saturating_sub(seg.byte_len);
                total_evicted += seg.byte_len;
            }
        }
    }

    fn prune_unused_init_segments(&mut self) {
        let active_id = self.active_init_id;
        let used_ids: Vec<u64> = self.media_segments.iter().map(|s| s.init_id).collect();
        self.init_segments
            .retain(|&id, _| Some(id) == active_id || used_ids.contains(&id));
    }

    /// Returns monotonic sorted buffered ranges merged within `BUFFERED_ADJACENCY_TOLERANCE_SECS` (~0.1s).
    pub fn buffered(&self) -> Vec<Range<f64>> {
        let mut ranges: Vec<Range<f64>> = Vec::new();
        for seg in &self.media_segments {
            if seg.end_pts <= seg.start_pts {
                continue;
            }
            if let Some(last) = ranges.last_mut() {
                if seg.start_pts <= last.end + BUFFERED_ADJACENCY_TOLERANCE_SECS {
                    last.end = last.end.max(seg.end_pts);
                    continue;
                }
            }
            ranges.push(seg.start_pts..seg.end_pts);
        }
        ranges
    }

    /// W3C Coded Frame Removal Algorithm: removes segments intersecting `[start, end)`
    /// and continues removing dependent non-keyframe segments up to the next keyframe.
    pub fn remove(&mut self, start: f64, end: f64) -> Result<(), MseError> {
        self.remove_with_removed_ids(start, end).map(|_| ())
    }

    /// Same as [`Self::remove`], returning `(id, start_pts, end_pts)` for each removed segment.
    pub fn remove_with_removed_ids(
        &mut self,
        start: f64,
        end: f64,
    ) -> Result<Vec<(u64, f64, f64)>, MseError> {
        if start.is_nan() || start < 0.0 || end.is_nan() || end <= start {
            return Err(MseError::InvalidArgument(format!(
                "invalid remove range [{start}, {end}]"
            )));
        }

        let mut removed_ids = Vec::new();
        let mut i = 0;
        let mut removing_chain = false;
        let mut last_removed_end = 0.0f64;

        while i < self.media_segments.len() {
            let seg = &self.media_segments[i];
            let overlaps = seg.end_pts > start && seg.start_pts < end;

            if overlaps {
                let removed = self.media_segments.remove(i);
                self.used_bytes = self.used_bytes.saturating_sub(removed.byte_len);
                removing_chain = true;
                last_removed_end = removed.end_pts;
                removed_ids.push((removed.id, removed.start_pts, removed.end_pts));
                continue;
            }

            if removing_chain && seg.start_pts >= end {
                // Stop once we reach a keyframe or a discontinuity gap.
                if seg.is_keyframe
                    || seg.start_pts > last_removed_end + BUFFERED_ADJACENCY_TOLERANCE_SECS
                {
                    break;
                }
                let removed = self.media_segments.remove(i);
                self.used_bytes = self.used_bytes.saturating_sub(removed.byte_len);
                last_removed_end = removed.end_pts;
                removed_ids.push((removed.id, removed.start_pts, removed.end_pts));
                continue;
            }

            if seg.start_pts >= end {
                break;
            }
            i += 1;
        }

        self.prune_unused_init_segments();
        Ok(removed_ids)
    }

    fn seek_keyframe_index(&self, target_time: f64) -> Option<usize> {
        if !target_time.is_finite() || target_time < 0.0 {
            return None;
        }
        self.media_segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, seg)| seg.is_keyframe && seg.start_pts <= target_time + 1e-6)
            .map(|(idx, _)| idx)
            .or_else(|| {
                // Handle streams whose initial keyframe begins slightly after 0.0 (e.g. 0.04s).
                self.media_segments
                    .iter()
                    .enumerate()
                    .find(|(_, seg)| {
                        seg.is_keyframe
                            && seg.start_pts <= target_time + 0.25
                            && seg.end_pts > target_time - 0.05
                    })
                    .map(|(idx, _)| idx)
            })
    }

    /// Returns the `start_pts` of the keyframe selected for seeking to `target_time`.
    pub fn seek_keyframe_start(&self, target_time: f64) -> Option<f64> {
        self.seek_keyframe_index(target_time)
            .map(|idx| self.media_segments[idx].start_pts)
    }

    /// Returns the latest keyframe at or before `target_time`, its matching `InitSegment`,
    /// and all subsequent contiguous segments sharing that `InitSegment`.
    pub fn segments_for_seek(&self, target_time: f64) -> Option<(&InitSegment, Vec<&MediaSegment>)> {
        let kf_idx = self.seek_keyframe_index(target_time)?;

        let first = &self.media_segments[kf_idx];
        let init = self.init_segments.get(&first.init_id)?;
        let mut segments = vec![first];
        let mut prev_end = first.end_pts;

        for seg in &self.media_segments[kf_idx + 1..] {
            if seg.init_id != first.init_id {
                break;
            }
            if seg.start_pts > prev_end + BUFFERED_ADJACENCY_TOLERANCE_SECS {
                break;
            }
            prev_end = prev_end.max(seg.end_pts);
            segments.push(seg);
        }

        Some((init, segments))
    }

    /// Returns contiguous `(&InitSegment, &MediaSegment)` pairs starting from the keyframe
    /// at or before `target_time`, continuing across ABR `InitSegment` switches until a gap.
    pub fn segments_for_seek_abr(&self, target_time: f64) -> Vec<(&InitSegment, &MediaSegment)> {
        let Some(kf_idx) = self.seek_keyframe_index(target_time) else {
            return Vec::new();
        };

        let first = &self.media_segments[kf_idx];
        let Some(first_init) = self.init_segments.get(&first.init_id) else {
            return Vec::new();
        };

        let mut out = vec![(first_init, first)];
        let mut prev_end = first.end_pts;

        for seg in &self.media_segments[kf_idx + 1..] {
            if seg.start_pts > prev_end + BUFFERED_ADJACENCY_TOLERANCE_SECS {
                break;
            }
            let Some(init) = self.init_segments.get(&seg.init_id) else {
                break;
            };
            prev_end = prev_end.max(seg.end_pts);
            out.push((init, seg));
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn box_bytes(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(payload);
        out
    }

    fn full_box_bytes(fourcc: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut inner = Vec::with_capacity(4 + payload.len());
        inner.push(version);
        inner.push(((flags >> 16) & 0xFF) as u8);
        inner.push(((flags >> 8) & 0xFF) as u8);
        inner.push((flags & 0xFF) as u8);
        inner.extend_from_slice(payload);
        box_bytes(fourcc, &inner)
    }

    fn build_fmp4_init(track_id: u32, timescale: u32, handler: &[u8; 4], codec_4cc: &[u8; 4]) -> Vec<u8> {
        let mut ftyp_payload = Vec::new();
        ftyp_payload.extend_from_slice(b"iso5");
        ftyp_payload.extend_from_slice(&512u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"iso5mp41");
        let ftyp = box_bytes(b"ftyp", &ftyp_payload);

        let mut mvhd_payload = Vec::new();
        mvhd_payload.extend_from_slice(&0u32.to_be_bytes()); // creation
        mvhd_payload.extend_from_slice(&0u32.to_be_bytes()); // modification
        mvhd_payload.extend_from_slice(&timescale.to_be_bytes());
        mvhd_payload.extend_from_slice(&0u32.to_be_bytes()); // duration
        let mvhd = full_box_bytes(b"mvhd", 0, 0, &mvhd_payload);

        let mut tkhd_payload = Vec::new();
        tkhd_payload.extend_from_slice(&0u32.to_be_bytes()); // creation
        tkhd_payload.extend_from_slice(&0u32.to_be_bytes()); // modification
        tkhd_payload.extend_from_slice(&track_id.to_be_bytes());
        let tkhd = full_box_bytes(b"tkhd", 0, 3, &tkhd_payload);

        let mut mdhd_payload = Vec::new();
        mdhd_payload.extend_from_slice(&0u32.to_be_bytes());
        mdhd_payload.extend_from_slice(&0u32.to_be_bytes());
        mdhd_payload.extend_from_slice(&timescale.to_be_bytes());
        let mdhd = full_box_bytes(b"mdhd", 0, 0, &mdhd_payload);

        let mut hdlr_payload = Vec::new();
        hdlr_payload.extend_from_slice(&0u32.to_be_bytes());
        hdlr_payload.extend_from_slice(handler);
        hdlr_payload.extend_from_slice(&[0u8; 12]);
        let hdlr = full_box_bytes(b"hdlr", 0, 0, &hdlr_payload);

        let sample_entry = box_bytes(codec_4cc, &[0u8; 8]);
        let mut stsd_payload = Vec::new();
        stsd_payload.extend_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&sample_entry);
        let stsd = full_box_bytes(b"stsd", 0, 0, &stsd_payload);
        let stbl = box_bytes(b"stbl", &stsd);
        let minf = box_bytes(b"minf", &stbl);

        let mut mdia_payload = Vec::new();
        mdia_payload.extend_from_slice(&mdhd);
        mdia_payload.extend_from_slice(&hdlr);
        mdia_payload.extend_from_slice(&minf);
        let mdia = box_bytes(b"mdia", &mdia_payload);

        let mut trak_payload = Vec::new();
        trak_payload.extend_from_slice(&tkhd);
        trak_payload.extend_from_slice(&mdia);
        let trak = box_bytes(b"trak", &trak_payload);

        let mut trex_payload = Vec::new();
        trex_payload.extend_from_slice(&track_id.to_be_bytes());
        trex_payload.extend_from_slice(&1u32.to_be_bytes()); // desc idx
        trex_payload.extend_from_slice(&1000u32.to_be_bytes()); // default dur
        trex_payload.extend_from_slice(&128u32.to_be_bytes()); // default size
        trex_payload.extend_from_slice(&0x0101_0000u32.to_be_bytes()); // non-sync default
        let trex = full_box_bytes(b"trex", 0, 0, &trex_payload);
        let mvex = box_bytes(b"mvex", &trex);

        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&mvhd);
        moov_payload.extend_from_slice(&trak);
        moov_payload.extend_from_slice(&mvex);
        let moov = box_bytes(b"moov", &moov_payload);

        let mut out = ftyp;
        out.extend_from_slice(&moov);
        out
    }

    fn build_fmp4_media_segment(
        seq: u32,
        track_id: u32,
        base_decode_time: u64,
        sample_durations: &[u32],
        sample_flags: &[u32],
        sample_ctos_v1: Option<&[i32]>,
        mdat_payload_size: usize,
    ) -> Vec<u8> {
        let mfhd = full_box_bytes(b"mfhd", 0, 0, &seq.to_be_bytes());
        let tfhd = full_box_bytes(b"tfhd", 0, 0, &track_id.to_be_bytes());
        let tfdt = full_box_bytes(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());

        let mut tr_flags: u32 = 0x000100 | 0x000400;
        let version = if sample_ctos_v1.is_some() {
            tr_flags |= 0x000800;
            1u8
        } else {
            0u8
        };
        let mut trun_payload = Vec::new();
        trun_payload.extend_from_slice(&(sample_durations.len() as u32).to_be_bytes());
        for i in 0..sample_durations.len() {
            trun_payload.extend_from_slice(&sample_durations[i].to_be_bytes());
            trun_payload.extend_from_slice(&sample_flags[i].to_be_bytes());
            if let Some(ctos) = sample_ctos_v1 {
                trun_payload.extend_from_slice(&ctos[i].to_be_bytes());
            }
        }
        let trun = full_box_bytes(b"trun", version, tr_flags, &trun_payload);

        let mut traf_payload = Vec::new();
        traf_payload.extend_from_slice(&tfhd);
        traf_payload.extend_from_slice(&tfdt);
        traf_payload.extend_from_slice(&trun);
        let traf = box_bytes(b"traf", &traf_payload);

        let mut moof_payload = Vec::new();
        moof_payload.extend_from_slice(&mfhd);
        moof_payload.extend_from_slice(&traf);
        let moof = box_bytes(b"moof", &moof_payload);

        let mdat = box_bytes(b"mdat", &vec![0xAAu8; mdat_payload_size]);
        let mut out = moof;
        out.extend_from_slice(&mdat);
        out
    }

    fn ebml_vint_size(len: u64) -> Vec<u8> {
        // 2-byte VINT size (supports up to 16382)
        assert!(len < 0x3FFF);
        let encoded = (len as u16) | 0x4000;
        encoded.to_be_bytes().to_vec()
    }

    fn ebml_elem(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        out.extend_from_slice(&ebml_vint_size(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn build_webm_init(ts_scale_ns: u32, codec: &str) -> Vec<u8> {
        let ebml = ebml_elem(&[0x1A, 0x45, 0xDF, 0xA3], &[0x42, 0x86, 0x81, 0x01]);
        let ts_scale = ebml_elem(&[0x2A, 0xD7, 0xB1], &ts_scale_ns.to_be_bytes());
        let info = ebml_elem(&[0x15, 0x49, 0xA9, 0x66], &ts_scale);

        let mut track_entry_payload = Vec::new();
        track_entry_payload.extend_from_slice(&ebml_elem(&[0xD7], &[1])); // TrackNumber 1
        track_entry_payload.extend_from_slice(&ebml_elem(&[0x83], &[1])); // Video
        track_entry_payload.extend_from_slice(&ebml_elem(&[0x86], codec.as_bytes()));
        let track_entry = ebml_elem(&[0xAE], &track_entry_payload);
        let tracks = ebml_elem(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);

        let mut out = ebml;
        // Unknown-size Segment (0x01FFFFFFFFFFFFFF)
        out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&info);
        out.extend_from_slice(&tracks);
        out
    }

    fn build_webm_simple_block(track_num: u8, rel_ts: i16, keyframe: bool, data_len: usize) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(0x80 | track_num); // 1-byte VINT track number
        payload.extend_from_slice(&rel_ts.to_be_bytes());
        payload.push(if keyframe { 0x80 } else { 0x00 });
        payload.resize(payload.len() + data_len, 0x55);
        ebml_elem(&[0xA3], &payload)
    }

    fn build_webm_cluster(cluster_ts: u32, blocks: &[u8], unknown_size: bool) -> Vec<u8> {
        let mut payload = ebml_elem(&[0xE7], &cluster_ts.to_be_bytes());
        payload.extend_from_slice(blocks);
        if unknown_size {
            let mut out = vec![0x1F, 0x43, 0xB6, 0x75, 0xFF];
            out.extend_from_slice(&payload);
            out
        } else {
            ebml_elem(&[0x1F, 0x43, 0xB6, 0x75], &payload)
        }
    }

    #[test]
    fn fmp4_chunked_init_and_moof_mdat_with_v1_signed_cto() {
        let mut buf = MseTrackBuffer::new_video();
        let init = build_fmp4_init(1, 1000, b"vide", b"avc1");
        let seg1 = build_fmp4_media_segment(
            1,
            1,
            1000,
            &[500, 500],
            &[0x0200_0000, 0x0101_0000],
            Some(&[-100, 200]),
            64,
        );

        let mut combined = init;
        combined.extend_from_slice(&seg1);

        // Append split into awkward 7-byte chunks to verify streaming reassembly.
        let mut init_count = 0;
        let mut media_count = 0;
        for chunk in combined.chunks(7) {
            let res = buf.append(chunk).unwrap();
            init_count += res.init_segments_parsed;
            media_count += res.media_segments_parsed;
        }
        assert_eq!(init_count, 1);
        assert_eq!(media_count, 1);

        let active = buf.active_init_segment().unwrap();
        assert_eq!(active.container, MseContainer::IsoBmff);
        assert_eq!(active.tracks[0].codec.as_deref(), Some("avc1"));
        assert_eq!(active.tracks[0].timescale, 1000);

        // Sample 0: decode=1000, cto=-100 -> pts=0.9, dur=0.5 -> end=1.4
        // Sample 1: decode=1500, cto=+200 -> pts=1.7, dur=0.5 -> end=2.2
        let m = &buf.media_segments()[0];
        assert!(m.is_keyframe);
        assert!((m.start_pts - 0.9).abs() < 1e-6);
        assert!((m.end_pts - 2.2).abs() < 1e-6);
        assert_eq!(buf.buffered(), vec![0.9..2.2]);
    }

    #[test]
    fn webm_known_and_unknown_size_clusters_with_block_group() {
        let mut buf = MseTrackBuffer::new_video();
        let init = build_webm_init(1_000_000, "V_VP9");
        let r0 = buf.append(&init).unwrap();
        assert_eq!(r0.init_segments_parsed, 1);

        // Unknown-size Cluster at t=0ms with 2 SimpleBlocks (0ms, 40ms) -> median delta 40ms -> end 80ms
        let mut blocks1 = build_webm_simple_block(1, 0, true, 16);
        blocks1.extend_from_slice(&build_webm_simple_block(1, 40, false, 16));
        let cluster1 = build_webm_cluster(0, &blocks1, true);

        // Known-size Cluster at t=80ms with a BlockGroup (BlockDuration = 40ms)
        let mut bg_payload = Vec::new();
        let mut blk = vec![0x81, 0x00, 0x00, 0x00];
        blk.resize(12, 0x33);
        bg_payload.extend_from_slice(&ebml_elem(&[0xA1], &blk));
        bg_payload.extend_from_slice(&ebml_elem(&[0x9B], &40u16.to_be_bytes()));
        let bg = ebml_elem(&[0xA0], &bg_payload);
        let cluster2 = build_webm_cluster(80, &bg, false);

        // Append cluster1 (unknown-size) then cluster2 (closes cluster1 and parses cluster2).
        let r1 = buf.append(&cluster1).unwrap();
        assert_eq!(r1.media_segments_parsed, 0);
        let r2 = buf.append(&cluster2).unwrap();
        assert_eq!(r2.media_segments_parsed, 2);

        let segs = buf.media_segments();
        assert!(segs[0].is_keyframe);
        assert!((segs[0].start_pts - 0.0).abs() < 1e-6);
        assert!((segs[0].end_pts - 0.08).abs() < 1e-6);
        assert!(segs[1].is_keyframe); // BlockGroup without ReferenceBlock
        assert!((segs[1].start_pts - 0.08).abs() < 1e-6);
        assert!((segs[1].end_pts - 0.12).abs() < 1e-6);
    }

    #[test]
    fn sequence_vs_segments_timestamp_offset_and_append_window() {
        let init = build_fmp4_init(1, 1000, b"vide", b"avc1");
        let seg_raw = build_fmp4_media_segment(1, 1, 5000, &[1000], &[0x0200_0000], None, 32);

        // Segments mode with timestamp_offset = +2.0 -> [7.0, 8.0)
        let mut buf_seg = MseTrackBuffer::new_video();
        buf_seg.append(&init).unwrap();
        buf_seg.set_timestamp_offset(2.0).unwrap();
        buf_seg.append(&seg_raw).unwrap();
        assert_eq!(buf_seg.buffered(), vec![7.0..8.0]);

        // Sequence mode chains from 0.0 -> shifts 5.0 to 0.0..1.0, then next segment chains to 1.0..2.0
        let mut buf_seq = MseTrackBuffer::new_video();
        buf_seq.set_append_mode(AppendMode::Sequence);
        buf_seq.append(&init).unwrap();
        buf_seq.append(&seg_raw).unwrap();
        assert_eq!(buf_seq.buffered(), vec![0.0..1.0]);
        let seg_raw2 = build_fmp4_media_segment(2, 1, 6000, &[1000], &[0x0200_0000], None, 32);
        buf_seq.append(&seg_raw2).unwrap();
        assert_eq!(buf_seq.buffered(), vec![0.0..2.0]);

        // Append window [0.5, 1.5) clips/filters out-of-window segments
        buf_seq.set_append_window(0.5, 1.5).unwrap();
        let seg_outside =
            build_fmp4_media_segment(3, 1, 10_000, &[1000], &[0x0200_0000], None, 32);
        let res = buf_seq.append(&seg_outside).unwrap();
        assert_eq!(res.media_segments_parsed, 0);
    }

    #[test]
    fn keyframe_preserving_remove_and_abr_segments_for_seek() {
        let mut buf = MseTrackBuffer::new_video();
        let init_720p = build_fmp4_init(1, 1000, b"vide", b"avc1");
        let init_1080p = build_fmp4_init(1, 1000, b"vide", b"hvc1");

        buf.append(&init_720p).unwrap();
        // [0, 2) keyframe, [2, 4) non-keyframe, [4, 6) non-keyframe, [6, 8) keyframe
        let s0 = build_fmp4_media_segment(1, 1, 0, &[2000], &[0x0200_0000], None, 32);
        let s1 = build_fmp4_media_segment(2, 1, 2000, &[2000], &[0x0101_0000], None, 32);
        let s2 = build_fmp4_media_segment(3, 1, 4000, &[2000], &[0x0101_0000], None, 32);
        let s3 = build_fmp4_media_segment(4, 1, 6000, &[2000], &[0x0200_0000], None, 32);
        for s in [&s0, &s1, &s2, &s3] {
            buf.append(s).unwrap();
        }

        // Remove [1.0, 3.0): removes s0, s1 AND dependent non-keyframe s2 [4, 6), preserving keyframe s3 [6, 8)!
        buf.remove(1.0, 3.0).unwrap();
        assert_eq!(buf.buffered(), vec![6.0..8.0]);

        // Re-append [0, 4) under 720p, then ABR switch to 1080p at [8, 10)
        buf.append(&s0).unwrap();
        buf.append(&s1).unwrap();
        buf.append(&init_1080p).unwrap();
        let s4 = build_fmp4_media_segment(5, 1, 8000, &[2000], &[0x0200_0000], None, 32);
        buf.append(&s4).unwrap();

        let (seek_init_1, segs_1) = buf.segments_for_seek(2.5).unwrap();
        assert_eq!(seek_init_1.tracks[0].codec.as_deref(), Some("avc1"));
        assert_eq!(segs_1.len(), 2); // s0, s1 (stops at gap before s3)

        let (seek_init_2, segs_2) = buf.segments_for_seek(9.0).unwrap();
        assert_eq!(seek_init_2.tracks[0].codec.as_deref(), Some("hvc1"));
        assert_eq!(segs_2.len(), 1);
        assert!((segs_2[0].start_pts - 8.0).abs() < 1e-6);
    }

    #[test]
    fn quota_eviction_and_quota_exceeded() {
        let init = build_fmp4_init(1, 1000, b"vide", b"avc1");
        let seg_a = build_fmp4_media_segment(1, 1, 0, &[2000], &[0x0200_0000], None, 100);
        let seg_b = build_fmp4_media_segment(2, 1, 2000, &[2000], &[0x0200_0000], None, 100);
        let seg_c = build_fmp4_media_segment(3, 1, 10_000, &[2000], &[0x0200_0000], None, 100);
        let seg_d = build_fmp4_media_segment(4, 1, 12_000, &[2000], &[0x0200_0000], None, 100);

        let seg_len = seg_a.len();
        // Quota fits 2 segments comfortably, 3rd triggers eviction.
        let mut buf = MseTrackBuffer::with_quota(seg_len * 2 + 10);
        buf.append(&init).unwrap();
        buf.set_playback_position(10.0); // protects [8.0, 40.0]

        // Append [0, 2) and [10, 12)
        buf.append(&seg_a).unwrap();
        buf.append(&seg_c).unwrap();
        assert_eq!(buf.buffered(), vec![0.0..2.0, 10.0..12.0]);

        // Appending [12, 14) evicts [0, 2) (outside protected window) and succeeds.
        let res = buf.append(&seg_d).unwrap();
        assert_eq!(res.evicted_bytes, seg_len);
        assert_eq!(buf.buffered(), vec![10.0..14.0]);

        // Now both [10, 12) and [12, 14) are inside [8.0, 40.0]; appending [2, 4) must fail QuotaExceeded.
        let err = buf.append(&seg_b).unwrap_err();
        assert!(matches!(err, MseError::QuotaExceeded { .. }));
    }

    #[test]
    fn mpeg_audio_mp3_and_adts_aac_parsing_with_id3() {
        let mut buf = MseTrackBuffer::new_audio();
        // ID3v2.3 tag with 4 bytes payload
        let mut stream = Vec::new();
        stream.extend_from_slice(b"ID3\x03\x00\x00\x00\x00\x00\x04ABCD");

        // MPEG1 Layer III, 128kbps (idx=9), 44100Hz (idx=0), no padding -> frame_size = 144*128000/44100 = 417
        let mut mp3_frame = vec![0xFF, 0xFB, 0x90, 0x00];
        mp3_frame.resize(417, 0x11);
        stream.extend_from_slice(&mp3_frame);
        stream.extend_from_slice(&mp3_frame);

        let res = buf.append(&stream).unwrap();
        assert_eq!(res.init_segments_parsed, 1);
        assert_eq!(res.media_segments_parsed, 2);
        let expected_end = 2.0 * (1152.0 / 44100.0);
        let buffered = buf.buffered();
        assert_eq!(buffered.len(), 1);
        assert!((buffered[0].start - 0.0).abs() < 1e-9);
        assert!((buffered[0].end - expected_end).abs() < 1e-6);

        // ADTS AAC frame: 48000Hz (sr_idx=3), frame_length=16, 1 raw data block
        let mut adts_buf = MseTrackBuffer::new_audio();
        let mut adts_frame = vec![0xFF, 0xF1, 0x4C, 0x00, 0x02, 0x00, 0xFC];
        adts_frame.resize(16, 0x22);
        let res_aac = adts_buf.append(&adts_frame).unwrap();
        assert_eq!(res_aac.media_segments_parsed, 1);
        assert!((adts_buf.buffered()[0].end - (1024.0 / 48000.0)).abs() < 1e-6);
    }

    #[test]
    fn truncated_and_corrupt_inputs_never_panic() {
        let mut buf = MseTrackBuffer::new_video();
        // Truncated ftyp/moov does not panic and waits for more data.
        let init = build_fmp4_init(1, 1000, b"vide", b"avc1");
        assert!(buf.append(&init[..12]).is_ok());

        // Malicious trun with huge sample_count returns InvalidData cleanly.
        let mut bad_buf = MseTrackBuffer::new_video();
        bad_buf.append(&init).unwrap();
        let mfhd = full_box_bytes(b"mfhd", 0, 0, &1u32.to_be_bytes());
        let tfhd = full_box_bytes(b"tfhd", 0, 0, &1u32.to_be_bytes());
        let trun = full_box_bytes(b"trun", 0, 0x000100, &u32::MAX.to_be_bytes());
        let mut traf_p = tfhd;
        traf_p.extend_from_slice(&trun);
        let traf = box_bytes(b"traf", &traf_p);
        let mut moof_p = mfhd;
        moof_p.extend_from_slice(&traf);
        let moof = box_bytes(b"moof", &moof_p);
        assert!(matches!(bad_buf.append(&moof), Err(MseError::InvalidData(_))));
    }

    #[test]
    fn fmp4_styp_sidx_emsg_prft_free_before_moof_and_trex_defaults() {
        let mut buf = MseTrackBuffer::new_video();
        let init = build_fmp4_init(1, 1000, b"vide", b"avc1");
        buf.append(&init).unwrap();

        // Standalone styp + sidx + emsg + prft + free + uuid before moof+mdat.
        // Note: build_fmp4_init sets trex default_sample_duration=1000, default_sample_flags=0x0101_0000.
        let styp = box_bytes(b"styp", b"msdh\x00\x00\x00\x00msdh");
        let mut sidx_p = Vec::new();
        sidx_p.extend_from_slice(&1u32.to_be_bytes()); // ref_id
        sidx_p.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        sidx_p.extend_from_slice(&0u32.to_be_bytes()); // earliest_pts
        sidx_p.extend_from_slice(&0u32.to_be_bytes()); // first_offset
        sidx_p.extend_from_slice(&0u16.to_be_bytes()); // reserved
        sidx_p.extend_from_slice(&1u16.to_be_bytes()); // ref_count = 1
        sidx_p.extend_from_slice(&[0u8; 12]);
        let sidx = full_box_bytes(b"sidx", 0, 0, &sidx_p);
        let emsg = box_bytes(b"emsg", &[0u8; 16]);
        let prft = box_bytes(b"prft", &[0u8; 16]);
        let free = box_bytes(b"free", &[0u8; 8]);
        let uuid = box_bytes(b"uuid", &[0x11u8; 20]);

        // Standalone index append must succeed without blocking or erroring.
        let r_idx = buf.append(&[styp.as_slice(), sidx.as_slice()].concat()).unwrap();
        assert_eq!(r_idx.media_segments_parsed, 0);

        // moof with tfhd default-base-is-moof (0x020000) and NO sample defaults,
        // and trun using trex defaults + first_sample_flags sync (0x0200_0000) across 2 truns.
        let mfhd = full_box_bytes(b"mfhd", 0, 0, &1u32.to_be_bytes());
        let tfhd = full_box_bytes(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
        let tfdt = full_box_bytes(b"tfdt", 0, 0, &2000u32.to_be_bytes());
        let mut trun1_p = Vec::new();
        trun1_p.extend_from_slice(&2u32.to_be_bytes()); // 2 samples using trex default_sample_duration=1000
        trun1_p.extend_from_slice(&0x0200_0000u32.to_be_bytes()); // first_sample_flags
        let trun1 = full_box_bytes(b"trun", 0, 0x000004, &trun1_p);
        let trun2 = full_box_bytes(b"trun", 0, 0, &1u32.to_be_bytes());
        let mut traf_p = tfhd;
        traf_p.extend_from_slice(&tfdt);
        traf_p.extend_from_slice(&trun1);
        traf_p.extend_from_slice(&trun2);
        let traf = box_bytes(b"traf", &traf_p);
        let mut moof_p = mfhd;
        moof_p.extend_from_slice(&traf);
        let moof = box_bytes(b"moof", &moof_p);
        let mdat = box_bytes(b"mdat", &[0xBBu8; 48]);

        let seg = [emsg, prft, free, uuid, moof, mdat].concat();
        let r_seg = buf.append(&seg).unwrap();
        assert_eq!(r_seg.media_segments_parsed, 1);
        assert!(buf.media_segments()[0].is_keyframe);
        assert_eq!(buf.buffered(), vec![2.0..5.0]);
    }

    #[test]
    fn webm_youtube_init_with_seekhead_info_tracks_cues_emits_immediately() {
        let mut buf = MseTrackBuffer::new_video();
        let ebml = ebml_elem(&[0x1A, 0x45, 0xDF, 0xA3], &[0x42, 0x86, 0x81, 0x01]);
        let seek_head = ebml_elem(&[0x11, 0x4D, 0x9B, 0x74], &[0xEC, 0x82, 0x00, 0x00]);
        let ts_scale = ebml_elem(&[0x2A, 0xD7, 0xB1], &1_000_000u32.to_be_bytes());
        let info = ebml_elem(&[0x15, 0x49, 0xA9, 0x66], &ts_scale);

        let mut track_entry_payload = Vec::new();
        track_entry_payload.extend_from_slice(&ebml_elem(&[0xD7], &[1]));
        track_entry_payload.extend_from_slice(&ebml_elem(&[0x83], &[1]));
        track_entry_payload.extend_from_slice(&ebml_elem(&[0x86], b"V_VP9"));
        let track_entry = ebml_elem(&[0xAE], &track_entry_payload);
        let tracks = ebml_elem(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);
        let cues = ebml_elem(&[0x1C, 0x53, 0xBB, 0x6B], &[0xBB, 0x82, 0x00, 0x00]);

        let mut yt_init = ebml;
        yt_init.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]); // Segment (unknown size)
        yt_init.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        yt_init.extend_from_slice(&seek_head);
        yt_init.extend_from_slice(&info);
        yt_init.extend_from_slice(&tracks);
        yt_init.extend_from_slice(&cues);

        // Must emit InitSegment immediately without waiting for any Cluster or EOS!
        let res = buf.append(&yt_init).unwrap();
        assert_eq!(res.init_segments_parsed, 1);
        assert_eq!(
            buf.active_init_segment().unwrap().tracks[0].codec.as_deref(),
            Some("V_VP9")
        );

        let blocks = build_webm_simple_block(1, 0, true, 16);
        let cluster = build_webm_cluster(0, &blocks, false);
        let res_c = buf.append(&cluster).unwrap();
        assert_eq!(res_c.media_segments_parsed, 1);
    }

    #[test]
    fn webm_youtube_finite_segment_size_init_and_separate_clusters() {
        let mut buf = MseTrackBuffer::new_video();
        let ebml = ebml_elem(&[0x1A, 0x45, 0xDF, 0xA3], &[0x42, 0x86, 0x81, 0x01]);
        let seek_head = ebml_elem(&[0x11, 0x4D, 0x9B, 0x74], &[0xEC, 0x82, 0x00, 0x00]);
        let ts_scale = ebml_elem(&[0x2A, 0xD7, 0xB1], &1_000_000u32.to_be_bytes());
        let info = ebml_elem(&[0x15, 0x49, 0xA9, 0x66], &ts_scale);

        let mut track_entry_payload = Vec::new();
        track_entry_payload.extend_from_slice(&ebml_elem(&[0xD7], &[1]));
        track_entry_payload.extend_from_slice(&ebml_elem(&[0x83], &[1]));
        track_entry_payload.extend_from_slice(&ebml_elem(&[0x86], b"V_VP9"));
        let track_entry = ebml_elem(&[0xAE], &track_entry_payload);
        let tracks = ebml_elem(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);

        // Segment declares finite 50_000_000-byte size (8-byte EBML VINT: 0x01 | 50_000_000),
        // while only ~150B init chunk is appended first.
        let finite_seg_size = (0x0100_0000_0000_0000u64 | 50_000_000u64).to_be_bytes();
        let mut yt_init = ebml;
        yt_init.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        yt_init.extend_from_slice(&finite_seg_size);
        yt_init.extend_from_slice(&seek_head);
        yt_init.extend_from_slice(&info);
        yt_init.extend_from_slice(&tracks);

        let res_init = buf.append(&yt_init).unwrap();
        assert_eq!(res_init.init_segments_parsed, 1);
        assert_eq!(res_init.media_segments_parsed, 0);

        // Separate Cluster appends parse immediately in streaming mode.
        let mut b1 = build_webm_simple_block(1, 0, true, 32);
        b1.extend_from_slice(&build_webm_simple_block(1, 500, false, 32));
        let c1 = build_webm_cluster(0, &b1, false);
        let r1 = buf.append(&c1).unwrap();
        assert_eq!(r1.media_segments_parsed, 1);
        assert_eq!(buf.buffered(), vec![0.0..1.0]);

        let mut b2 = build_webm_simple_block(1, 0, true, 32);
        b2.extend_from_slice(&build_webm_simple_block(1, 500, false, 32));
        let c2 = build_webm_cluster(1000, &b2, false);
        let r2 = buf.append(&c2).unwrap();
        assert_eq!(r2.media_segments_parsed, 1);
        assert_eq!(buf.buffered(), vec![0.0..2.0]);
    }

    #[test]
    fn abr_seek_across_init_switches_and_change_type_container_switch() {
        let mut buf = MseTrackBuffer::new_video();
        let init_a = build_fmp4_init(1, 1000, b"vide", b"avc1");
        let init_b = build_fmp4_init(1, 1000, b"vide", b"avc3");
        buf.append(&init_a).unwrap();
        let s0 = build_fmp4_media_segment(1, 1, 0, &[2000], &[0x0200_0000], None, 32);
        buf.append(&s0).unwrap();

        buf.append(&init_b).unwrap();
        let s1 = build_fmp4_media_segment(2, 1, 2000, &[2000], &[0x0200_0000], None, 32);
        buf.append(&s1).unwrap();

        let abr = buf.segments_for_seek_abr(1.0);
        assert_eq!(abr.len(), 2);
        assert_eq!(abr[0].0.tracks[0].codec.as_deref(), Some("avc1"));
        assert_eq!(abr[1].0.tracks[0].codec.as_deref(), Some("avc3"));

        // changeType() from ISO-BMFF to WebM resets container detection while keeping buffered frames.
        buf.change_type();
        let webm_init = build_webm_init(1_000_000, "V_VP9");
        let r_init = buf.append(&webm_init).unwrap();
        assert_eq!(r_init.init_segments_parsed, 1);
        let b_webm = build_webm_simple_block(1, 0, true, 16);
        let c_webm = build_webm_cluster(4000, &b_webm, false);
        let r_c = buf.append(&c_webm).unwrap();
        assert_eq!(r_c.media_segments_parsed, 1);
        assert!(buf.buffered().iter().any(|r| r.start < 0.1 && r.end >= 4.0));
    }

    #[test]
    fn fmp4_cenc_encv_reports_frma_codec_and_synthesizes_clearkey_pssh() {
        let kid = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99];
        let frma = box_bytes(b"frma", b"avc1");
        let mut schm_p = vec![0u8; 12];
        schm_p[4..8].copy_from_slice(b"cenc");
        schm_p[8..12].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        let schm = box_bytes(b"schm", &schm_p);
        let mut tenc_p = vec![0u8; 24];
        tenc_p[6] = 1;
        tenc_p[7] = 8;
        tenc_p[8..24].copy_from_slice(&kid);
        let tenc = box_bytes(b"tenc", &tenc_p);
        let schi = box_bytes(b"schi", &tenc);
        let mut sinf_p = Vec::new();
        sinf_p.extend_from_slice(&frma);
        sinf_p.extend_from_slice(&schm);
        sinf_p.extend_from_slice(&schi);
        let sinf = box_bytes(b"sinf", &sinf_p);

        let mut encv_p = vec![0u8; 78];
        encv_p.extend_from_slice(&sinf);
        let encv = box_bytes(b"encv", &encv_p);

        let mut stsd_p = vec![0u8; 8];
        stsd_p[7] = 1;
        stsd_p.extend_from_slice(&encv);
        let stsd = box_bytes(b"stsd", &stsd_p);
        let stbl = box_bytes(b"stbl", &stsd);
        let minf = box_bytes(b"minf", &stbl);
        let mut hdlr_p = vec![0u8; 12];
        hdlr_p.extend_from_slice(b"vide");
        let hdlr = box_bytes(b"hdlr", &hdlr_p);
        let mut mdhd_p = vec![0u8; 24];
        mdhd_p[12..16].copy_from_slice(&1000u32.to_be_bytes());
        let mdhd = box_bytes(b"mdhd", &mdhd_p);
        let mut mdia_p = Vec::new();
        mdia_p.extend_from_slice(&mdhd);
        mdia_p.extend_from_slice(&hdlr);
        mdia_p.extend_from_slice(&minf);
        let mdia = box_bytes(b"mdia", &mdia_p);

        let mut tkhd_p = vec![0u8; 84];
        tkhd_p[12..16].copy_from_slice(&1u32.to_be_bytes());
        let tkhd = box_bytes(b"tkhd", &tkhd_p);
        let mut trak_p = Vec::new();
        trak_p.extend_from_slice(&tkhd);
        trak_p.extend_from_slice(&mdia);
        let trak = box_bytes(b"trak", &trak_p);

        let mut mvhd_p = vec![0u8; 20];
        mvhd_p[12..16].copy_from_slice(&1000u32.to_be_bytes());
        let mvhd = box_bytes(b"mvhd", &mvhd_p);
        let mut moov_p = Vec::new();
        moov_p.extend_from_slice(&mvhd);
        moov_p.extend_from_slice(&trak);
        let moov = box_bytes(b"moov", &moov_p);
        let ftyp = box_bytes(b"ftyp", b"isom\x00\x00\x02\x00isom");

        let mut init = Vec::new();
        init.extend_from_slice(&ftyp);
        init.extend_from_slice(&moov);

        let mut buf = MseTrackBuffer::new_video();
        let res = buf.append(&init).expect("parse encrypted init");
        assert_eq!(res.init_segments_parsed, 1);
        assert_eq!(
            buf.active_init_segment().unwrap().tracks[0].codec.as_deref(),
            Some("avc1")
        );
        assert_eq!(res.encrypted_init_data.len(), 1);
        assert_eq!(res.encrypted_init_data[0].0, "cenc");
        let kids = crate::cenc::extract_kids_from_cenc_pssh(&res.encrypted_init_data[0].1)
            .expect("valid synthesized pssh");
        assert_eq!(kids, vec![kid]);
    }
}
