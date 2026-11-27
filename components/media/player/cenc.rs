/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pure-Rust ISO-BMFF Common Encryption (CENC / ISO/IEC 23001-7) parser,
//! ClearKey key store, init-segment rewriter, and media-segment decryptor.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock};

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

/// W3C Common SystemID for ClearKey / Common Encryption (`1077efec-c0b2-4d02-ace3-3c1e52e2fb4b`).
pub const COMMON_PSSH_SYSTEM_ID: [u8; 16] = [
    0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02, 0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b,
];

/// PIFF / Smooth Streaming `senc` UUID box identifier (`a2394f52-5a9b-4f14-a244-6c427c648df4`).
const SENC_UUID: [u8; 16] = [
    0xa2, 0x39, 0x4f, 0x52, 0x5a, 0x9b, 0x4f, 0x14, 0xa2, 0x44, 0x6c, 0x42, 0x7c, 0x64, 0x8d, 0xf4,
];

type SessionKeyMap = HashMap<String, HashMap<[u8; 16], [u8; 16]>>;

/// Thread-safe store mapping 16-byte Key IDs (KID) to 16-byte AES-128 content keys,
/// partitioned by EME session ID so closing one session never evicts a shared KID
/// still owned by another active session.
#[derive(Clone, Debug, Default)]
pub struct ClearKeyStore {
    sessions: Arc<RwLock<SessionKeyMap>>,
}

impl ClearKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_key(&self, kid: [u8; 16], key: [u8; 16]) {
        self.set_session_key("", kid, key);
    }

    pub fn set_session_key(&self, session_id: &str, kid: [u8; 16], key: [u8; 16]) {
        self.sessions
            .write()
            .unwrap()
            .entry(session_id.to_owned())
            .or_default()
            .insert(kid, key);
    }

    pub fn get_key(&self, kid: &[u8; 16]) -> Option<[u8; 16]> {
        self.sessions
            .read()
            .unwrap()
            .values()
            .find_map(|session| session.get(kid).copied())
    }

    pub fn remove_key(&self, kid: &[u8; 16]) -> Option<[u8; 16]> {
        let mut sessions = self.sessions.write().unwrap();
        let removed = sessions.get_mut("").and_then(|session| session.remove(kid));
        if sessions.get("").is_some_and(HashMap::is_empty) {
            sessions.remove("");
        }
        removed
    }

    pub fn remove_session(&self, session_id: &str) {
        self.sessions.write().unwrap().remove(session_id);
    }

    pub fn clear(&self) {
        self.sessions.write().unwrap().clear();
    }

    pub fn len(&self) -> usize {
        let sessions = self.sessions.read().unwrap();
        if sessions.len() <= 1 {
            return sessions.values().next().map_or(0, HashMap::len);
        }
        let mut distinct = HashSet::new();
        for session in sessions.values() {
            for kid in session.keys() {
                distinct.insert(*kid);
            }
        }
        distinct.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions
            .read()
            .unwrap()
            .values()
            .all(HashMap::is_empty)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CencScheme {
    Cenc,
    Cbcs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackEncryptionInfo {
    pub track_id: u32,
    pub original_fourcc: [u8; 4],
    pub scheme: CencScheme,
    pub default_is_protected: bool,
    pub default_per_sample_iv_size: u8,
    pub default_kid: [u8; 16],
    pub default_crypt_byte_block: u8,
    pub default_skip_byte_block: u8,
    pub default_constant_iv: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CencDecryptError {
    MissingKey([u8; 16]),
    InvalidData(String),
}

impl fmt::Display for CencDecryptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKey(kid) => {
                write!(f, "missing ClearKey for KID ")?;
                for b in kid {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            },
            Self::InvalidData(msg) => write!(f, "invalid CENC data: {msg}"),
        }
    }
}

impl std::error::Error for CencDecryptError {}

// ---------------------------------------------------------------------------
// ISO-BMFF box scanning helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct BoxSpan {
    box_type: [u8; 4],
    start: usize,
    payload_start: usize,
    end: usize,
    type_offset: usize,
}

fn read_box_span(data: &[u8], offset: usize, container_end: usize) -> Result<BoxSpan, String> {
    if offset + 8 > container_end {
        return Err("truncated ISO-BMFF box header".into());
    }
    let size32 = u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]);
    let box_type = [
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ];
    let type_offset = offset + 4;
    let (total_size, header_size) = if size32 == 1 {
        if offset + 16 > container_end {
            return Err("truncated 64-bit ISO-BMFF box header".into());
        }
        let largesize = u64::from_be_bytes([
            data[offset + 8],
            data[offset + 9],
            data[offset + 10],
            data[offset + 11],
            data[offset + 12],
            data[offset + 13],
            data[offset + 14],
            data[offset + 15],
        ]);
        if largesize < 16 {
            return Err("invalid 64-bit ISO-BMFF box size < 16".into());
        }
        (largesize, 16usize)
    } else if size32 == 0 {
        ((container_end - offset) as u64, 8usize)
    } else {
        if size32 < 8 {
            return Err("invalid ISO-BMFF box size < 8".into());
        }
        (size32 as u64, 8usize)
    };

    let total_usize =
        usize::try_from(total_size).map_err(|_| "ISO-BMFF box size overflow".to_string())?;
    let end = offset
        .checked_add(total_usize)
        .ok_or_else(|| "ISO-BMFF box end overflow".to_string())?;
    if end > container_end {
        return Err("ISO-BMFF box extends past container".into());
    }
    Ok(BoxSpan {
        box_type,
        start: offset,
        payload_start: offset + header_size,
        end,
        type_offset,
    })
}

fn iter_boxes(data: &[u8], start: usize, end: usize) -> Result<Vec<BoxSpan>, String> {
    let mut boxes = Vec::new();
    let mut off = start;
    while off < end {
        let span = read_box_span(data, off, end)?;
        off = span.end;
        boxes.push(span);
    }
    Ok(boxes)
}

// ---------------------------------------------------------------------------
// PSSH extraction, KID parsing, and ClearKey PSSH synthesis
// ---------------------------------------------------------------------------

fn collect_pssh_recursive(data: &[u8], start: usize, end: usize, out: &mut Vec<u8>) {
    let Ok(boxes) = iter_boxes(data, start, end) else {
        return;
    };
    for bx in boxes {
        match &bx.box_type {
            b"pssh" => out.extend_from_slice(&data[bx.start..bx.end]),
            b"moov" | b"moof" | b"trak" | b"traf" => {
                collect_pssh_recursive(data, bx.payload_start, bx.end, out);
            },
            _ => {},
        }
    }
}

/// Scans an ISO-BMFF init/media segment (`moov`, `moof`, or top-level boxes) and
/// concatenates all full `pssh` boxes found.
pub fn extract_cenc_pssh_boxes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    collect_pssh_recursive(bytes, 0, bytes.len(), &mut out);
    out
}

/// Parses one or more concatenated `pssh` boxes and returns the deduplicated list
/// of 16-byte KIDs in discovery order.
pub fn extract_kids_from_cenc_pssh(init_data: &[u8]) -> Result<Vec<[u8; 16]>, String> {
    if init_data.is_empty() {
        return Err("empty cenc initData".into());
    }
    let boxes = iter_boxes(init_data, 0, init_data.len())?;
    let mut kids = Vec::new();

    for bx in boxes {
        if &bx.box_type != b"pssh" {
            return Err("cenc initData contains non-pssh box".into());
        }
        let payload = &init_data[bx.payload_start..bx.end];
        if payload.len() < 24 {
            return Err("truncated pssh box".into());
        }
        let version = payload[0];
        let system_id = &payload[4..20];
        let mut cursor = 20usize;

        if version == 1 {
            if cursor + 4 > payload.len() {
                return Err("truncated pssh v1 KID count".into());
            }
            let kid_count = u32::from_be_bytes([
                payload[cursor],
                payload[cursor + 1],
                payload[cursor + 2],
                payload[cursor + 3],
            ]) as usize;
            cursor += 4;
            let kid_bytes_len = kid_count
                .checked_mul(16)
                .ok_or_else(|| "pssh KID count overflow".to_string())?;
            if cursor + kid_bytes_len > payload.len() {
                return Err("truncated pssh v1 KID list".into());
            }
            for _ in 0..kid_count {
                let mut kid = [0u8; 16];
                kid.copy_from_slice(&payload[cursor..cursor + 16]);
                cursor += 16;
                if !kids.contains(&kid) {
                    kids.push(kid);
                }
            }
        } else if version != 0 {
            return Err(format!("unsupported pssh version {version}"));
        }

        if cursor + 4 > payload.len() {
            return Err("truncated pssh data size".into());
        }
        let data_size = u32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]) as usize;
        cursor += 4;
        if cursor + data_size > payload.len() {
            return Err("truncated pssh payload data".into());
        }

        // Fallback for ClearKey v0 pssh carrying raw 16-byte KIDs in Data when no KID list existed.
        if version == 0 &&
            system_id == COMMON_PSSH_SYSTEM_ID &&
            data_size >= 16 &&
            data_size % 16 == 0 &&
            kids.is_empty()
        {
            for chunk in payload[cursor..cursor + data_size].chunks_exact(16) {
                let mut kid = [0u8; 16];
                kid.copy_from_slice(chunk);
                if !kids.contains(&kid) {
                    kids.push(kid);
                }
            }
        }
    }

    Ok(kids)
}

/// Synthesizes a version-1 `pssh` box using the W3C Common SystemID
/// (`1077efec-c0b2-4d02-ace3-3c1e52e2fb4b`) for tracks that declare `tenc`
/// encryption without an explicit `pssh` box (e.g. `ffmpeg -encryption_scheme`).
pub fn synthesize_clearkey_pssh_v1(kids: &[[u8; 16]]) -> Vec<u8> {
    let total_size = 36 + kids.len() * 16;
    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(&(total_size as u32).to_be_bytes());
    out.extend_from_slice(b"pssh");
    out.extend_from_slice(&[1, 0, 0, 0]); // version=1, flags=0
    out.extend_from_slice(&COMMON_PSSH_SYSTEM_ID);
    out.extend_from_slice(&(kids.len() as u32).to_be_bytes());
    for kid in kids {
        out.extend_from_slice(kid);
    }
    out.extend_from_slice(&0u32.to_be_bytes()); // data_size = 0
    out
}

// ---------------------------------------------------------------------------
// `moov` encryption metadata parsing & init segment clear rewrite
// ---------------------------------------------------------------------------

fn sample_entry_header_skip(box_type: &[u8; 4], payload: &[u8]) -> Option<usize> {
    match box_type {
        b"encv" => Some(78),
        b"enca" => {
            if payload.len() >= 10 {
                let version = u16::from_be_bytes([payload[8], payload[9]]);
                match version {
                    0 => Some(28),
                    1 => Some(44),
                    2 => Some(64),
                    _ => Some(28),
                }
            } else {
                Some(28)
            }
        },
        b"enct" | b"encs" => Some(8),
        _ => None,
    }
}

struct ParsedSinf {
    original_fourcc: [u8; 4],
    scheme: CencScheme,
    default_is_protected: bool,
    default_per_sample_iv_size: u8,
    default_kid: [u8; 16],
    default_crypt_byte_block: u8,
    default_skip_byte_block: u8,
    default_constant_iv: Vec<u8>,
    sinf_type_offset: usize,
}

fn parse_sinf_box(data: &[u8], sinf_bx: BoxSpan) -> Option<ParsedSinf> {
    let children = iter_boxes(data, sinf_bx.payload_start, sinf_bx.end).ok()?;
    let mut original_fourcc: Option<[u8; 4]> = None;
    let mut scheme: Option<CencScheme> = None;
    let mut tenc_data: Option<(bool, u8, [u8; 16], u8, u8, Vec<u8>)> = None;

    for child in children {
        let payload = &data[child.payload_start..child.end];
        match &child.box_type {
            b"frma" if payload.len() >= 4 => {
                original_fourcc = Some([payload[0], payload[1], payload[2], payload[3]]);
            },
            b"schm" if payload.len() >= 8 => {
                let scheme_fourcc = &payload[4..8];
                scheme = match scheme_fourcc {
                    b"cenc" | b"cens" => Some(CencScheme::Cenc),
                    b"cbcs" | b"cbc1" => Some(CencScheme::Cbcs),
                    _ => None,
                };
            },
            b"schi" => {
                if let Ok(schi_children) = iter_boxes(data, child.payload_start, child.end) {
                    for sc in schi_children {
                        if &sc.box_type == b"tenc" {
                            let tp = &data[sc.payload_start..sc.end];
                            if tp.len() >= 24 {
                                let version = tp[0];
                                let (crypt, skip) = if version >= 1 {
                                    (tp[5] >> 4, tp[5] & 0x0f)
                                } else {
                                    (0, 0)
                                };
                                let is_protected = tp[6] != 0;
                                let per_sample_iv_size = tp[7];
                                let mut kid = [0u8; 16];
                                kid.copy_from_slice(&tp[8..24]);
                                let mut const_iv = Vec::new();
                                if is_protected && per_sample_iv_size == 0 && tp.len() >= 25 {
                                    let iv_sz = tp[24] as usize;
                                    if tp.len() >= 25 + iv_sz {
                                        const_iv.extend_from_slice(&tp[25..25 + iv_sz]);
                                    }
                                }
                                tenc_data = Some((
                                    is_protected,
                                    per_sample_iv_size,
                                    kid,
                                    crypt,
                                    skip,
                                    const_iv,
                                ));
                            }
                        }
                    }
                }
            },
            _ => {},
        }
    }

    let original_fourcc = original_fourcc?;
    let scheme = scheme.unwrap_or(CencScheme::Cenc);
    let (
        default_is_protected,
        default_per_sample_iv_size,
        default_kid,
        default_crypt_byte_block,
        default_skip_byte_block,
        default_constant_iv,
    ) = tenc_data?;

    Some(ParsedSinf {
        original_fourcc,
        scheme,
        default_is_protected,
        default_per_sample_iv_size,
        default_kid,
        default_crypt_byte_block,
        default_skip_byte_block,
        default_constant_iv,
        sinf_type_offset: sinf_bx.type_offset,
    })
}

fn parse_tkhd_id(payload: &[u8]) -> Option<u32> {
    if payload.len() < 4 {
        return None;
    }
    let version = payload[0];
    let id_offset = if version == 0 {
        12
    } else if version == 1 {
        20
    } else {
        return None;
    };
    if payload.len() < id_offset + 4 {
        return None;
    }
    Some(u32::from_be_bytes([
        payload[id_offset],
        payload[id_offset + 1],
        payload[id_offset + 2],
        payload[id_offset + 3],
    ]))
}

struct RewritePatch {
    offset: usize,
    fourcc: [u8; 4],
}

fn scan_moov_for_encryption(
    data: &[u8],
    moov_payload_start: usize,
    moov_end: usize,
    out_map: &mut HashMap<u32, TrackEncryptionInfo>,
    out_patches: &mut Vec<RewritePatch>,
) {
    let Ok(moov_children) = iter_boxes(data, moov_payload_start, moov_end) else {
        return;
    };
    for bx in moov_children {
        if &bx.box_type == b"pssh" {
            out_patches.push(RewritePatch {
                offset: bx.type_offset,
                fourcc: *b"free",
            });
            continue;
        }
        if &bx.box_type != b"trak" {
            continue;
        }
        let Ok(trak_children) = iter_boxes(data, bx.payload_start, bx.end) else {
            continue;
        };
        let mut track_id: Option<u32> = None;
        let mut stsd_span: Option<BoxSpan> = None;

        for tc in trak_children {
            match &tc.box_type {
                b"pssh" => {
                    out_patches.push(RewritePatch {
                        offset: tc.type_offset,
                        fourcc: *b"free",
                    });
                },
                b"tkhd" => track_id = parse_tkhd_id(&data[tc.payload_start..tc.end]),
                b"mdia" => {
                    if let Ok(mdia_children) = iter_boxes(data, tc.payload_start, tc.end) {
                        for mc in mdia_children {
                            if &mc.box_type == b"minf" {
                                if let Ok(minf_children) = iter_boxes(data, mc.payload_start, mc.end)
                                {
                                    for minfc in minf_children {
                                        if &minfc.box_type == b"stbl" {
                                            if let Ok(stbl_children) =
                                                iter_boxes(data, minfc.payload_start, minfc.end)
                                            {
                                                for sc in stbl_children {
                                                    if &sc.box_type == b"stsd" {
                                                        stsd_span = Some(sc);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                _ => {},
            }
        }

        let (Some(track_id), Some(stsd)) = (track_id, stsd_span) else {
            continue;
        };
        // `stsd` is a FullBox: 4 bytes version/flags + 4 bytes entry_count.
        if stsd.payload_start + 8 > stsd.end {
            continue;
        }
        let Ok(entries) = iter_boxes(data, stsd.payload_start + 8, stsd.end) else {
            continue;
        };
        for entry in entries {
            let entry_payload = &data[entry.payload_start..entry.end];
            let Some(skip) = sample_entry_header_skip(&entry.box_type, entry_payload) else {
                continue;
            };
            if entry.payload_start + skip > entry.end {
                continue;
            }
            let Ok(entry_children) = iter_boxes(data, entry.payload_start + skip, entry.end) else {
                continue;
            };
            for ec in entry_children {
                if &ec.box_type == b"sinf" {
                    if let Some(sinf) = parse_sinf_box(data, ec) {
                        out_patches.push(RewritePatch {
                            offset: entry.type_offset,
                            fourcc: sinf.original_fourcc,
                        });
                        out_patches.push(RewritePatch {
                            offset: sinf.sinf_type_offset,
                            fourcc: *b"free",
                        });
                        out_map.insert(
                            track_id,
                            TrackEncryptionInfo {
                                track_id,
                                original_fourcc: sinf.original_fourcc,
                                scheme: sinf.scheme,
                                default_is_protected: sinf.default_is_protected,
                                default_per_sample_iv_size: sinf.default_per_sample_iv_size,
                                default_kid: sinf.default_kid,
                                default_crypt_byte_block: sinf.default_crypt_byte_block,
                                default_skip_byte_block: sinf.default_skip_byte_block,
                                default_constant_iv: sinf.default_constant_iv,
                            },
                        );
                    }
                }
            }
        }
    }
}

fn collect_moov_encryption_and_patches(
    bytes: &[u8],
) -> (HashMap<u32, TrackEncryptionInfo>, Vec<RewritePatch>) {
    let mut map = HashMap::new();
    let mut patches = Vec::new();
    if let Ok(top_boxes) = iter_boxes(bytes, 0, bytes.len()) {
        let mut found_moov = false;
        for bx in top_boxes {
            match &bx.box_type {
                b"pssh" => patches.push(RewritePatch {
                    offset: bx.type_offset,
                    fourcc: *b"free",
                }),
                b"moov" => {
                    found_moov = true;
                    scan_moov_for_encryption(
                        bytes,
                        bx.payload_start,
                        bx.end,
                        &mut map,
                        &mut patches,
                    );
                },
                _ => {},
            }
        }
        if !found_moov {
            // Also allow callers passing `moov` payload bytes directly.
            scan_moov_for_encryption(bytes, 0, bytes.len(), &mut map, &mut patches);
        }
    }
    (map, patches)
}

/// Parses `moov` (either wrapped inside an init segment or bare `moov` box/payload)
/// for encrypted tracks (`encv`/`enca` -> `sinf` -> `frma`/`schm`/`schi`/`tenc`).
pub fn parse_moov_encryption(moov_bytes: &[u8]) -> HashMap<u32, TrackEncryptionInfo> {
    collect_moov_encryption_and_patches(moov_bytes).0
}

/// If `init_bytes` contains encrypted sample entries (`encv`/`enca`), returns a
/// length-preserving copy rewritten into an unencrypted ISO-BMFF init segment
/// (`encv`/`enca` replaced with `frma`, and `sinf`/`pssh` replaced with `free`)
/// together with the per-track encryption metadata.
pub fn rewrite_init_segment_to_clear(
    init_bytes: &[u8],
) -> Option<(Vec<u8>, HashMap<u32, TrackEncryptionInfo>)> {
    let (map, patches) = collect_moov_encryption_and_patches(init_bytes);
    if map.is_empty() {
        return None;
    }
    let mut out = init_bytes.to_vec();
    for patch in patches {
        if patch.offset + 4 <= out.len() {
            out[patch.offset..patch.offset + 4].copy_from_slice(&patch.fourcc);
        }
    }
    Some((out, map))
}

// ---------------------------------------------------------------------------
// Media segment (`moof` + `mdat`) CENC / CBCS decryption
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct TfhdInfo {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_base_is_moof: bool,
    default_sample_size: u32,
}

fn parse_tfhd_info(payload: &[u8]) -> Result<TfhdInfo, CencDecryptError> {
    if payload.len() < 8 {
        return Err(CencDecryptError::InvalidData("truncated tfhd".into()));
    }
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    let track_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let mut off = 8usize;

    let base_data_offset = if flags & 0x000001 != 0 {
        if off + 8 > payload.len() {
            return Err(CencDecryptError::InvalidData(
                "truncated tfhd base_data_offset".into(),
            ));
        }
        let val = u64::from_be_bytes(payload[off..off + 8].try_into().unwrap());
        off += 8;
        Some(val)
    } else {
        None
    };

    if flags & 0x000002 != 0 {
        off += 4;
    }
    if flags & 0x000008 != 0 {
        off += 4;
    }

    let default_sample_size = if flags & 0x000010 != 0 {
        if off + 4 > payload.len() {
            return Err(CencDecryptError::InvalidData(
                "truncated tfhd default_sample_size".into(),
            ));
        }
        u32::from_be_bytes(payload[off..off + 4].try_into().unwrap())
    } else {
        0
    };

    Ok(TfhdInfo {
        track_id,
        base_data_offset,
        default_base_is_moof: (flags & 0x020000) != 0,
        default_sample_size,
    })
}

struct TrunInfo {
    data_offset: Option<i32>,
    sample_sizes: Vec<u32>,
}

fn parse_trun_info(payload: &[u8], default_sample_size: u32) -> Result<TrunInfo, CencDecryptError> {
    if payload.len() < 8 {
        return Err(CencDecryptError::InvalidData("truncated trun".into()));
    }
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    let sample_count = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
    let mut off = 8usize;

    let data_offset = if flags & 0x000001 != 0 {
        if off + 4 > payload.len() {
            return Err(CencDecryptError::InvalidData(
                "truncated trun data_offset".into(),
            ));
        }
        let v = i32::from_be_bytes(payload[off..off + 4].try_into().unwrap());
        off += 4;
        Some(v)
    } else {
        None
    };

    if flags & 0x000004 != 0 {
        off += 4;
    }

    let has_duration = (flags & 0x000100) != 0;
    let has_size = (flags & 0x000200) != 0;
    let has_flags = (flags & 0x000400) != 0;
    let has_cto = (flags & 0x000800) != 0;

    let per_sample = (has_duration as usize + has_size as usize + has_flags as usize + has_cto as usize) * 4;
    let required = sample_count
        .checked_mul(per_sample)
        .ok_or_else(|| CencDecryptError::InvalidData("trun sample_count overflow".into()))?;
    if off + required > payload.len() {
        return Err(CencDecryptError::InvalidData(
            "trun sample table exceeds box length".into(),
        ));
    }

    let mut sample_sizes = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if has_duration {
            off += 4;
        }
        let sz = if has_size {
            let v = u32::from_be_bytes(payload[off..off + 4].try_into().unwrap());
            off += 4;
            v
        } else {
            default_sample_size
        };
        if has_flags {
            off += 4;
        }
        if has_cto {
            off += 4;
        }
        sample_sizes.push(sz);
    }

    Ok(TrunInfo {
        data_offset,
        sample_sizes,
    })
}

#[derive(Clone, Debug)]
struct SubsampleEntry {
    clear_bytes: u16,
    protected_bytes: u32,
}

#[derive(Clone, Debug)]
struct SampleAuxInfo {
    iv: Vec<u8>,
    subsamples: Vec<SubsampleEntry>,
}

fn parse_senc_payload(
    payload: &[u8],
    per_sample_iv_size: u8,
) -> Result<Vec<SampleAuxInfo>, CencDecryptError> {
    if payload.len() < 8 {
        return Err(CencDecryptError::InvalidData("truncated senc".into()));
    }
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    let use_subsamples = (flags & 0x000002) != 0;
    let sample_count = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
    let iv_len = per_sample_iv_size as usize;

    let mut off = 8usize;
    let mut entries = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if off + iv_len > payload.len() {
            return Err(CencDecryptError::InvalidData("truncated senc IV".into()));
        }
        let iv = payload[off..off + iv_len].to_vec();
        off += iv_len;

        let mut subsamples = Vec::new();
        if use_subsamples {
            if off + 2 > payload.len() {
                return Err(CencDecryptError::InvalidData(
                    "truncated senc subsample_count".into(),
                ));
            }
            let sub_count = u16::from_be_bytes([payload[off], payload[off + 1]]) as usize;
            off += 2;
            let sub_bytes = sub_count
                .checked_mul(6)
                .ok_or_else(|| CencDecryptError::InvalidData("senc subsample overflow".into()))?;
            if off + sub_bytes > payload.len() {
                return Err(CencDecryptError::InvalidData(
                    "truncated senc subsample entries".into(),
                ));
            }
            for _ in 0..sub_count {
                let clear_bytes = u16::from_be_bytes([payload[off], payload[off + 1]]);
                let protected_bytes = u32::from_be_bytes([
                    payload[off + 2],
                    payload[off + 3],
                    payload[off + 4],
                    payload[off + 5],
                ]);
                off += 6;
                subsamples.push(SubsampleEntry {
                    clear_bytes,
                    protected_bytes,
                });
            }
        }
        entries.push(SampleAuxInfo { iv, subsamples });
    }
    Ok(entries)
}

fn parse_saiz_payload(payload: &[u8]) -> Result<Vec<u8>, CencDecryptError> {
    if payload.len() < 9 {
        return Err(CencDecryptError::InvalidData("truncated saiz".into()));
    }
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    let mut off = 4usize;
    if flags & 0x000001 != 0 {
        if off + 8 > payload.len() {
            return Err(CencDecryptError::InvalidData("truncated saiz aux_info".into()));
        }
        off += 8;
    }
    if off + 5 > payload.len() {
        return Err(CencDecryptError::InvalidData(
            "truncated saiz sample_count".into(),
        ));
    }
    let default_size = payload[off];
    let sample_count =
        u32::from_be_bytes(payload[off + 1..off + 5].try_into().unwrap()) as usize;
    off += 5;

    if default_size != 0 {
        Ok(vec![default_size; sample_count])
    } else {
        if off + sample_count > payload.len() {
            return Err(CencDecryptError::InvalidData(
                "truncated saiz sample_info_size table".into(),
            ));
        }
        Ok(payload[off..off + sample_count].to_vec())
    }
}

fn is_cenc_aux_info_box(payload: &[u8]) -> bool {
    if payload.len() < 4 {
        return false;
    }
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    if flags & 0x000001 == 0 {
        return true;
    }
    payload.len() >= 8 && matches!(&payload[4..8], b"cenc" | b"cens" | b"cbcs" | b"cbc1")
}

fn parse_saio_payload(payload: &[u8]) -> Result<Vec<u64>, CencDecryptError> {
    if payload.len() < 8 {
        return Err(CencDecryptError::InvalidData("truncated saio".into()));
    }
    let version = payload[0];
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    let mut off = 4usize;
    if flags & 0x000001 != 0 {
        if off + 8 > payload.len() {
            return Err(CencDecryptError::InvalidData("truncated saio aux_info".into()));
        }
        off += 8;
    }
    if off + 4 > payload.len() {
        return Err(CencDecryptError::InvalidData(
            "truncated saio entry_count".into(),
        ));
    }
    let entry_count =
        u32::from_be_bytes(payload[off..off + 4].try_into().unwrap()) as usize;
    off += 4;
    let width = if version == 0 { 4 } else { 8 };
    let needed = entry_count
        .checked_mul(width)
        .ok_or_else(|| CencDecryptError::InvalidData("saio entry_count overflow".into()))?;
    if off + needed > payload.len() {
        return Err(CencDecryptError::InvalidData(
            "truncated saio offset table".into(),
        ));
    }
    let mut offsets = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let v = if version == 0 {
            u32::from_be_bytes(payload[off..off + 4].try_into().unwrap()) as u64
        } else {
            u64::from_be_bytes(payload[off..off + 8].try_into().unwrap())
        };
        off += width;
        offsets.push(v);
    }
    Ok(offsets)
}

fn parse_aux_from_saiz_saio(
    data: &[u8],
    traf_base: usize,
    saiz_sizes: &[u8],
    saio_offsets: &[u64],
    per_sample_iv_size: u8,
) -> Result<Vec<SampleAuxInfo>, CencDecryptError> {
    let iv_len = per_sample_iv_size as usize;
    let mut entries = Vec::with_capacity(saiz_sizes.len());
    if saio_offsets.is_empty() {
        return Err(CencDecryptError::InvalidData(
            "saio has zero offset entries".into(),
        ));
    }

    let mut sequential_cursor: Option<usize> = None;
    for (idx, &entry_sz) in saiz_sizes.iter().enumerate() {
        let entry_off = if saio_offsets.len() == 1 {
            if let Some(cur) = sequential_cursor {
                cur
            } else {
                let off_usize = usize::try_from(saio_offsets[0]).map_err(|_| {
                    CencDecryptError::InvalidData("saio offset overflow".into())
                })?;
                traf_base.checked_add(off_usize).ok_or_else(|| {
                    CencDecryptError::InvalidData("saio offset overflow".into())
                })?
            }
        } else {
            let off_val = *saio_offsets.get(idx).ok_or_else(|| {
                CencDecryptError::InvalidData("saio offset count < saiz sample_count".into())
            })?;
            let off_usize = usize::try_from(off_val)
                .map_err(|_| CencDecryptError::InvalidData("saio offset overflow".into()))?;
            traf_base
                .checked_add(off_usize)
                .ok_or_else(|| CencDecryptError::InvalidData("saio offset overflow".into()))?
        };

        let sz = entry_sz as usize;
        let end = entry_off
            .checked_add(sz)
            .ok_or_else(|| CencDecryptError::InvalidData("saiz entry overflow".into()))?;
        if end > data.len() {
            return Err(CencDecryptError::InvalidData(
                "saiz/saio entry extends past segment".into(),
            ));
        }
        if saio_offsets.len() == 1 {
            sequential_cursor = Some(end);
        }

        let entry = &data[entry_off..end];
        if entry.len() < iv_len {
            return Err(CencDecryptError::InvalidData(
                "saiz entry smaller than IV".into(),
            ));
        }
        let iv = entry[..iv_len].to_vec();
        let mut p = iv_len;
        let mut subsamples = Vec::new();
        if p + 2 <= entry.len() {
            let sub_count = u16::from_be_bytes([entry[p], entry[p + 1]]) as usize;
            p += 2;
            if p + sub_count * 6 > entry.len() {
                return Err(CencDecryptError::InvalidData(
                    "saiz entry subsamples truncated".into(),
                ));
            }
            for _ in 0..sub_count {
                subsamples.push(SubsampleEntry {
                    clear_bytes: u16::from_be_bytes([entry[p], entry[p + 1]]),
                    protected_bytes: u32::from_be_bytes([
                        entry[p + 2],
                        entry[p + 3],
                        entry[p + 4],
                        entry[p + 5],
                    ]),
                });
                p += 6;
            }
        }
        entries.push(SampleAuxInfo { iv, subsamples });
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// AES-128-CTR (`cenc`) and AES-128-CBC pattern (`cbcs`) primitives
// ---------------------------------------------------------------------------

struct Aes128CtrStream {
    cipher: Aes128,
    counter_block: [u8; 16],
    keystream: [u8; 16],
    ks_offset: usize,
}

impl Aes128CtrStream {
    fn new(key: &[u8; 16], iv: &[u8]) -> Result<Self, CencDecryptError> {
        let mut counter_block = [0u8; 16];
        match iv.len() {
            8 => counter_block[..8].copy_from_slice(iv),
            16 => counter_block.copy_from_slice(iv),
            len => {
                return Err(CencDecryptError::InvalidData(format!(
                    "unsupported CENC IV length {len}"
                )));
            },
        }
        let cipher = Aes128::new(key.into());
        Ok(Self {
            cipher,
            counter_block,
            keystream: [0u8; 16],
            ks_offset: 16,
        })
    }

    fn increment_counter(&mut self) {
        // ISO/IEC 23001-7 Section 9.4: standard 128-bit big-endian counter increment
        // (lower 64 bits for 8-byte IV, full 128-bit wrap for 16-byte IV).
        for byte in self.counter_block.iter_mut().rev() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
    }

    fn apply_keystream(&mut self, data: &mut [u8]) {
        for byte in data {
            if self.ks_offset == 16 {
                self.keystream = self.counter_block;
                self.cipher.encrypt_block((&mut self.keystream).into());
                self.increment_counter();
                self.ks_offset = 0;
            }
            *byte ^= self.keystream[self.ks_offset];
            self.ks_offset += 1;
        }
    }
}

fn decrypt_cbcs_range(
    key: &[u8; 16],
    iv: &[u8],
    crypt_blocks: u8,
    skip_blocks: u8,
    data: &mut [u8],
) -> Result<(), CencDecryptError> {
    let mut prev_chain = [0u8; 16];
    match iv.len() {
        8 => prev_chain[..8].copy_from_slice(iv),
        16 => prev_chain.copy_from_slice(iv),
        len => {
            return Err(CencDecryptError::InvalidData(format!(
                "unsupported CBCS IV length {len}"
            )));
        },
    }
    let cipher = Aes128::new(key.into());

    let full_blocks = data.len() / 16;
    let pattern_len = (crypt_blocks as usize) + (skip_blocks as usize);

    for blk_idx in 0..full_blocks {
        let encrypted = if pattern_len == 0 {
            true
        } else {
            (blk_idx % pattern_len) < (crypt_blocks as usize)
        };
        if !encrypted {
            continue;
        }
        let start = blk_idx * 16;
        let block_slice = &mut data[start..start + 16];
        let mut ciphertext = [0u8; 16];
        ciphertext.copy_from_slice(block_slice);

        let mut decrypted = ciphertext;
        cipher.decrypt_block((&mut decrypted).into());
        for i in 0..16 {
            block_slice[i] = decrypted[i] ^ prev_chain[i];
        }
        prev_chain = ciphertext;
    }
    Ok(())
}

fn decrypt_sample_slice(
    scheme: CencScheme,
    key: &[u8; 16],
    iv: &[u8],
    subsamples: &[SubsampleEntry],
    crypt_blocks: u8,
    skip_blocks: u8,
    sample: &mut [u8],
) -> Result<(), CencDecryptError> {
    match scheme {
        CencScheme::Cenc => {
            let mut ctr = Aes128CtrStream::new(key, iv)?;
            if subsamples.is_empty() {
                ctr.apply_keystream(sample);
            } else {
                let mut pos = 0usize;
                for sub in subsamples {
                    let clear = sub.clear_bytes as usize;
                    let prot = sub.protected_bytes as usize;
                    pos = pos
                        .checked_add(clear)
                        .ok_or_else(|| CencDecryptError::InvalidData("subsample offset overflow".into()))?;
                    let end = pos
                        .checked_add(prot)
                        .ok_or_else(|| CencDecryptError::InvalidData("subsample end overflow".into()))?;
                    if end > sample.len() {
                        return Err(CencDecryptError::InvalidData(
                            "subsample bounds exceed sample length".into(),
                        ));
                    }
                    ctr.apply_keystream(&mut sample[pos..end]);
                    pos = end;
                }
            }
        },
        CencScheme::Cbcs => {
            if subsamples.is_empty() {
                decrypt_cbcs_range(key, iv, crypt_blocks, skip_blocks, sample)?;
            } else {
                let mut pos = 0usize;
                for sub in subsamples {
                    let clear = sub.clear_bytes as usize;
                    let prot = sub.protected_bytes as usize;
                    pos = pos
                        .checked_add(clear)
                        .ok_or_else(|| CencDecryptError::InvalidData("subsample offset overflow".into()))?;
                    let end = pos
                        .checked_add(prot)
                        .ok_or_else(|| CencDecryptError::InvalidData("subsample end overflow".into()))?;
                    if end > sample.len() {
                        return Err(CencDecryptError::InvalidData(
                            "subsample bounds exceed sample length".into(),
                        ));
                    }
                    if prot > 0 {
                        decrypt_cbcs_range(
                            key,
                            iv,
                            crypt_blocks,
                            skip_blocks,
                            &mut sample[pos..end],
                        )?;
                    }
                    pos = end;
                }
            }
        },
    }
    Ok(())
}

/// Decrypts all protected samples in an ISO-BMFF media segment (`moof` + `mdat`)
/// using `enc_map` (from `moov`) and `keys`, and replaces `senc`/`saiz`/`saio`/`pssh`
/// box types with `free` in-place to preserve all `moof`/`trun` offsets.
pub fn decrypt_media_segment(
    seg_bytes: &[u8],
    enc_map: &HashMap<u32, TrackEncryptionInfo>,
    keys: &ClearKeyStore,
) -> Result<Vec<u8>, CencDecryptError> {
    let top_boxes =
        iter_boxes(seg_bytes, 0, seg_bytes.len()).map_err(CencDecryptError::InvalidData)?;
    let mut out = seg_bytes.to_vec();
    let mut free_patches: Vec<usize> = Vec::new();

    for (top_idx, bx) in top_boxes.iter().enumerate() {
        if &bx.box_type == b"pssh" {
            free_patches.push(bx.type_offset);
            continue;
        }
        if &bx.box_type != b"moof" {
            continue;
        }

        let moof_start = bx.start;
        // Fallback mdat payload offset if a trun omits `data_offset` and `base_data_offset`.
        let fallback_mdat_payload = top_boxes[top_idx + 1..]
            .iter()
            .find(|b| &b.box_type == b"mdat")
            .map(|b| b.payload_start);

        let moof_children = iter_boxes(seg_bytes, bx.payload_start, bx.end)
            .map_err(CencDecryptError::InvalidData)?;

        let mut prev_traf_data_end: Option<usize> = None;

        for mc in moof_children {
            if &mc.box_type == b"pssh" {
                free_patches.push(mc.type_offset);
                continue;
            }
            if &mc.box_type != b"traf" {
                continue;
            }

            let traf_children = iter_boxes(seg_bytes, mc.payload_start, mc.end)
                .map_err(CencDecryptError::InvalidData)?;

            let mut tfhd: Option<TfhdInfo> = None;
            let mut truns: Vec<TrunInfo> = Vec::new();
            let mut senc_payload: Option<&[u8]> = None;
            let mut saiz_payload: Option<&[u8]> = None;
            let mut saio_payload: Option<&[u8]> = None;
            let mut traf_free_offsets: Vec<usize> = Vec::new();

            for tc in &traf_children {
                let tp = &seg_bytes[tc.payload_start..tc.end];
                match &tc.box_type {
                    b"tfhd" => tfhd = Some(parse_tfhd_info(tp)?),
                    b"senc" => {
                        senc_payload = Some(tp);
                        traf_free_offsets.push(tc.type_offset);
                    },
                    b"uuid" if tp.len() >= 16 && tp[..16] == SENC_UUID => {
                        senc_payload = Some(&tp[16..]);
                        traf_free_offsets.push(tc.type_offset);
                    },
                    b"saiz" if is_cenc_aux_info_box(tp) => {
                        saiz_payload = Some(tp);
                        traf_free_offsets.push(tc.type_offset);
                    },
                    b"saio" if is_cenc_aux_info_box(tp) => {
                        saio_payload = Some(tp);
                        traf_free_offsets.push(tc.type_offset);
                    },
                    _ => {},
                }
            }

            let tfhd =
                tfhd.ok_or_else(|| CencDecryptError::InvalidData("traf missing tfhd".into()))?;
            for tc in &traf_children {
                if &tc.box_type == b"trun" {
                    let tp = &seg_bytes[tc.payload_start..tc.end];
                    truns.push(parse_trun_info(tp, tfhd.default_sample_size)?);
                }
            }

            let traf_base: usize = if let Some(bdo) = tfhd.base_data_offset {
                usize::try_from(bdo)
                    .map_err(|_| CencDecryptError::InvalidData("base_data_offset overflow".into()))?
            } else if tfhd.default_base_is_moof || prev_traf_data_end.is_none() {
                moof_start
            } else {
                prev_traf_data_end.unwrap()
            };

            let mut sample_ranges: Vec<(usize, usize)> = Vec::new();
            let mut cursor: Option<usize> = None;
            for (trun_idx, trun) in truns.iter().enumerate() {
                let mut cur = if let Some(off) = trun.data_offset {
                    let abs = (traf_base as i128) + (off as i128);
                    usize::try_from(abs).map_err(|_| {
                        CencDecryptError::InvalidData("negative trun data_offset".into())
                    })?
                } else if trun_idx == 0 {
                    if tfhd.base_data_offset.is_some()
                        || (!tfhd.default_base_is_moof && prev_traf_data_end.is_some())
                    {
                        traf_base
                    } else {
                        fallback_mdat_payload.ok_or_else(|| {
                            CencDecryptError::InvalidData("missing mdat for trun".into())
                        })?
                    }
                } else {
                    cursor.unwrap_or(traf_base)
                };

                for &sz in &trun.sample_sizes {
                    let sz_usize = sz as usize;
                    let end = cur.checked_add(sz_usize).ok_or_else(|| {
                        CencDecryptError::InvalidData("sample offset overflow".into())
                    })?;
                    if end > out.len() {
                        return Err(CencDecryptError::InvalidData(
                            "sample byte range exceeds media segment".into(),
                        ));
                    }
                    sample_ranges.push((cur, end));
                    cur = end;
                }
                cursor = Some(cur);
            }
            if let Some(end) = cursor {
                prev_traf_data_end = Some(end);
            }

            let Some(enc) = enc_map.get(&tfhd.track_id) else {
                continue;
            };
            if !enc.default_is_protected {
                free_patches.extend(traf_free_offsets);
                continue;
            }

            let key = keys
                .get_key(&enc.default_kid)
                .ok_or(CencDecryptError::MissingKey(enc.default_kid))?;

            let aux_entries = if let Some(senc) = senc_payload {
                parse_senc_payload(senc, enc.default_per_sample_iv_size)?
            } else if let (Some(saiz), Some(saio)) = (saiz_payload, saio_payload) {
                let saiz_sizes = parse_saiz_payload(saiz)?;
                let saio_offsets = parse_saio_payload(saio)?;
                parse_aux_from_saiz_saio(
                    seg_bytes,
                    traf_base,
                    &saiz_sizes,
                    &saio_offsets,
                    enc.default_per_sample_iv_size,
                )?
            } else if enc.default_per_sample_iv_size == 0 && !enc.default_constant_iv.is_empty() {
                vec![
                    SampleAuxInfo {
                        iv: enc.default_constant_iv.clone(),
                        subsamples: Vec::new(),
                    };
                    sample_ranges.len()
                ]
            } else {
                return Err(CencDecryptError::InvalidData(
                    "encrypted traf missing senc and saiz/saio".into(),
                ));
            };

            if aux_entries.len() < sample_ranges.len() {
                return Err(CencDecryptError::InvalidData(format!(
                    "encryption sample count ({}) < trun sample count ({})",
                    aux_entries.len(),
                    sample_ranges.len()
                )));
            }

            for ((start, end), aux) in sample_ranges.iter().zip(aux_entries.iter()) {
                let iv = if aux.iv.is_empty() && !enc.default_constant_iv.is_empty() {
                    enc.default_constant_iv.as_slice()
                } else {
                    aux.iv.as_slice()
                };
                decrypt_sample_slice(
                    enc.scheme,
                    &key,
                    iv,
                    &aux.subsamples,
                    enc.default_crypt_byte_block,
                    enc.default_skip_byte_block,
                    &mut out[*start..*end],
                )?;
            }

            free_patches.extend(traf_free_offsets);
        }
    }

    for off in free_patches {
        if off + 4 <= out.len() {
            out[off..off + 4].copy_from_slice(b"free");
        }
    }

    Ok(out)
}

fn wrap_box_32(fourcc: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>, String> {
    let total = payload
        .len()
        .checked_add(8)
        .ok_or_else(|| "ISO-BMFF box size overflow".to_string())?;
    let sz = u32::try_from(total).map_err(|_| "ISO-BMFF box exceeds 4 GiB".to_string())?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&sz.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    Ok(out)
}

fn encrypt_cbcs_range(
    key: &[u8; 16],
    iv: &[u8],
    crypt_blocks: u8,
    skip_blocks: u8,
    data: &mut [u8],
) -> Result<(), String> {
    let mut prev_chain = [0u8; 16];
    match iv.len() {
        8 => prev_chain[..8].copy_from_slice(iv),
        16 => prev_chain.copy_from_slice(iv),
        len => return Err(format!("unsupported CBCS IV length {len}")),
    }
    let cipher = Aes128::new(key.into());
    let full_blocks = data.len() / 16;
    let pattern_len = (crypt_blocks as usize) + (skip_blocks as usize);

    for blk_idx in 0..full_blocks {
        let encrypted = if pattern_len == 0 {
            true
        } else {
            (blk_idx % pattern_len) < (crypt_blocks as usize)
        };
        if !encrypted {
            continue;
        }
        let start = blk_idx * 16;
        let block_slice = &mut data[start..start + 16];
        let mut blk = [0u8; 16];
        for i in 0..16 {
            blk[i] = block_slice[i] ^ prev_chain[i];
        }
        cipher.encrypt_block((&mut blk).into());
        block_slice.copy_from_slice(&blk);
        prev_chain = blk;
    }
    Ok(())
}

fn build_cbcs_sinf(
    original_fourcc: [u8; 4],
    kid: [u8; 16],
    constant_iv: [u8; 16],
) -> Result<Vec<u8>, String> {
    let frma = wrap_box_32(b"frma", &original_fourcc)?;
    let mut schm_p = vec![0u8; 12];
    schm_p[4..8].copy_from_slice(b"cbcs");
    schm_p[8..12].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    let schm = wrap_box_32(b"schm", &schm_p)?;

    let mut tenc_p = Vec::with_capacity(41);
    tenc_p.push(1); // version = 1
    tenc_p.extend_from_slice(&[0, 0, 0]); // flags
    tenc_p.push(0); // reserved
    tenc_p.push((1u8 << 4) | 9u8); // 1:9 pattern
    tenc_p.push(1); // default_isProtected = 1
    tenc_p.push(0); // default_per_sample_iv_size = 0 (constant_iv)
    tenc_p.extend_from_slice(&kid);
    tenc_p.push(16); // default_constant_iv_size = 16
    tenc_p.extend_from_slice(&constant_iv);
    let tenc = wrap_box_32(b"tenc", &tenc_p)?;
    let schi = wrap_box_32(b"schi", &tenc)?;

    let mut sinf_p = Vec::with_capacity(frma.len() + schm.len() + schi.len());
    sinf_p.extend_from_slice(&frma);
    sinf_p.extend_from_slice(&schm);
    sinf_p.extend_from_slice(&schi);
    wrap_box_32(b"sinf", &sinf_p)
}

fn rewrite_stsd_to_cbcs(
    data: &[u8],
    stsd: BoxSpan,
    kid: [u8; 16],
    constant_iv: [u8; 16],
) -> Result<(Vec<u8>, Option<bool>), String> {
    if stsd.payload_start + 8 > stsd.end {
        return Err("truncated stsd box".into());
    }
    let mut stsd_payload = data[stsd.payload_start..stsd.payload_start + 8].to_vec();
    let entries = iter_boxes(data, stsd.payload_start + 8, stsd.end)?;
    let mut encrypted_is_avc: Option<bool> = None;

    for entry in entries {
        match &entry.box_type {
            b"encv" | b"enca" | b"enct" | b"encs" => {
                return Err("clear_fmp4 already contains an encrypted sample entry".into());
            },
            b"avc1" | b"avc3" => {
                let sinf = build_cbcs_sinf(entry.box_type, kid, constant_iv)?;
                let mut payload = data[entry.payload_start..entry.end].to_vec();
                payload.extend_from_slice(&sinf);
                stsd_payload.extend_from_slice(&wrap_box_32(b"encv", &payload)?);
                encrypted_is_avc = Some(true);
            },
            b"mp4a" => {
                let sinf = build_cbcs_sinf(entry.box_type, kid, constant_iv)?;
                let mut payload = data[entry.payload_start..entry.end].to_vec();
                payload.extend_from_slice(&sinf);
                stsd_payload.extend_from_slice(&wrap_box_32(b"enca", &payload)?);
                encrypted_is_avc = Some(false);
            },
            _ => {
                stsd_payload.extend_from_slice(&data[entry.start..entry.end]);
            },
        }
    }

    Ok((wrap_box_32(b"stsd", &stsd_payload)?, encrypted_is_avc))
}

fn rewrite_container_child(
    data: &[u8],
    container: BoxSpan,
    target_fourcc: &[u8; 4],
    mut transform: impl FnMut(BoxSpan) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let children = iter_boxes(data, container.payload_start, container.end)?;
    let mut payload = Vec::new();
    for child in children {
        if &child.box_type == target_fourcc {
            payload.extend_from_slice(&transform(child)?);
        } else {
            payload.extend_from_slice(&data[child.start..child.end]);
        }
    }
    wrap_box_32(&container.box_type, &payload)
}

fn rewrite_moov_to_cbcs(
    data: &[u8],
    moov: BoxSpan,
    kid: [u8; 16],
    constant_iv: [u8; 16],
) -> Result<(Vec<u8>, HashMap<u32, bool>), String> {
    let mut enc_tracks = HashMap::new();
    let moov_children = iter_boxes(data, moov.payload_start, moov.end)?;
    let mut moov_payload = Vec::new();

    for mc in moov_children {
        if &mc.box_type != b"trak" {
            moov_payload.extend_from_slice(&data[mc.start..mc.end]);
            continue;
        }

        let trak_children = iter_boxes(data, mc.payload_start, mc.end)?;
        let mut track_id: Option<u32> = None;
        for tc in &trak_children {
            if &tc.box_type == b"tkhd" {
                track_id = parse_tkhd_id(&data[tc.payload_start..tc.end]);
            }
        }

        let mut track_is_avc: Option<bool> = None;
        let new_trak = rewrite_container_child(data, mc, b"mdia", |mdia| {
            rewrite_container_child(data, mdia, b"minf", |minf| {
                rewrite_container_child(data, minf, b"stbl", |stbl| {
                    rewrite_container_child(data, stbl, b"stsd", |stsd| {
                        let (new_stsd, is_avc) =
                            rewrite_stsd_to_cbcs(data, stsd, kid, constant_iv)?;
                        if is_avc.is_some() {
                            track_is_avc = is_avc;
                        }
                        Ok(new_stsd)
                    })
                })
            })
        })?;

        if let (Some(tid), Some(is_avc)) = (track_id, track_is_avc) {
            enc_tracks.insert(tid, is_avc);
        }
        moov_payload.extend_from_slice(&new_trak);
    }

    if enc_tracks.is_empty() {
        return Err("clear_fmp4 moov contains no supported avc1/avc3/mp4a tracks".into());
    }

    moov_payload.extend_from_slice(&synthesize_clearkey_pssh_v1(&[kid]));
    Ok((wrap_box_32(b"moov", &moov_payload)?, enc_tracks))
}

fn parse_avc_nalu_subsamples(sample: &[u8]) -> Option<Vec<SubsampleEntry>> {
    if sample.is_empty() {
        return None;
    }
    let mut p = 0usize;
    let mut subsamples = Vec::new();
    while p < sample.len() {
        if p + 4 > sample.len() {
            return None;
        }
        let nal_len =
            u32::from_be_bytes([sample[p], sample[p + 1], sample[p + 2], sample[p + 3]]) as usize;
        let end = p.checked_add(4)?.checked_add(nal_len)?;
        if end > sample.len() {
            return None;
        }
        if nal_len == 0 {
            subsamples.push(SubsampleEntry {
                clear_bytes: 4,
                protected_bytes: 0,
            });
        } else {
            subsamples.push(SubsampleEntry {
                clear_bytes: 5,
                protected_bytes: (nal_len - 1) as u32,
            });
        }
        p = end;
    }
    Some(subsamples)
}

fn patch_tfra_offsets(payload: &mut [u8], moof_offset_map: &[(u64, u64)]) -> Result<(), String> {
    if payload.len() < 16 {
        return Err("truncated tfra box".into());
    }
    let version = payload[0];
    let length_byte = payload[11];
    let len_traf = ((length_byte >> 4) & 0x03) as usize + 1;
    let len_trun = ((length_byte >> 2) & 0x03) as usize + 1;
    let len_sample = (length_byte & 0x03) as usize + 1;
    let entry_count = u32::from_be_bytes([payload[12], payload[13], payload[14], payload[15]]) as usize;
    let time_sz = if version == 1 { 8usize } else { 4usize };
    let off_sz = time_sz;
    let entry_sz = time_sz + off_sz + len_traf + len_trun + len_sample;
    if 16 + entry_count * entry_sz > payload.len() {
        return Err("tfra entries exceed box length".into());
    }
    for i in 0..entry_count {
        let moof_off_pos = 16 + i * entry_sz + time_sz;
        let old_off = if version == 1 {
            u64::from_be_bytes(payload[moof_off_pos..moof_off_pos + 8].try_into().unwrap())
        } else {
            u32::from_be_bytes(payload[moof_off_pos..moof_off_pos + 4].try_into().unwrap()) as u64
        };
        if let Some(&(_, new_off)) = moof_offset_map.iter().find(|&&(o, _)| o == old_off) {
            if version == 1 {
                payload[moof_off_pos..moof_off_pos + 8].copy_from_slice(&new_off.to_be_bytes());
            } else {
                let new_u32 = u32::try_from(new_off)
                    .map_err(|_| "updated tfra moof_offset overflows u32".to_string())?;
                payload[moof_off_pos..moof_off_pos + 4].copy_from_slice(&new_u32.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Encrypts a clear fragmented MP4 (`ftyp` + `moov` + `moof`/`mdat` pairs) into a
/// Common-Encryption `cbcs` stream using AES-128-CBC 1:9 pattern encryption and a
/// 16-byte `constant_iv` (`tenc` v1, `default_per_sample_iv_size = 0`, W3C Common
/// SystemID `pssh` v1, `avc1`/`avc3` -> `encv`, `mp4a` -> `enca`).
pub fn encrypt_clear_fmp4_to_cbcs(
    clear_fmp4: &[u8],
    kid: [u8; 16],
    key: [u8; 16],
    constant_iv: [u8; 16],
) -> Result<Vec<u8>, String> {
    let top_boxes = iter_boxes(clear_fmp4, 0, clear_fmp4.len())?;
    let mut out = Vec::with_capacity(clear_fmp4.len() + 1024);
    let mut enc_tracks: Option<HashMap<u32, bool>> = None;
    let mut moof_offset_map: Vec<(u64, u64)> = Vec::new();
    let mut idx = 0usize;

    while idx < top_boxes.len() {
        let bx = top_boxes[idx];
        match &bx.box_type {
            b"moov" => {
                let (new_moov, map) = rewrite_moov_to_cbcs(clear_fmp4, bx, kid, constant_iv)?;
                enc_tracks = Some(map);
                out.extend_from_slice(&new_moov);
                idx += 1;
            },
            b"moof" => {
                let tracks = enc_tracks
                    .as_ref()
                    .ok_or_else(|| "clear_fmp4 contains moof before moov".to_string())?;
                let mdat_rel_idx = top_boxes[idx + 1..]
                    .iter()
                    .position(|b| &b.box_type == b"mdat")
                    .ok_or_else(|| "clear_fmp4 moof missing following mdat".to_string())?;
                let mdat_bx = top_boxes[idx + 1 + mdat_rel_idx];
                let mut mdat_payload = clear_fmp4[mdat_bx.payload_start..mdat_bx.end].to_vec();

                let moof_children = iter_boxes(clear_fmp4, bx.payload_start, bx.end)?;
                let mut prev_traf_data_end: Option<usize> = None;

                struct PendingTraf {
                    tfhd: TfhdInfo,
                    base_is_moof: bool,
                    other_children: Vec<Vec<u8>>,
                    truns: Vec<Vec<u8>>,
                    senc: Option<Vec<u8>>,
                }

                let mut trafs: Vec<PendingTraf> = Vec::new();
                let mut non_traf_prefix_len = 0usize;
                let mut moof_parts: Vec<Result<Vec<u8>, usize>> = Vec::new();
                let mut total_senc_added = 0usize;

                for mc in moof_children {
                    if &mc.box_type != b"traf" {
                        let raw = clear_fmp4[mc.start..mc.end].to_vec();
                        non_traf_prefix_len += raw.len();
                        moof_parts.push(Ok(raw));
                        continue;
                    }

                    let traf_children = iter_boxes(clear_fmp4, mc.payload_start, mc.end)?;
                    let mut tfhd: Option<TfhdInfo> = None;
                    let mut trun_infos: Vec<TrunInfo> = Vec::new();
                    let mut trun_bytes: Vec<Vec<u8>> = Vec::new();
                    let mut other_children: Vec<Vec<u8>> = Vec::new();

                    for tc in &traf_children {
                        match &tc.box_type {
                            b"tfhd" => {
                                tfhd = Some(
                                    parse_tfhd_info(&clear_fmp4[tc.payload_start..tc.end])
                                        .map_err(|e| e.to_string())?,
                                );
                                other_children.push(clear_fmp4[tc.start..tc.end].to_vec());
                            },
                            b"trun" => {
                                trun_bytes.push(clear_fmp4[tc.start..tc.end].to_vec());
                            },
                            _ => {
                                other_children.push(clear_fmp4[tc.start..tc.end].to_vec());
                            },
                        }
                    }

                    let tfhd = tfhd.ok_or_else(|| "traf missing tfhd".to_string())?;
                    for tc in &traf_children {
                        if &tc.box_type == b"trun" {
                            trun_infos.push(
                                parse_trun_info(
                                    &clear_fmp4[tc.payload_start..tc.end],
                                    tfhd.default_sample_size,
                                )
                                .map_err(|e| e.to_string())?,
                            );
                        }
                    }

                    let (traf_base, base_is_moof): (usize, bool) = if let Some(bdo) =
                        tfhd.base_data_offset
                    {
                        let bdo_usize = usize::try_from(bdo)
                            .map_err(|_| "tfhd base_data_offset overflow".to_string())?;
                        (bdo_usize, bdo == bx.start as u64)
                    } else if !tfhd.default_base_is_moof &&
                        let Some(prev_end) = prev_traf_data_end
                    {
                        (prev_end, false)
                    } else {
                        (bx.start, true)
                    };

                    let mut sample_ranges: Vec<(usize, usize)> = Vec::new();
                    let mut cursor: Option<usize> = None;
                    for (trun_idx, trun) in trun_infos.iter().enumerate() {
                        let mut cur = if let Some(off) = trun.data_offset {
                            let abs = (traf_base as i128) + (off as i128);
                            usize::try_from(abs)
                                .map_err(|_| "negative trun data_offset".to_string())?
                        } else if trun_idx == 0 {
                            if tfhd.base_data_offset.is_some() ||
                                (!tfhd.default_base_is_moof && prev_traf_data_end.is_some())
                            {
                                traf_base
                            } else {
                                mdat_bx.payload_start
                            }
                        } else {
                            cursor.unwrap_or(traf_base)
                        };

                        for &sz in &trun.sample_sizes {
                            let end = cur
                                .checked_add(sz as usize)
                                .ok_or_else(|| "sample offset overflow".to_string())?;
                            if cur < mdat_bx.payload_start || end > mdat_bx.end {
                                return Err("sample byte range outside mdat".into());
                            }
                            sample_ranges.push((cur - mdat_bx.payload_start, end - mdat_bx.payload_start));
                            cur = end;
                        }
                        cursor = Some(cur);
                    }
                    if let Some(end) = cursor {
                        prev_traf_data_end = Some(end);
                    }

                    let senc = if let Some(&is_avc) = tracks.get(&tfhd.track_id) {
                        let parsed_nalus: Option<Vec<Vec<SubsampleEntry>>> = if is_avc {
                            sample_ranges
                                .iter()
                                .map(|&(s, e)| parse_avc_nalu_subsamples(&mdat_payload[s..e]))
                                .collect()
                        } else {
                            None
                        };

                        if let Some(all_subs) = parsed_nalus {
                            let mut senc_p = Vec::new();
                            senc_p.extend_from_slice(&[0, 0, 0, 2]); // flags=2 (UseSubsampleEncryption)
                            senc_p.extend_from_slice(&(sample_ranges.len() as u32).to_be_bytes());
                            for (&(s, e), subs) in sample_ranges.iter().zip(all_subs.iter()) {
                                let sample = &mut mdat_payload[s..e];
                                let mut pos = 0usize;
                                for sub in subs {
                                    pos += sub.clear_bytes as usize;
                                    let prot_end = pos + sub.protected_bytes as usize;
                                    if sub.protected_bytes > 0 {
                                        encrypt_cbcs_range(
                                            &key,
                                            &constant_iv,
                                            1,
                                            9,
                                            &mut sample[pos..prot_end],
                                        )?;
                                    }
                                    pos = prot_end;
                                }
                                let sub_count = u16::try_from(subs.len())
                                    .map_err(|_| "too many NALU subsamples in sample".to_string())?;
                                senc_p.extend_from_slice(&sub_count.to_be_bytes());
                                for sub in subs {
                                    senc_p.extend_from_slice(&sub.clear_bytes.to_be_bytes());
                                    senc_p.extend_from_slice(&sub.protected_bytes.to_be_bytes());
                                }
                            }
                            let senc_box = wrap_box_32(b"senc", &senc_p)?;
                            total_senc_added += senc_box.len();
                            Some(senc_box)
                        } else {
                            for &(s, e) in &sample_ranges {
                                encrypt_cbcs_range(&key, &constant_iv, 1, 9, &mut mdat_payload[s..e])?;
                            }
                            None
                        }
                    } else {
                        None
                    };

                    let traf_idx = trafs.len();
                    trafs.push(PendingTraf {
                        tfhd,
                        base_is_moof,
                        other_children,
                        truns: trun_bytes,
                        senc,
                    });
                    moof_parts.push(Err(traf_idx));
                }

                let new_moof_start = out.len();
                let mut trafs_total_len = 0usize;
                for traf in &trafs {
                    let inner_len: usize = traf.other_children.iter().map(Vec::len).sum::<usize>() +
                        traf.truns.iter().map(Vec::len).sum::<usize>() +
                        traf.senc.as_ref().map_or(0, Vec::len);
                    trafs_total_len += 8 + inner_len;
                }
                let new_moof_size = 8 + non_traf_prefix_len + trafs_total_len;
                let inter_len: usize = top_boxes[idx + 1..idx + 1 + mdat_rel_idx]
                    .iter()
                    .map(|b| b.end - b.start)
                    .sum();
                let new_mdat_payload_start = new_moof_start + new_moof_size + inter_len + 8;

                let mut built_trafs: Vec<Vec<u8>> = Vec::with_capacity(trafs.len());
                for mut traf in trafs {
                    if let Some(old_bdo) = traf.tfhd.base_data_offset {
                        let new_bdo = if old_bdo == bx.start as u64 {
                            new_moof_start as u64
                        } else if old_bdo == mdat_bx.payload_start as u64 {
                            new_mdat_payload_start as u64
                        } else {
                            let delta = (new_moof_start as i128) - (bx.start as i128);
                            u64::try_from((old_bdo as i128) + delta)
                                .map_err(|_| "base_data_offset underflow".to_string())?
                        };
                        if let Some(tfhd_box) = traf
                            .other_children
                            .iter_mut()
                            .find(|b| b.len() >= 24 && &b[4..8] == b"tfhd")
                        {
                            tfhd_box[16..24].copy_from_slice(&new_bdo.to_be_bytes());
                        }
                    }
                    if traf.base_is_moof {
                        for trun_box in &mut traf.truns {
                            let flags = ((trun_box[9] as u32) << 16) |
                                ((trun_box[10] as u32) << 8) |
                                (trun_box[11] as u32);
                            if flags & 0x000001 != 0 && trun_box.len() >= 20 {
                                let old_off =
                                    i32::from_be_bytes(trun_box[16..20].try_into().unwrap());
                                let new_off = old_off + (total_senc_added as i32);
                                trun_box[16..20].copy_from_slice(&new_off.to_be_bytes());
                            }
                        }
                    }

                    let mut traf_p = Vec::new();
                    for child in traf.other_children {
                        traf_p.extend_from_slice(&child);
                    }
                    for trun_box in traf.truns {
                        traf_p.extend_from_slice(&trun_box);
                    }
                    if let Some(senc_box) = traf.senc {
                        traf_p.extend_from_slice(&senc_box);
                    }
                    built_trafs.push(wrap_box_32(b"traf", &traf_p)?);
                }

                let mut moof_p = Vec::new();
                for part in moof_parts {
                    match part {
                        Ok(raw) => moof_p.extend_from_slice(&raw),
                        Err(tidx) => moof_p.extend_from_slice(&built_trafs[tidx]),
                    }
                }
                let new_moof = wrap_box_32(b"moof", &moof_p)?;
                moof_offset_map.push((bx.start as u64, new_moof_start as u64));
                out.extend_from_slice(&new_moof);

                for inter in &top_boxes[idx + 1..idx + 1 + mdat_rel_idx] {
                    out.extend_from_slice(&clear_fmp4[inter.start..inter.end]);
                }
                out.extend_from_slice(&wrap_box_32(b"mdat", &mdat_payload)?);
                idx += 1 + mdat_rel_idx + 1;
            },
            b"mfra" => {
                let children = iter_boxes(clear_fmp4, bx.payload_start, bx.end)?;
                let mut mfra_p = Vec::new();
                for child in children {
                    if &child.box_type == b"tfra" {
                        let mut tp = clear_fmp4[child.payload_start..child.end].to_vec();
                        patch_tfra_offsets(&mut tp, &moof_offset_map)?;
                        mfra_p.extend_from_slice(&wrap_box_32(b"tfra", &tp)?);
                    } else {
                        mfra_p.extend_from_slice(&clear_fmp4[child.start..child.end]);
                    }
                }
                out.extend_from_slice(&wrap_box_32(b"mfra", &mfra_p)?);
                idx += 1;
            },
            _ => {
                out.extend_from_slice(&clear_fmp4[bx.start..bx.end]);
                idx += 1;
            },
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockCipherEncrypt;

    use super::*;

    fn box_wrap(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let sz = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(sz as usize);
        out.extend_from_slice(&sz.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(payload);
        out
    }

    fn build_test_init(scheme: [u8; 4], iv_size: u8, crypt: u8, skip: u8, const_iv: &[u8], kid: [u8; 16], include_pssh: bool) -> Vec<u8> {
        let ftyp = box_wrap(b"ftyp", b"isom\x00\x00\x02\x00isomiso6");

        // tkhd v0: version/flags(4) + ctime(4) + mtime(4) + track_id(4) + ...
        let mut tkhd_p = vec![0u8; 84];
        tkhd_p[12..16].copy_from_slice(&1u32.to_be_bytes());
        let tkhd = box_wrap(b"tkhd", &tkhd_p);

        // frma + schm + schi/tenc
        let frma = box_wrap(b"frma", b"avc1");
        let mut schm_p = vec![0u8; 12];
        schm_p[4..8].copy_from_slice(&scheme);
        schm_p[8..12].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        let schm = box_wrap(b"schm", &schm_p);

        let mut tenc_p = Vec::new();
        tenc_p.push(if crypt > 0 || skip > 0 { 1 } else { 0 }); // version
        tenc_p.extend_from_slice(&[0, 0, 0]); // flags
        tenc_p.push(0); // reserved
        tenc_p.push((crypt << 4) | (skip & 0x0f));
        tenc_p.push(1); // default_isProtected
        tenc_p.push(iv_size);
        tenc_p.extend_from_slice(&kid);
        if iv_size == 0 && !const_iv.is_empty() {
            tenc_p.push(const_iv.len() as u8);
            tenc_p.extend_from_slice(const_iv);
        }
        let tenc = box_wrap(b"tenc", &tenc_p);
        let schi = box_wrap(b"schi", &tenc);

        let mut sinf_p = Vec::new();
        sinf_p.extend_from_slice(&frma);
        sinf_p.extend_from_slice(&schm);
        sinf_p.extend_from_slice(&schi);
        let sinf = box_wrap(b"sinf", &sinf_p);

        // encv: 78 bytes header + sinf
        let mut encv_p = vec![0u8; 78];
        encv_p.extend_from_slice(&sinf);
        let encv = box_wrap(b"encv", &encv_p);

        let mut stsd_p = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd_p.extend_from_slice(&encv);
        let stsd = box_wrap(b"stsd", &stsd_p);
        let stbl = box_wrap(b"stbl", &stsd);
        let minf = box_wrap(b"minf", &stbl);
        let mdia = box_wrap(b"mdia", &minf);

        let mut trak_p = Vec::new();
        trak_p.extend_from_slice(&tkhd);
        trak_p.extend_from_slice(&mdia);
        let trak = box_wrap(b"trak", &trak_p);

        let mut moov_p = Vec::new();
        moov_p.extend_from_slice(&trak);
        if include_pssh {
            moov_p.extend_from_slice(&synthesize_clearkey_pssh_v1(&[kid]));
        }
        let moov = box_wrap(b"moov", &moov_p);

        let mut init = Vec::new();
        init.extend_from_slice(&ftyp);
        init.extend_from_slice(&moov);
        init
    }

    #[test]
    fn pssh_v1_and_v0_parsing_and_synthesis() {
        let kid1 = [0xaa; 16];
        let kid2 = [0xbb; 16];
        let pssh_v1 = synthesize_clearkey_pssh_v1(&[kid1, kid2]);
        let parsed = extract_kids_from_cenc_pssh(&pssh_v1).expect("parse v1 pssh");
        assert_eq!(parsed, vec![kid1, kid2]);

        // Build a v0 pssh with ClearKey KID in data, followed by v1 pssh.
        let mut v0_p = Vec::new();
        v0_p.extend_from_slice(&[0, 0, 0, 0]);
        v0_p.extend_from_slice(&COMMON_PSSH_SYSTEM_ID);
        v0_p.extend_from_slice(&16u32.to_be_bytes());
        v0_p.extend_from_slice(&kid1);
        let pssh_v0 = box_wrap(b"pssh", &v0_p);
        let parsed_v0 = extract_kids_from_cenc_pssh(&pssh_v0).expect("parse v0 pssh");
        assert_eq!(parsed_v0, vec![kid1]);

        let mut combined = pssh_v0;
        combined.extend_from_slice(&pssh_v1);
        let extracted = extract_cenc_pssh_boxes(&combined);
        assert_eq!(extracted.len(), combined.len());
    }

    #[test]
    fn rewrite_init_preserves_length_and_parses_tenc() {
        let kid = [0x11; 16];
        let init = build_test_init(*b"cenc", 8, 0, 0, &[], kid, true);
        let (clear, enc_map) = rewrite_init_segment_to_clear(&init).expect("expected rewrite");
        assert_eq!(clear.len(), init.len());
        assert!(clear.windows(4).any(|w| w == b"avc1"));
        assert!(!clear.windows(4).any(|w| w == b"encv"));
        assert!(!clear.windows(4).any(|w| w == b"sinf"));
        assert!(!clear.windows(4).any(|w| w == b"pssh"));
        let info = enc_map.get(&1).expect("track 1");
        assert_eq!(info.original_fourcc, *b"avc1");
        assert_eq!(info.scheme, CencScheme::Cenc);
        assert_eq!(info.default_kid, kid);
        assert_eq!(info.default_per_sample_iv_size, 8);
    }

    #[test]
    fn decrypt_cenc_ctr_subsamples_and_missing_key() {
        let kid = [0xaa; 16];
        let key = [0x22; 16];
        let iv = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let init = build_test_init(*b"cenc", 8, 0, 0, &[], kid, false);
        let (_, enc_map) = rewrite_init_segment_to_clear(&init).unwrap();

        // Plaintext sample: 5 clear + 19 protected + 4 clear + 12 protected = 40 bytes.
        let mut plain = vec![0xA5u8; 40];
        plain[0..5].copy_from_slice(b"NALU1");
        plain[24..28].copy_from_slice(b"NAL2");

        // Encrypt protected subsamples continuously using Aes128CtrStream.
        let mut encrypted_sample = plain.clone();
        {
            let mut ctr = Aes128CtrStream::new(&key, &iv).unwrap();
            ctr.apply_keystream(&mut encrypted_sample[5..24]);
            ctr.apply_keystream(&mut encrypted_sample[28..40]);
        }

        // Build styp + moof + mdat to also test non-zero moof_start offset.
        let styp = box_wrap(b"styp", b"msdh\x00\x00\x00\x00msdh");
        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        // tfhd: flags = 0x020000 (default-base-is-moof), track_id = 1
        let tfhd = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]);

        let mut senc_p = vec![0, 0, 0, 2]; // flags=2 (UseSubsampleEncryption)
        senc_p.extend_from_slice(&1u32.to_be_bytes());
        senc_p.extend_from_slice(&iv);
        senc_p.extend_from_slice(&2u16.to_be_bytes()); // 2 subsamples
        senc_p.extend_from_slice(&5u16.to_be_bytes());
        senc_p.extend_from_slice(&19u32.to_be_bytes());
        senc_p.extend_from_slice(&4u16.to_be_bytes());
        senc_p.extend_from_slice(&12u32.to_be_bytes());
        let senc = box_wrap(b"senc", &senc_p);

        // Compute trun data_offset: moof_size + 8 (mdat header)
        let mut trun_p = vec![0, 0, 0x02, 0x01]; // data_offset + sample_size
        trun_p.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
        let data_offset_pos_in_trun_p = trun_p.len();
        trun_p.extend_from_slice(&0i32.to_be_bytes()); // placeholder
        trun_p.extend_from_slice(&(encrypted_sample.len() as u32).to_be_bytes());
        let trun_dummy = box_wrap(b"trun", &trun_p);
        let traf_size = 8 + tfhd.len() + trun_dummy.len() + senc.len();
        let moof_size = 8 + mfhd.len() + traf_size;
        let data_offset = (moof_size + 8) as i32;
        trun_p[data_offset_pos_in_trun_p..data_offset_pos_in_trun_p + 4]
            .copy_from_slice(&data_offset.to_be_bytes());
        let trun = box_wrap(b"trun", &trun_p);

        let mut traf_p = Vec::new();
        traf_p.extend_from_slice(&tfhd);
        traf_p.extend_from_slice(&trun);
        traf_p.extend_from_slice(&senc);
        let traf = box_wrap(b"traf", &traf_p);

        let mut moof_p = Vec::new();
        moof_p.extend_from_slice(&mfhd);
        moof_p.extend_from_slice(&traf);
        let moof = box_wrap(b"moof", &moof_p);
        let mdat = box_wrap(b"mdat", &encrypted_sample);

        let mut seg = Vec::new();
        seg.extend_from_slice(&styp);
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);

        let empty_store = ClearKeyStore::new();
        assert_eq!(
            decrypt_media_segment(&seg, &enc_map, &empty_store),
            Err(CencDecryptError::MissingKey(kid))
        );

        let store = ClearKeyStore::new();
        store.set_key(kid, key);
        let clear_seg = decrypt_media_segment(&seg, &enc_map, &store).expect("decrypt cenc");
        assert_eq!(clear_seg.len(), seg.len());
        assert!(!clear_seg.windows(4).any(|w| w == b"senc"));
        let decrypted_sample = &clear_seg[clear_seg.len() - plain.len()..];
        assert_eq!(decrypted_sample, plain.as_slice());
    }

    #[test]
    fn decrypt_cbcs_pattern_constant_iv() {
        let kid = [0xcc; 16];
        let key = [0x33; 16];
        let iv = [0x44; 16];
        // 1:9 pattern with constant 16-byte IV
        let init = build_test_init(*b"cbcs", 0, 1, 9, &iv, kid, false);
        let (_, enc_map) = rewrite_init_segment_to_clear(&init).unwrap();
        assert_eq!(enc_map[&1].scheme, CencScheme::Cbcs);

        // 12 blocks (192 bytes) + 5 trailing bytes = 197 bytes.
        // Pattern 1:9 => blocks 0 and 10 are encrypted; blocks 1..9, 11, and trailing 5 bytes are clear!
        let plain: Vec<u8> = (0..197).map(|i| (i & 0xff) as u8).collect();
        let mut encrypted_sample = plain.clone();
        let cipher = Aes128::new((&key).into());
        let mut chain = iv;
        for blk_idx in [0usize, 10usize] {
            let start = blk_idx * 16;
            let mut blk = [0u8; 16];
            for i in 0..16 {
                blk[i] = encrypted_sample[start + i] ^ chain[i];
            }
            cipher.encrypt_block((&mut blk).into());
            encrypted_sample[start..start + 16].copy_from_slice(&blk);
            chain = blk;
        }

        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        let tfhd = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]);
        let mut trun_p = vec![0, 0, 0x02, 0x01];
        trun_p.extend_from_slice(&1u32.to_be_bytes());
        let off_pos = trun_p.len();
        trun_p.extend_from_slice(&0i32.to_be_bytes());
        trun_p.extend_from_slice(&(encrypted_sample.len() as u32).to_be_bytes());
        let trun_dummy = box_wrap(b"trun", &trun_p);
        let moof_size = 8 + mfhd.len() + 8 + tfhd.len() + trun_dummy.len();
        trun_p[off_pos..off_pos + 4].copy_from_slice(&((moof_size + 8) as i32).to_be_bytes());
        let trun = box_wrap(b"trun", &trun_p);
        let mut traf_p = Vec::new();
        traf_p.extend_from_slice(&tfhd);
        traf_p.extend_from_slice(&trun);
        let traf = box_wrap(b"traf", &traf_p);
        let mut moof_p = Vec::new();
        moof_p.extend_from_slice(&mfhd);
        moof_p.extend_from_slice(&traf);
        let moof = box_wrap(b"moof", &moof_p);
        let mdat = box_wrap(b"mdat", &encrypted_sample);

        let mut seg = Vec::new();
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);

        let store = ClearKeyStore::new();
        store.set_key(kid, key);
        let clear_seg = decrypt_media_segment(&seg, &enc_map, &store).expect("decrypt cbcs");
        let decrypted_sample = &clear_seg[clear_seg.len() - plain.len()..];
        assert_eq!(decrypted_sample, plain.as_slice());
    }

    #[test]
    fn non_common_v0_pssh_does_not_leak_bogus_kids() {
        let widevine_system_id: [u8; 16] = [
            0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d,
            0x21, 0xed,
        ];
        let mut v0_p = Vec::new();
        v0_p.extend_from_slice(&[0, 0, 0, 0]); // version=0, flags=0
        v0_p.extend_from_slice(&widevine_system_id);
        v0_p.extend_from_slice(&32u32.to_be_bytes());
        v0_p.extend_from_slice(&[0x55; 32]); // 32 bytes (divisible by 16!) of opaque protobuf
        let pssh = box_wrap(b"pssh", &v0_p);

        let kids = extract_kids_from_cenc_pssh(&pssh).expect("valid v0 pssh box");
        assert!(
            kids.is_empty(),
            "non-Common v0 pssh opaque payload must not be treated as ClearKey KIDs"
        );
    }

    #[test]
    fn decrypt_saiz_saio_without_senc_and_multi_traf_dual_track() {
        let kid_v = [0x11; 16];
        let key_v = [0x21; 16];
        let kid_a = [0x12; 16];
        let key_a = [0x22; 16];

        // Build dual-track init: track 1 encv(avc1), track 2 enca(mp4a)
        let ftyp = box_wrap(b"ftyp", b"isom\x00\x00\x02\x00isomiso6");
        let make_trak = |track_id: u32, enc_fourcc: &[u8; 4], orig_fourcc: &[u8; 4], hdr_len: usize, kid: [u8; 16]| {
            let mut tkhd_p = vec![0u8; 84];
            tkhd_p[12..16].copy_from_slice(&track_id.to_be_bytes());
            let tkhd = box_wrap(b"tkhd", &tkhd_p);
            let frma = box_wrap(b"frma", orig_fourcc);
            let mut schm_p = vec![0u8; 12];
            schm_p[4..8].copy_from_slice(b"cenc");
            let schm = box_wrap(b"schm", &schm_p);
            let mut tenc_p = vec![0, 0, 0, 0, 0, 0, 1, 8];
            tenc_p.extend_from_slice(&kid);
            let tenc = box_wrap(b"tenc", &tenc_p);
            let schi = box_wrap(b"schi", &tenc);
            let mut sinf_p = Vec::new();
            sinf_p.extend_from_slice(&frma);
            sinf_p.extend_from_slice(&schm);
            sinf_p.extend_from_slice(&schi);
            let sinf = box_wrap(b"sinf", &sinf_p);
            let mut entry_p = vec![0u8; hdr_len];
            entry_p.extend_from_slice(&sinf);
            let entry = box_wrap(enc_fourcc, &entry_p);
            let mut stsd_p = vec![0, 0, 0, 0, 0, 0, 0, 1];
            stsd_p.extend_from_slice(&entry);
            let stsd = box_wrap(b"stsd", &stsd_p);
            let stbl = box_wrap(b"stbl", &stsd);
            let minf = box_wrap(b"minf", &stbl);
            let mdia = box_wrap(b"mdia", &minf);
            let mut trak_p = Vec::new();
            trak_p.extend_from_slice(&tkhd);
            trak_p.extend_from_slice(&mdia);
            box_wrap(b"trak", &trak_p)
        };
        let mut moov_p = Vec::new();
        moov_p.extend_from_slice(&make_trak(1, b"encv", b"avc1", 78, kid_v));
        moov_p.extend_from_slice(&make_trak(2, b"enca", b"mp4a", 28, kid_a));
        let moov = box_wrap(b"moov", &moov_p);
        let mut init = Vec::new();
        init.extend_from_slice(&ftyp);
        init.extend_from_slice(&moov);

        let (clear_init, enc_map) = rewrite_init_segment_to_clear(&init).expect("dual rewrite");
        assert_eq!(clear_init.len(), init.len());
        assert_eq!(enc_map[&1].original_fourcc, *b"avc1");
        assert_eq!(enc_map[&2].original_fourcc, *b"mp4a");

        let iv_v = [1, 2, 3, 4, 5, 6, 7, 8];
        let iv_a = [8, 7, 6, 5, 4, 3, 2, 1];
        let plain_v = b"VIDEO_CLEAR_AND_PROTECTED_BYTES!".to_vec();
        let plain_a = b"AUDIO_FULL_ENCRYPTED_FRAME!".to_vec();

        let mut enc_v = plain_v.clone();
        {
            let mut ctr = Aes128CtrStream::new(&key_v, &iv_v).unwrap();
            ctr.apply_keystream(&mut enc_v[6..]); // 6 clear + rest protected
        }
        let mut enc_a = plain_a.clone();
        {
            let mut ctr = Aes128CtrStream::new(&key_a, &iv_a).unwrap();
            ctr.apply_keystream(&mut enc_a);
        }

        // Auxiliary info stored in an auxiliary block inside mdat, referenced ONLY by saiz+saio (no senc).
        let mut aux_blob_v = Vec::new();
        aux_blob_v.extend_from_slice(&iv_v);
        aux_blob_v.extend_from_slice(&1u16.to_be_bytes());
        aux_blob_v.extend_from_slice(&6u16.to_be_bytes());
        aux_blob_v.extend_from_slice(&((plain_v.len() - 6) as u32).to_be_bytes());
        let aux_blob_a = iv_a.to_vec();

        // mdat layout: [aux_blob_v][aux_blob_a][enc_v][enc_a]
        let mut mdat_p = Vec::new();
        mdat_p.extend_from_slice(&aux_blob_v);
        mdat_p.extend_from_slice(&aux_blob_a);
        let enc_v_rel_mdat = mdat_p.len();
        mdat_p.extend_from_slice(&enc_v);
        mdat_p.extend_from_slice(&enc_a);

        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        // traf 1 (default-base-is-moof)
        let tfhd_v = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]);
        let mut saiz_v_p = vec![0, 0, 0, 0, aux_blob_v.len() as u8];
        saiz_v_p.extend_from_slice(&1u32.to_be_bytes());
        let saiz_v = box_wrap(b"saiz", &saiz_v_p);

        // traf 2 (no default-base-is-moof, omits trun data_offset so it follows traf 1 in mdat!)
        let tfhd_a = box_wrap(b"tfhd", &[0, 0, 0, 0, 0, 0, 0, 2]);
        let mut saiz_a_p = vec![0, 0, 0, 0, aux_blob_a.len() as u8];
        saiz_a_p.extend_from_slice(&1u32.to_be_bytes());
        let saiz_a = box_wrap(b"saiz", &saiz_a_p);

        let mut trun_v_p = vec![0, 0, 0x02, 0x01];
        trun_v_p.extend_from_slice(&1u32.to_be_bytes());
        let trun_v_off_pos = trun_v_p.len();
        trun_v_p.extend_from_slice(&0i32.to_be_bytes());
        trun_v_p.extend_from_slice(&(enc_v.len() as u32).to_be_bytes());

        let mut trun_a_p = vec![0, 0, 0x02, 0x00]; // NO data_offset!
        trun_a_p.extend_from_slice(&1u32.to_be_bytes());
        trun_a_p.extend_from_slice(&(enc_a.len() as u32).to_be_bytes());
        let trun_a = box_wrap(b"trun", &trun_a_p);

        let mut saio_v_p = vec![0, 0, 0, 0];
        saio_v_p.extend_from_slice(&1u32.to_be_bytes());
        let saio_v_off_pos = saio_v_p.len();
        saio_v_p.extend_from_slice(&0u32.to_be_bytes());

        let mut saio_a_p = vec![0, 0, 0, 0];
        saio_a_p.extend_from_slice(&1u32.to_be_bytes());
        let saio_a_off_pos = saio_a_p.len();
        saio_a_p.extend_from_slice(&0u32.to_be_bytes());

        // Compute moof size with placeholder offsets
        let trun_v_dummy = box_wrap(b"trun", &trun_v_p);
        let saio_v_dummy = box_wrap(b"saio", &saio_v_p);
        let saio_a_dummy = box_wrap(b"saio", &saio_a_p);
        let traf_v_size = 8 + tfhd_v.len() + trun_v_dummy.len() + saiz_v.len() + saio_v_dummy.len();
        let traf_a_size = 8 + tfhd_a.len() + trun_a.len() + saiz_a.len() + saio_a_dummy.len();
        let moof_size = 8 + mfhd.len() + traf_v_size + traf_a_size;
        let mdat_payload_off = moof_size + 8;

        // Patch trun_v data_offset (relative to moof_start=0)
        trun_v_p[trun_v_off_pos..trun_v_off_pos + 4]
            .copy_from_slice(&((mdat_payload_off + enc_v_rel_mdat) as i32).to_be_bytes());
        let trun_v = box_wrap(b"trun", &trun_v_p);

        // Patch saio_v (relative to traf_v base = moof_start = 0)
        saio_v_p[saio_v_off_pos..saio_v_off_pos + 4]
            .copy_from_slice(&(mdat_payload_off as u32).to_be_bytes());
        let saio_v = box_wrap(b"saio", &saio_v_p);

        // Patch saio_a (relative to traf_a base = prev_traf_data_end = mdat_payload_off + enc_v_rel_mdat + enc_v.len())
        // Use a small negative? Wait: saio_a in traf_a is relative to traf_a_base; put aux_blob_a relative to traf_a_base?
        // Actually in real files saio_a uses default_base_is_moof, or if traf_a_base is prev_traf_data_end, let's test both:
        // Put aux_blob_a right after enc_v? Even cleaner: set tfhd_a default_base_is_moof=false and test trun_a without data_offset!
        // Wait: if saio_a is relative to traf_a_base (which is enc_a's start), we can't point backwards with unsigned saio;
        // so let's test saio_a with `default_base_is_moof` = false by placing `senc` on traf_a OR placing aux_blob_a after enc_v!
        // Let's rebuild mdat_p with aux_blob_a right after enc_v so traf_a_base == aux_blob_a start, and trun_a omits data_offset? Wait: if trun_a omits data_offset, enc_a starts right at traf_a_base.
        // Instead, give traf_a a `senc` box (mixing saiz/saio on video and senc on audio + omitted trun_a data_offset)!
        let _ = saio_a_off_pos;
        let _ = aux_blob_a;
        let mut senc_a_p = vec![0, 0, 0, 0];
        senc_a_p.extend_from_slice(&1u32.to_be_bytes());
        senc_a_p.extend_from_slice(&iv_a);
        let senc_a = box_wrap(b"senc", &senc_a_p);

        let mut mdat_p2 = Vec::new();
        mdat_p2.extend_from_slice(&aux_blob_v);
        let enc_v_rel = mdat_p2.len();
        mdat_p2.extend_from_slice(&enc_v);
        let enc_a_rel = mdat_p2.len();
        mdat_p2.extend_from_slice(&enc_a);

        // Recompute sizes cleanly
        let traf_v_size2 = 8 + tfhd_v.len() + trun_v.len() + saiz_v.len() + saio_v.len();
        let traf_a_size2 = 8 + tfhd_a.len() + trun_a.len() + senc_a.len();
        let moof_size2 = 8 + mfhd.len() + traf_v_size2 + traf_a_size2;
        let mdat_payload_off2 = moof_size2 + 8;

        trun_v_p[trun_v_off_pos..trun_v_off_pos + 4]
            .copy_from_slice(&((mdat_payload_off2 + enc_v_rel) as i32).to_be_bytes());
        let trun_v = box_wrap(b"trun", &trun_v_p);
        saio_v_p[saio_v_off_pos..saio_v_off_pos + 4]
            .copy_from_slice(&(mdat_payload_off2 as u32).to_be_bytes());
        let saio_v = box_wrap(b"saio", &saio_v_p);

        let mut traf_v_p = Vec::new();
        traf_v_p.extend_from_slice(&tfhd_v);
        traf_v_p.extend_from_slice(&trun_v);
        traf_v_p.extend_from_slice(&saiz_v);
        traf_v_p.extend_from_slice(&saio_v);
        let traf_v = box_wrap(b"traf", &traf_v_p);

        let mut traf_a_p = Vec::new();
        traf_a_p.extend_from_slice(&tfhd_a);
        traf_a_p.extend_from_slice(&trun_a);
        traf_a_p.extend_from_slice(&senc_a);
        let traf_a = box_wrap(b"traf", &traf_a_p);

        let mut moof_p = Vec::new();
        moof_p.extend_from_slice(&mfhd);
        moof_p.extend_from_slice(&traf_v);
        moof_p.extend_from_slice(&traf_a);
        let moof = box_wrap(b"moof", &moof_p);
        let mdat = box_wrap(b"mdat", &mdat_p2);

        let mut seg = Vec::new();
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);

        let store = ClearKeyStore::new();
        store.set_key(kid_v, key_v);
        // Only video key set -> must fail with MissingKey(kid_a)
        assert_eq!(
            decrypt_media_segment(&seg, &enc_map, &store),
            Err(CencDecryptError::MissingKey(kid_a))
        );

        store.set_key(kid_a, key_a);
        let clear_seg = decrypt_media_segment(&seg, &enc_map, &store).expect("dual decrypt");
        assert!(!clear_seg.windows(4).any(|w| w == b"saiz" || w == b"saio" || w == b"senc"));
        assert_eq!(
            &clear_seg[mdat_payload_off2 + enc_v_rel..mdat_payload_off2 + enc_v_rel + plain_v.len()],
            plain_v.as_slice()
        );
        assert_eq!(
            &clear_seg[mdat_payload_off2 + enc_a_rel..mdat_payload_off2 + enc_a_rel + plain_a.len()],
            plain_a.as_slice()
        );
    }

    #[test]
    fn decrypt_cbcs_8_byte_iv_with_subsamples() {
        let kid = [0xdd; 16];
        let key = [0x44; 16];
        let iv8 = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];
        let init = build_test_init(*b"cbcs", 8, 1, 9, &[], kid, false);
        let (_, enc_map) = rewrite_init_segment_to_clear(&init).unwrap();

        // 4 bytes clear + 35 bytes protected (2 blocks + 3 trailing clear bytes).
        let mut plain = vec![0x7Au8; 39];
        plain[..4].copy_from_slice(b"HDR!");
        let mut enc_sample = plain.clone();
        // Pattern 1:9 -> only block 0 of protected range [4..39] is encrypted using iv8 zero-padded to 16.
        let cipher = Aes128::new((&key).into());
        let mut iv16 = [0u8; 16];
        iv16[..8].copy_from_slice(&iv8);
        let mut blk = [0u8; 16];
        for i in 0..16 {
            blk[i] = enc_sample[4 + i] ^ iv16[i];
        }
        cipher.encrypt_block((&mut blk).into());
        enc_sample[4..20].copy_from_slice(&blk);

        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        let tfhd = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]);
        let mut senc_p = vec![0, 0, 0, 2];
        senc_p.extend_from_slice(&1u32.to_be_bytes());
        senc_p.extend_from_slice(&iv8);
        senc_p.extend_from_slice(&1u16.to_be_bytes());
        senc_p.extend_from_slice(&4u16.to_be_bytes());
        senc_p.extend_from_slice(&35u32.to_be_bytes());
        let senc = box_wrap(b"senc", &senc_p);

        let mut trun_p = vec![0, 0, 0x02, 0x01];
        trun_p.extend_from_slice(&1u32.to_be_bytes());
        let off_pos = trun_p.len();
        trun_p.extend_from_slice(&0i32.to_be_bytes());
        trun_p.extend_from_slice(&(enc_sample.len() as u32).to_be_bytes());
        let trun_dummy = box_wrap(b"trun", &trun_p);
        let moof_size = 8 + mfhd.len() + 8 + tfhd.len() + trun_dummy.len() + senc.len();
        trun_p[off_pos..off_pos + 4].copy_from_slice(&((moof_size + 8) as i32).to_be_bytes());
        let trun = box_wrap(b"trun", &trun_p);
        let mut traf_p = Vec::new();
        traf_p.extend_from_slice(&tfhd);
        traf_p.extend_from_slice(&trun);
        traf_p.extend_from_slice(&senc);
        let traf = box_wrap(b"traf", &traf_p);
        let mut moof_p = Vec::new();
        moof_p.extend_from_slice(&mfhd);
        moof_p.extend_from_slice(&traf);
        let moof = box_wrap(b"moof", &moof_p);
        let mdat = box_wrap(b"mdat", &enc_sample);

        let mut seg = Vec::new();
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);

        let store = ClearKeyStore::new();
        store.set_key(kid, key);
        let clear_seg = decrypt_media_segment(&seg, &enc_map, &store).expect("decrypt cbcs 8B IV");
        assert_eq!(&clear_seg[clear_seg.len() - plain.len()..], plain.as_slice());
    }

    #[test]
    fn multi_session_shared_kid_lifecycle() {
        let store = ClearKeyStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        let kid_shared = [0x11; 16];
        let kid_b_only = [0x22; 16];
        let key_shared = [0xaa; 16];
        let key_b_only = [0xbb; 16];

        store.set_session_key("session-a", kid_shared, key_shared);
        store.set_session_key("session-b", kid_shared, key_shared);
        store.set_session_key("session-b", kid_b_only, key_b_only);
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
        assert_eq!(store.get_key(&kid_shared), Some(key_shared));
        assert_eq!(store.get_key(&kid_b_only), Some(key_b_only));

        // Closing Session A must NOT evict kid_shared while Session B still owns it.
        store.remove_session("session-a");
        assert_eq!(store.len(), 2);
        assert_eq!(store.get_key(&kid_shared), Some(key_shared));
        assert_eq!(store.get_key(&kid_b_only), Some(key_b_only));

        // Closing Session B evicts both kid_shared and kid_b_only.
        store.remove_session("session-b");
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
        assert_eq!(store.get_key(&kid_shared), None);
        assert_eq!(store.get_key(&kid_b_only), None);

        // Backwards-compatible default "" session methods coexist with named sessions.
        store.set_key(kid_shared, key_shared);
        store.set_session_key("session-c", kid_shared, key_shared);
        assert_eq!(store.len(), 1);
        assert_eq!(store.remove_key(&kid_shared), Some(key_shared));
        assert_eq!(store.get_key(&kid_shared), Some(key_shared));
        store.remove_session("session-c");
        assert!(store.is_empty());
    }

    #[test]
    fn encrypt_clear_fmp4_to_cbcs_roundtrip_with_nalus_and_mfra() {
        let kid = [0x51; 16];
        let key = [0x62; 16];
        let constant_iv = [0x73; 16];

        // Build clear ftyp + moov (avc1 + avcC).
        let ftyp = box_wrap(b"ftyp", b"isom\x00\x00\x02\x00isomiso6");
        let mut tkhd_p = vec![0u8; 84];
        tkhd_p[12..16].copy_from_slice(&1u32.to_be_bytes());
        let tkhd = box_wrap(b"tkhd", &tkhd_p);
        let avcc = box_wrap(b"avcC", &[1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, 0, 0]);
        let mut avc1_p = vec![0u8; 78];
        avc1_p.extend_from_slice(&avcc);
        let avc1 = box_wrap(b"avc1", &avc1_p);
        let mut stsd_p = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd_p.extend_from_slice(&avc1);
        let stsd = box_wrap(b"stsd", &stsd_p);
        let stbl = box_wrap(b"stbl", &stsd);
        let minf = box_wrap(b"minf", &stbl);
        let mdia = box_wrap(b"mdia", &minf);
        let mut trak_p = Vec::new();
        trak_p.extend_from_slice(&tkhd);
        trak_p.extend_from_slice(&mdia);
        let trak = box_wrap(b"trak", &trak_p);
        let moov = box_wrap(b"moov", &trak);

        // Build two H.264 samples with 4-byte NALU lengths (multi-NALU sample 0 + single-NALU sample 1).
        let mut s0_nal1 = vec![0x65u8]; // IDR slice header
        s0_nal1.extend((0u8..199).collect::<Vec<_>>()); // total NALU = 200 bytes
        let mut s0_nal2 = vec![0x06u8]; // SEI
        s0_nal2.extend_from_slice(&[0xaa; 10]); // total NALU = 11 bytes
        let mut sample0 = Vec::new();
        sample0.extend_from_slice(&(s0_nal1.len() as u32).to_be_bytes());
        sample0.extend_from_slice(&s0_nal1);
        sample0.extend_from_slice(&(s0_nal2.len() as u32).to_be_bytes());
        sample0.extend_from_slice(&s0_nal2);

        let mut s1_nal = vec![0x41u8]; // non-IDR slice
        s1_nal.extend((0u8..64).map(|v| v ^ 0x5a).collect::<Vec<_>>());
        let mut sample1 = Vec::new();
        sample1.extend_from_slice(&(s1_nal.len() as u32).to_be_bytes());
        sample1.extend_from_slice(&s1_nal);

        let mut mdat_payload = Vec::new();
        mdat_payload.extend_from_slice(&sample0);
        mdat_payload.extend_from_slice(&sample1);

        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        let tfhd = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]); // default-base-is-moof
        let mut trun_p = vec![0, 0, 0x02, 0x01]; // data_offset + sample_size
        trun_p.extend_from_slice(&2u32.to_be_bytes());
        let off_pos = trun_p.len();
        trun_p.extend_from_slice(&0i32.to_be_bytes());
        trun_p.extend_from_slice(&(sample0.len() as u32).to_be_bytes());
        trun_p.extend_from_slice(&(sample1.len() as u32).to_be_bytes());
        let trun_dummy = box_wrap(b"trun", &trun_p);
        let moof_size = 8 + mfhd.len() + 8 + tfhd.len() + trun_dummy.len();
        trun_p[off_pos..off_pos + 4].copy_from_slice(&((moof_size + 8) as i32).to_be_bytes());
        let trun = box_wrap(b"trun", &trun_p);
        let mut traf_p = Vec::new();
        traf_p.extend_from_slice(&tfhd);
        traf_p.extend_from_slice(&trun);
        let traf = box_wrap(b"traf", &traf_p);
        let mut moof_p = Vec::new();
        moof_p.extend_from_slice(&mfhd);
        moof_p.extend_from_slice(&traf);
        let moof = box_wrap(b"moof", &moof_p);
        let mdat = box_wrap(b"mdat", &mdat_payload);

        let orig_moof_offset = (ftyp.len() + moov.len()) as u64;
        let mut tfra_p = vec![1, 0, 0, 0]; // version=1
        tfra_p.extend_from_slice(&1u32.to_be_bytes()); // track_id=1
        tfra_p.extend_from_slice(&0u32.to_be_bytes()); // lengths = 1 byte each
        tfra_p.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        tfra_p.extend_from_slice(&0u64.to_be_bytes()); // time
        tfra_p.extend_from_slice(&orig_moof_offset.to_be_bytes()); // moof_offset
        tfra_p.extend_from_slice(&[1, 1, 1]); // traf, trun, sample
        let tfra = box_wrap(b"tfra", &tfra_p);
        let mfra = box_wrap(b"mfra", &tfra);

        let mut clear_fmp4 = Vec::new();
        clear_fmp4.extend_from_slice(&ftyp);
        clear_fmp4.extend_from_slice(&moov);
        clear_fmp4.extend_from_slice(&moof);
        clear_fmp4.extend_from_slice(&mdat);
        clear_fmp4.extend_from_slice(&mfra);

        let cbcs_fmp4 = encrypt_clear_fmp4_to_cbcs(&clear_fmp4, kid, key, constant_iv)
            .expect("encrypt clear fmp4 to cbcs");
        let top = iter_boxes(&cbcs_fmp4, 0, cbcs_fmp4.len()).expect("valid top-level boxes");
        assert_eq!(top.len(), 5);
        assert_eq!(&top[1].box_type, b"moov");
        assert_eq!(&top[2].box_type, b"moof");
        assert_eq!(&top[3].box_type, b"mdat");
        assert_eq!(&top[4].box_type, b"mfra");

        // Verify Common SystemID pssh v1 is present and parseable.
        let pssh = extract_cenc_pssh_boxes(&cbcs_fmp4[top[0].start..top[1].end]);
        assert_eq!(extract_kids_from_cenc_pssh(&pssh).unwrap(), vec![kid]);

        // Verify NALU headers remain in clear and encrypted bytes differ from plaintext.
        let enc_mdat = &cbcs_fmp4[top[3].payload_start..top[3].end];
        assert_ne!(enc_mdat, mdat_payload.as_slice());
        assert_eq!(&enc_mdat[..5], &sample0[..5]);

        // Verify tfra moof_offset was updated to match new moof offset.
        let mfra_children = iter_boxes(&cbcs_fmp4, top[4].payload_start, top[4].end).unwrap();
        let tfra_payload = &cbcs_fmp4[mfra_children[0].payload_start..mfra_children[0].end];
        let patched_moof_off = u64::from_be_bytes(tfra_payload[24..32].try_into().unwrap());
        assert_eq!(patched_moof_off, top[2].start as u64);

        // Roundtrip decrypt via rewrite_init_segment_to_clear + decrypt_media_segment.
        let (clear_init, enc_map) =
            rewrite_init_segment_to_clear(&cbcs_fmp4[top[0].start..top[1].end])
                .expect("rewrite cbcs init to clear");
        assert_eq!(enc_map[&1].scheme, CencScheme::Cbcs);
        assert_eq!(enc_map[&1].default_crypt_byte_block, 1);
        assert_eq!(enc_map[&1].default_skip_byte_block, 9);
        assert_eq!(enc_map[&1].default_constant_iv, constant_iv.to_vec());
        assert!(clear_init.windows(4).any(|w| w == b"avc1"));

        let store = ClearKeyStore::new();
        store.set_key(kid, key);
        let clear_seg =
            decrypt_media_segment(&cbcs_fmp4[top[2].start..top[3].end], &enc_map, &store)
                .expect("decrypt cbcs segment");
        let clear_top = iter_boxes(&clear_seg, 0, clear_seg.len()).unwrap();
        let dec_mdat = &clear_seg[clear_top[1].payload_start..clear_top[1].end];
        assert_eq!(dec_mdat, mdat_payload.as_slice());
    }

    #[test]
    fn encrypt_clear_fmp4_to_cbcs_multi_traf_avc_and_mp4a() {
        let kid = [0x91; 16];
        let key = [0xa2; 16];
        let constant_iv = [0xb3; 16];

        let ftyp = box_wrap(b"ftyp", b"isom\x00\x00\x02\x00isomiso6");
        let make_trak = |track_id: u32, fourcc: &[u8; 4], hdr_len: usize, child: &[u8]| {
            let mut tkhd_p = vec![0u8; 84];
            tkhd_p[12..16].copy_from_slice(&track_id.to_be_bytes());
            let tkhd = box_wrap(b"tkhd", &tkhd_p);
            let mut entry_p = vec![0u8; hdr_len];
            entry_p.extend_from_slice(child);
            let entry = box_wrap(fourcc, &entry_p);
            let mut stsd_p = vec![0, 0, 0, 0, 0, 0, 0, 1];
            stsd_p.extend_from_slice(&entry);
            let stsd = box_wrap(b"stsd", &stsd_p);
            let stbl = box_wrap(b"stbl", &stsd);
            let minf = box_wrap(b"minf", &stbl);
            let mdia = box_wrap(b"mdia", &minf);
            let mut trak_p = Vec::new();
            trak_p.extend_from_slice(&tkhd);
            trak_p.extend_from_slice(&mdia);
            box_wrap(b"trak", &trak_p)
        };
        let avcc = box_wrap(b"avcC", &[1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, 0, 0]);
        let esds = box_wrap(b"esds", &[0, 0, 0, 0, 0x03, 0x01, 0x00]);
        let mut moov_p = Vec::new();
        moov_p.extend_from_slice(&make_trak(1, b"avc1", 78, &avcc));
        moov_p.extend_from_slice(&make_trak(2, b"mp4a", 28, &esds));
        let moov = box_wrap(b"moov", &moov_p);

        let mut v_nal = vec![0x65u8];
        v_nal.extend((0u8..48).collect::<Vec<_>>());
        let mut v_sample = Vec::new();
        v_sample.extend_from_slice(&(v_nal.len() as u32).to_be_bytes());
        v_sample.extend_from_slice(&v_nal);

        let a_sample: Vec<u8> = (0u8..40).map(|b| b.wrapping_mul(3)).collect();

        let mut mdat_payload = Vec::new();
        mdat_payload.extend_from_slice(&v_sample);
        mdat_payload.extend_from_slice(&a_sample);

        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        // traf 1: default-base-is-moof with trun data_offset relative to moof
        let tfhd_v = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]);
        let mut trun_v_p = vec![0, 0, 0x02, 0x01];
        trun_v_p.extend_from_slice(&1u32.to_be_bytes());
        let off_v_pos = trun_v_p.len();
        trun_v_p.extend_from_slice(&0i32.to_be_bytes());
        trun_v_p.extend_from_slice(&(v_sample.len() as u32).to_be_bytes());

        // traf 2: default_base_is_moof = false, explicit trun data_offset = 0 relative to prev_traf_data_end!
        let tfhd_a = box_wrap(b"tfhd", &[0, 0, 0, 0, 0, 0, 0, 2]);
        let mut trun_a_p = vec![0, 0, 0x02, 0x01];
        trun_a_p.extend_from_slice(&1u32.to_be_bytes());
        trun_a_p.extend_from_slice(&0i32.to_be_bytes()); // data_offset = 0 relative to prev_traf_data_end
        trun_a_p.extend_from_slice(&(a_sample.len() as u32).to_be_bytes());
        let trun_a = box_wrap(b"trun", &trun_a_p);

        let trun_v_dummy = box_wrap(b"trun", &trun_v_p);
        let traf_v_size = 8 + tfhd_v.len() + trun_v_dummy.len();
        let traf_a_size = 8 + tfhd_a.len() + trun_a.len();
        let moof_size = 8 + mfhd.len() + traf_v_size + traf_a_size;
        trun_v_p[off_v_pos..off_v_pos + 4].copy_from_slice(&((moof_size + 8) as i32).to_be_bytes());
        let trun_v = box_wrap(b"trun", &trun_v_p);

        let mut traf_v_p = Vec::new();
        traf_v_p.extend_from_slice(&tfhd_v);
        traf_v_p.extend_from_slice(&trun_v);
        let traf_v = box_wrap(b"traf", &traf_v_p);

        let mut traf_a_p = Vec::new();
        traf_a_p.extend_from_slice(&tfhd_a);
        traf_a_p.extend_from_slice(&trun_a);
        let traf_a = box_wrap(b"traf", &traf_a_p);

        let mut moof_p = Vec::new();
        moof_p.extend_from_slice(&mfhd);
        moof_p.extend_from_slice(&traf_v);
        moof_p.extend_from_slice(&traf_a);
        let moof = box_wrap(b"moof", &moof_p);
        let mdat = box_wrap(b"mdat", &mdat_payload);

        let mut clear_fmp4 = Vec::new();
        clear_fmp4.extend_from_slice(&ftyp);
        clear_fmp4.extend_from_slice(&moov);
        clear_fmp4.extend_from_slice(&moof);
        clear_fmp4.extend_from_slice(&mdat);

        let cbcs_fmp4 = encrypt_clear_fmp4_to_cbcs(&clear_fmp4, kid, key, constant_iv)
            .expect("encrypt multi-traf clear fmp4 to cbcs");
        let top = iter_boxes(&cbcs_fmp4, 0, cbcs_fmp4.len()).unwrap();
        let (clear_init, enc_map) =
            rewrite_init_segment_to_clear(&cbcs_fmp4[top[0].start..top[1].end]).unwrap();
        assert!(clear_init.windows(4).any(|w| w == b"avc1"));
        assert!(clear_init.windows(4).any(|w| w == b"mp4a"));
        assert_eq!(enc_map[&1].original_fourcc, *b"avc1");
        assert_eq!(enc_map[&2].original_fourcc, *b"mp4a");

        let store = ClearKeyStore::new();
        store.set_key(kid, key);
        let clear_seg =
            decrypt_media_segment(&cbcs_fmp4[top[2].start..top[3].end], &enc_map, &store)
                .expect("decrypt multi-traf cbcs segment");
        let clear_top = iter_boxes(&clear_seg, 0, clear_seg.len()).unwrap();
        let dec_mdat = &clear_seg[clear_top[1].payload_start..clear_top[1].end];
        assert_eq!(dec_mdat, mdat_payload.as_slice());
    }
}
