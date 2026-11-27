/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use servo_base::generic_channel::GenericCallback;
use servo_media_player::cenc::{
    self, CencDecryptError, ClearKeyStore, TrackEncryptionInfo,
};
use servo_media_player::{
    MseEndOfStreamError, MseTrackDescriptor, MseTrackKind, PlayerError, PlayerEvent,
};
use url::Url;

const MSE_APPSRC_MAX_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB per SourceBuffer track.

static REGISTER_MSE_SRC: Once = Once::new();

fn secs_to_clocktime(secs: f64) -> Option<gstreamer::ClockTime> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    gstreamer::ClockTime::try_from_seconds_f64(secs).ok()
}

fn infer_kind_from_mime(mime_type: &str) -> Option<MseTrackKind> {
    let lower = mime_type.trim().to_ascii_lowercase();
    if lower.starts_with("audio/") {
        Some(MseTrackKind::Audio)
    } else if lower.starts_with("video/") {
        Some(MseTrackKind::Video)
    } else if lower.starts_with("text/") {
        Some(MseTrackKind::Text)
    } else {
        None
    }
}

fn send_player_event(
    observer: &Mutex<Option<Arc<Mutex<GenericCallback<PlayerEvent>>>>>,
    event: PlayerEvent,
) {
    let cb = observer.lock().unwrap().clone();
    if let Some(cb) = cb {
        let _ = cb.lock().unwrap().send(event);
    }
}

fn contains_top_level_box(bytes: &[u8], target: [u8; 4]) -> bool {
    let mut offset = 0usize;
    while offset + 8 <= bytes.len() {
        let size32 = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as u64;
        let fourcc = [
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ];
        let box_size = if size32 == 1 {
            if offset + 16 > bytes.len() {
                break;
            }
            u64::from_be_bytes([
                bytes[offset + 8],
                bytes[offset + 9],
                bytes[offset + 10],
                bytes[offset + 11],
                bytes[offset + 12],
                bytes[offset + 13],
                bytes[offset + 14],
                bytes[offset + 15],
            ]) as usize
        } else if size32 == 0 {
            bytes.len() - offset
        } else {
            size32 as usize
        };
        if box_size < 8 || offset + box_size > bytes.len() {
            break;
        }
        if fourcc == target {
            return true;
        }
        offset += box_size;
    }
    false
}

struct TrackEntry {
    id: u32,
    kind: Option<MseTrackKind>,
    appsrc: gstreamer_app::AppSrc,
    ghost_pad: Option<gstreamer::GhostPad>,
    pending_buffers: VecDeque<Vec<u8>>,
    has_data: bool,
    flushed_epoch: u64,
    dirty_since_flush: bool,
    ended: bool,
    encryption_info: HashMap<u32, TrackEncryptionInfo>,
    pending_encrypted: VecDeque<Vec<u8>>,
    waiting_for_key: bool,
    pending_eos: bool,
}

#[derive(Default)]
struct SourceState {
    tracks: BTreeMap<u32, TrackEntry>,
    committed_tracks: BTreeMap<u32, Option<MseTrackKind>>,
    tracks_committed: bool,
    pads_finalized: bool,
    duration_set: bool,
    duration: Option<f64>,
    live_seekable_range: Option<(f64, f64)>,
    seek_epoch: u64,
    last_seek_seqnum: Option<gstreamer::Seqnum>,
    pending_seek: Option<f64>,
    clearkey_store: Option<ClearKeyStore>,
}

struct ExposedPadSpec {
    id: u32,
    kind: Option<MseTrackKind>,
    appsrc: gstreamer_app::AppSrc,
}

mod imp {
    use super::*;

    static AUDIO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "audio_src_%u",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &gstreamer::Caps::new_any(),
        )
        .expect("Could not create MSE audio_src_%u pad template")
    });

    static VIDEO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "video_src_%u",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &gstreamer::Caps::new_any(),
        )
        .expect("Could not create MSE video_src_%u pad template")
    });

    static TEXT_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "text_src_%u",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &gstreamer::Caps::new_any(),
        )
        .expect("Could not create MSE text_src_%u pad template")
    });

    static GENERIC_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "src_%u",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &gstreamer::Caps::new_any(),
        )
        .expect("Could not create MSE src_%u pad template")
    });

    pub struct ServoMediaSourceSrc {
        cat: gstreamer::DebugCategory,
        state: Mutex<SourceState>,
        observer: Arc<Mutex<Option<Arc<Mutex<GenericCallback<PlayerEvent>>>>>>,
        stopping: Arc<AtomicBool>,
    }

    impl ServoMediaSourceSrc {
        pub fn set_observer(&self, observer: Arc<Mutex<GenericCallback<PlayerEvent>>>) {
            if !self.stopping.load(Ordering::Acquire) {
                *self.observer.lock().unwrap() = Some(observer);
            }
        }

        pub fn duration(&self) -> Option<f64> {
            self.state.lock().unwrap().duration
        }

        pub fn has_explicit_duration(&self) -> bool {
            self.state.lock().unwrap().duration_set
        }

        pub fn prepare_stop(&self) {
            self.stopping.store(true, Ordering::Release);
            *self.observer.lock().unwrap() = None;
            let appsrcs: Vec<(gstreamer_app::AppSrc, bool)> = {
                let mut state = self.state.lock().unwrap();
                let mut list = Vec::with_capacity(state.tracks.len());
                for track in state.tracks.values_mut() {
                    track.pending_buffers.clear();
                    track.pending_encrypted.clear();
                    track.waiting_for_key = false;
                    track.pending_eos = false;
                    track.ended = true;
                    list.push((track.appsrc.clone(), track.ghost_pad.is_some()));
                }
                list
            };
            for (appsrc, is_exposed) in appsrcs {
                appsrc.set_callbacks(gstreamer_app::AppSrcCallbacks::builder().build());
                appsrc.send_event(gstreamer::event::FlushStart::new());
                let _ = appsrc.end_of_stream();
                if !is_exposed {
                    let _ = appsrc.set_state(gstreamer::State::Null);
                }
            }
        }

        pub fn live_seekable_range(&self) -> Option<(f64, f64)> {
            self.state.lock().unwrap().live_seekable_range
        }

        pub fn pads_finalized(&self) -> bool {
            self.state.lock().unwrap().pads_finalized
        }

        pub fn notify_seek(&self, target_secs: f64) {
            {
                let mut state = self.state.lock().unwrap();
                state.pending_seek = Some(target_secs);
            }
            send_player_event(&self.observer, PlayerEvent::MseSeek(target_secs));
        }

        pub fn is_seek_in_range(&self, time: f64) -> bool {
            if !time.is_finite() || time < 0.0 {
                return false;
            }
            let state = self.state.lock().unwrap();
            if let Some((start, end)) = state.live_seekable_range {
                return time >= start - 1e-6 && time <= end + 1e-6;
            }
            if let Some(dur) = state.duration {
                return time <= dur + 1e-6;
            }
            true
        }

        pub fn add_source_buffer(&self, id: u32, mime_type: &str) -> Result<(), PlayerError> {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut state = self.state.lock().unwrap();
            if state.tracks.contains_key(&id) {
                return Ok(());
            }

            let appsrc = gstreamer::ElementFactory::make("appsrc")
                .name(format!("mse-appsrc-{id}"))
                .build()
                .map_err(|err| {
                    PlayerError::Backend(format!("Could not create MSE appsrc {id}: {err:?}"))
                })?
                .downcast::<gstreamer_app::AppSrc>()
                .map_err(|_| PlayerError::Backend("Element is not an AppSrc".into()))?;

            appsrc.set_caps(None::<&gstreamer::Caps>);
            appsrc.set_max_bytes(MSE_APPSRC_MAX_BYTES);
            appsrc.set_block(false);
            appsrc.set_format(gstreamer::Format::Bytes);
            appsrc.set_stream_type(gstreamer_app::AppStreamType::Stream);

            let observer_need = self.observer.clone();
            let observer_enough = self.observer.clone();
            let stopping_need = self.stopping.clone();
            let stopping_enough = self.stopping.clone();

            appsrc.set_callbacks(
                gstreamer_app::AppSrcCallbacks::builder()
                    .need_data(move |_, _| {
                        if stopping_need.load(Ordering::Acquire) {
                            return;
                        }
                        send_player_event(&observer_need, PlayerEvent::MseNeedData(id));
                    })
                    .enough_data(move |_| {
                        if stopping_enough.load(Ordering::Acquire) {
                            return;
                        }
                        send_player_event(&observer_enough, PlayerEvent::MseEnoughData(id));
                    })
                    .build(),
            );

            let kind = infer_kind_from_mime(mime_type);
            let seek_epoch = state.seek_epoch;
            state.tracks.insert(
                id,
                TrackEntry {
                    id,
                    kind,
                    appsrc,
                    ghost_pad: None,
                    pending_buffers: VecDeque::new(),
                    has_data: false,
                    flushed_epoch: seek_epoch,
                    dirty_since_flush: false,
                    ended: false,
                    encryption_info: HashMap::new(),
                    pending_encrypted: VecDeque::new(),
                    waiting_for_key: false,
                    pending_eos: false,
                },
            );

            Ok(())
        }

        pub fn remove_source_buffer(&self, id: u32) -> Result<(), PlayerError> {
            let removed = {
                let mut state = self.state.lock().unwrap();
                state.committed_tracks.remove(&id);
                state.tracks.remove(&id)
            };

            let Some(entry) = removed else {
                return Ok(());
            };

            entry
                .appsrc
                .set_callbacks(gstreamer_app::AppSrcCallbacks::builder().build());

            if let Some(ref ghost_pad) = entry.ghost_pad {
                let _ = ghost_pad.set_active(false);
                let _ = self.obj().remove_pad(ghost_pad);
            }

            if entry.appsrc.parent().as_ref() == Some(self.obj().upcast_ref::<gstreamer::Object>())
            {
                let _ = entry.appsrc.set_state(gstreamer::State::Null);
                let _ = self.obj().remove(&entry.appsrc);
            }

            self.maybe_expose_pads_and_finalize(false);
            Ok(())
        }

        pub fn commit_tracks(&self, tracks: &[MseTrackDescriptor]) -> Result<(), PlayerError> {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            {
                let mut state = self.state.lock().unwrap();
                state.tracks_committed = true;
                state.committed_tracks.clear();

                // Group kinds by source_buffer_id: if a single SourceBuffer carries multiple
                // track kinds (muxed container), keep `None` so it uses generic `src_%u`.
                let mut kinds_per_sb: BTreeMap<u32, Option<MseTrackKind>> = BTreeMap::new();
                let mut mixed_sb: BTreeMap<u32, bool> = BTreeMap::new();
                for desc in tracks {
                    if !state.tracks.contains_key(&desc.source_buffer_id) {
                        return Err(PlayerError::Backend(format!(
                            "mse_commit_tracks referenced unknown SourceBuffer {}",
                            desc.source_buffer_id
                        )));
                    }
                    match kinds_per_sb.get(&desc.source_buffer_id) {
                        None => {
                            kinds_per_sb.insert(desc.source_buffer_id, Some(desc.kind));
                        },
                        Some(Some(existing)) if *existing != desc.kind => {
                            mixed_sb.insert(desc.source_buffer_id, true);
                        },
                        _ => {},
                    }
                }

                for (sb_id, kind_opt) in kinds_per_sb {
                    let resolved = if mixed_sb.get(&sb_id).copied().unwrap_or(false) {
                        None
                    } else {
                        kind_opt
                    };
                    state.committed_tracks.insert(sb_id, resolved);
                    if let Some(track) = state.tracks.get_mut(&sb_id) &&
                        resolved.is_some()
                    {
                        track.kind = resolved;
                    }
                }
            }

            self.maybe_expose_pads_and_finalize(false);
            Ok(())
        }

        pub fn set_clearkey_store(&self, store: Option<ClearKeyStore>) -> Result<(), PlayerError> {
            {
                let mut state = self.state.lock().unwrap();
                state.clearkey_store = store;
            }
            self.drain_encrypted_tracks()
        }

        pub fn notify_keys_updated(&self) -> Result<(), PlayerError> {
            self.drain_encrypted_tracks()
        }

        fn drain_encrypted_tracks(&self) -> Result<(), PlayerError> {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }

            let mut ready_to_push: Vec<(gstreamer_app::AppSrc, Vec<u8>)> = Vec::new();
            let mut need_expose = false;
            let mut eos_appsrcs: Vec<gstreamer_app::AppSrc> = Vec::new();
            let mut decode_error: Option<String> = None;
            let mut emit_waiting_for_key = false;

            {
                let mut state = self.state.lock().unwrap();
                let store = state.clearkey_store.clone();
                for track in state.tracks.values_mut() {
                    while let Some(enc_seg) = track.pending_encrypted.front() {
                        let Some(ref store_ref) = store else {
                            if !track.waiting_for_key {
                                track.waiting_for_key = true;
                                emit_waiting_for_key = true;
                            }
                            break;
                        };
                        match cenc::decrypt_media_segment(
                            enc_seg,
                            &track.encryption_info,
                            store_ref,
                        ) {
                            Ok(clear_bytes) => {
                                track.pending_encrypted.pop_front();
                                track.waiting_for_key = false;
                                track.has_data = true;
                                track.dirty_since_flush = true;
                                track.ended = false;
                                if track.ghost_pad.is_none() {
                                    track.pending_buffers.push_back(clear_bytes);
                                    need_expose = true;
                                } else {
                                    ready_to_push.push((track.appsrc.clone(), clear_bytes));
                                }
                            },
                            Err(CencDecryptError::MissingKey(_)) => {
                                if !track.waiting_for_key {
                                    track.waiting_for_key = true;
                                    emit_waiting_for_key = true;
                                }
                                break;
                            },
                            Err(CencDecryptError::InvalidData(msg)) => {
                                track.pending_encrypted.pop_front();
                                decode_error = Some(msg);
                                break;
                            },
                        }
                    }

                    if track.pending_encrypted.is_empty() &&
                        track.pending_eos &&
                        track.ghost_pad.is_some() &&
                        !track.ended
                    {
                        track.pending_eos = false;
                        track.ended = true;
                        eos_appsrcs.push(track.appsrc.clone());
                    }
                }
            }

            for (appsrc, bytes) in ready_to_push {
                self.push_bytes_to_appsrc(&appsrc, bytes)?;
            }

            if need_expose {
                self.maybe_expose_pads_and_finalize(false);
                let newly_eos: Vec<gstreamer_app::AppSrc> = {
                    let mut state = self.state.lock().unwrap();
                    let mut list = Vec::new();
                    for track in state.tracks.values_mut() {
                        if track.pending_encrypted.is_empty() &&
                            track.pending_eos &&
                            track.ghost_pad.is_some() &&
                            !track.ended
                        {
                            track.pending_eos = false;
                            track.ended = true;
                            list.push(track.appsrc.clone());
                        }
                    }
                    list
                };
                eos_appsrcs.extend(newly_eos);
            }

            for appsrc in eos_appsrcs {
                if let Err(err) = appsrc.end_of_stream() &&
                    !matches!(
                        err,
                        gstreamer::FlowError::Eos | gstreamer::FlowError::Flushing
                    )
                {
                    return Err(PlayerError::EOSFailed);
                }
            }

            if emit_waiting_for_key {
                send_player_event(&self.observer, PlayerEvent::WaitingForKey);
            }

            if let Some(msg) = decode_error {
                send_player_event(
                    &self.observer,
                    PlayerEvent::Error(format!("CENC decryption error: {msg}")),
                );
                return Err(PlayerError::BufferPushFailed);
            }

            Ok(())
        }

        pub fn push_data(&self, source_buffer_id: u32, mut data: Vec<u8>) -> Result<(), PlayerError> {
            if data.is_empty() || self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }

            let has_moov = contains_top_level_box(&data, *b"moov");
            let has_moof = contains_top_level_box(&data, *b"moof");
            let rewritten_init = if has_moov {
                cenc::rewrite_init_segment_to_clear(&data)
            } else {
                None
            };

            enum PushAction {
                Push(Option<(gstreamer_app::AppSrc, Vec<u8>)>),
                DrainEncrypted { emit_waiting: bool },
                InvalidData(String),
            }

            let action = {
                let mut state = self.state.lock().unwrap();
                let store = state.clearkey_store.clone();
                let Some(track) = state.tracks.get_mut(&source_buffer_id) else {
                    return Err(PlayerError::BufferPushFailed);
                };

                if let Some((clear_init, enc_map)) = rewritten_init {
                    track.encryption_info = enc_map;
                    data = clear_init;
                } else if has_moov {
                    track.encryption_info.clear();
                }

                if !track.encryption_info.is_empty() && has_moof {
                    if !track.pending_encrypted.is_empty() {
                        track.pending_encrypted.push_back(data);
                        track.has_data = true;
                        PushAction::DrainEncrypted {
                            emit_waiting: false,
                        }
                    } else if let Some(ref store_ref) = store {
                        match cenc::decrypt_media_segment(&data, &track.encryption_info, store_ref) {
                            Ok(clear_bytes) => {
                                track.waiting_for_key = false;
                                track.has_data = true;
                                track.dirty_since_flush = true;
                                track.ended = false;
                                if track.ghost_pad.is_none() {
                                    track.pending_buffers.push_back(clear_bytes);
                                    PushAction::Push(None)
                                } else {
                                    PushAction::Push(Some((track.appsrc.clone(), clear_bytes)))
                                }
                            },
                            Err(CencDecryptError::MissingKey(_)) => {
                                let emit_waiting = !track.waiting_for_key;
                                track.waiting_for_key = true;
                                track.pending_encrypted.push_back(data);
                                PushAction::DrainEncrypted { emit_waiting }
                            },
                            Err(CencDecryptError::InvalidData(msg)) => PushAction::InvalidData(msg),
                        }
                    } else {
                        let emit_waiting = !track.waiting_for_key;
                        track.waiting_for_key = true;
                        track.pending_encrypted.push_back(data);
                        PushAction::DrainEncrypted { emit_waiting }
                    }
                } else {
                    track.has_data = true;
                    track.dirty_since_flush = true;
                    track.ended = false;
                    if track.ghost_pad.is_none() {
                        track.pending_buffers.push_back(data);
                        PushAction::Push(None)
                    } else {
                        PushAction::Push(Some((track.appsrc.clone(), data)))
                    }
                }
            };

            match action {
                PushAction::Push(Some((appsrc, bytes))) => {
                    self.push_bytes_to_appsrc(&appsrc, bytes)?;
                },
                PushAction::Push(None) => {
                    self.maybe_expose_pads_and_finalize(false);
                },
                PushAction::DrainEncrypted { emit_waiting } => {
                    if emit_waiting {
                        send_player_event(&self.observer, PlayerEvent::WaitingForKey);
                    }
                    self.drain_encrypted_tracks()?;
                },
                PushAction::InvalidData(msg) => {
                    send_player_event(
                        &self.observer,
                        PlayerEvent::Error(format!("CENC decryption error: {msg}")),
                    );
                    return Err(PlayerError::BufferPushFailed);
                },
            }

            Ok(())
        }

        pub fn flush_track(&self, source_buffer_id: u32) -> Result<(), PlayerError> {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            let appsrc_to_flush = {
                let mut state = self.state.lock().unwrap();
                let seek_epoch = state.seek_epoch;
                let Some(track) = state.tracks.get_mut(&source_buffer_id) else {
                    return Err(PlayerError::Backend(format!(
                        "Unknown MSE SourceBuffer {}",
                        source_buffer_id
                    )));
                };
                track.pending_buffers.clear();
                track.pending_encrypted.clear();
                track.waiting_for_key = false;
                track.pending_eos = false;
                track.ended = false;
                if track.ghost_pad.is_some() &&
                    (track.flushed_epoch < seek_epoch || track.dirty_since_flush)
                {
                    track.flushed_epoch = seek_epoch;
                    track.dirty_since_flush = false;
                    Some(track.appsrc.clone())
                } else {
                    track.flushed_epoch = seek_epoch;
                    track.dirty_since_flush = false;
                    None
                }
            };

            if let Some(appsrc) = appsrc_to_flush {
                appsrc.send_event(gstreamer::event::FlushStart::new());
                appsrc.send_event(gstreamer::event::FlushStop::builder(true).build());
            }

            Ok(())
        }

        pub fn end_of_stream(
            &self,
            error: Option<MseEndOfStreamError>,
        ) -> Result<(), PlayerError> {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            self.maybe_expose_pads_and_finalize(true);

            let appsrcs: Vec<gstreamer_app::AppSrc> = {
                let mut state = self.state.lock().unwrap();
                let mut list = Vec::new();
                for track in state.tracks.values_mut() {
                    if !track.pending_encrypted.is_empty() {
                        track.pending_eos = true;
                    } else if track.ghost_pad.is_some() && !track.ended {
                        track.ended = true;
                        list.push(track.appsrc.clone());
                    }
                }
                list
            };

            for appsrc in appsrcs {
                if let Err(err) = appsrc.end_of_stream() &&
                    !matches!(
                        err,
                        gstreamer::FlowError::Eos | gstreamer::FlowError::Flushing
                    )
                {
                    return Err(PlayerError::EOSFailed);
                }
            }

            if let Some(err) = error {
                let msg = match err {
                    MseEndOfStreamError::Network => "MSE endOfStream(network) error",
                    MseEndOfStreamError::Decode => "MSE endOfStream(decode) error",
                };
                send_player_event(&self.observer, PlayerEvent::Error(msg.to_owned()));
            }

            Ok(())
        }

        pub fn set_duration(&self, duration: Option<f64>) -> Result<(), PlayerError> {
            let normalized = match duration {
                Some(d) if d.is_nan() || d < 0.0 => {
                    return Err(PlayerError::Backend(format!(
                        "Invalid MSE duration: {d}"
                    )));
                },
                Some(d) if d.is_finite() && d < 1e12 => Some(d),
                _ => None,
            };

            {
                let mut state = self.state.lock().unwrap();
                state.duration_set = true;
                state.duration = normalized;
            }

            let _ = self.obj().post_message(
                gstreamer::message::DurationChanged::builder()
                    .src(&*self.obj())
                    .build(),
            );
            Ok(())
        }

        pub fn set_live_seekable_range(&self, start: f64, end: f64) -> Result<(), PlayerError> {
            if start.is_nan() || end.is_nan() || !start.is_finite() || !end.is_finite() ||
                start < 0.0 ||
                end < start
            {
                return Err(PlayerError::Backend(format!(
                    "Invalid MSE live seekable range [{start}, {end}]"
                )));
            }

            let range = if start == 0.0 && end == 0.0 {
                None
            } else {
                Some((start, end))
            };
            self.state.lock().unwrap().live_seekable_range = range;
            Ok(())
        }

        fn push_bytes_to_appsrc(
            &self,
            appsrc: &gstreamer_app::AppSrc,
            data: Vec<u8>,
        ) -> Result<(), PlayerError> {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            let buffer = gstreamer::Buffer::from_slice(data);
            match appsrc.push_buffer(buffer) {
                Ok(_) | Err(gstreamer::FlowError::Flushing) => Ok(()),
                Err(gstreamer::FlowError::Eos) if self.stopping.load(Ordering::Acquire) => Ok(()),
                Err(_) => Err(PlayerError::BufferPushFailed),
            }
        }

        fn maybe_expose_pads_and_finalize(&self, force_eos: bool) {
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            let (specs, fire_no_more_pads) = {
                let mut state = self.state.lock().unwrap();

                let target_ids: Vec<u32> = if !state.committed_tracks.is_empty() {
                    state
                        .committed_tracks
                        .keys()
                        .copied()
                        .filter(|id| state.tracks.contains_key(id))
                        .collect()
                } else if force_eos {
                    state.tracks.keys().copied().collect()
                } else {
                    return;
                };

                if target_ids.is_empty() {
                    return;
                }

                if !force_eos {
                    if !state.tracks_committed {
                        return;
                    }
                    let all_have_init_data = target_ids
                        .iter()
                        .all(|id| state.tracks.get(id).is_some_and(|t| t.has_data));
                    if !all_have_init_data {
                        return;
                    }
                } else if !state.tracks.values().any(|t| t.has_data) && state.pads_finalized {
                    return;
                }

                let mut specs = Vec::new();
                for id in target_ids {
                    if let Some(track) = state.tracks.get(&id) &&
                        track.ghost_pad.is_none()
                    {
                        specs.push(ExposedPadSpec {
                            id: track.id,
                            kind: track.kind,
                            appsrc: track.appsrc.clone(),
                        });
                    }
                }

                let fire_no_more_pads = !state.pads_finalized;
                if fire_no_more_pads {
                    state.pads_finalized = true;
                }

                (specs, fire_no_more_pads)
            };

            if specs.is_empty() && !fire_no_more_pads {
                return;
            }

            let bin = self.obj();
            let mut added_pads: Vec<(u32, gstreamer::GhostPad, gstreamer_app::AppSrc)> =
                Vec::with_capacity(specs.len());

            for spec in specs {
                if spec.appsrc.parent().is_none() {
                    if let Err(err) = bin.add(&spec.appsrc) {
                        gstreamer::warning!(
                            self.cat,
                            obj = &*bin,
                            "Could not add MSE appsrc {} to bin: {:?}",
                            spec.id,
                            err
                        );
                        continue;
                    }
                }

                let (template, pad_name) = match spec.kind {
                    Some(MseTrackKind::Audio) => {
                        (&*AUDIO_SRC_PAD_TEMPLATE, format!("audio_src_{}", spec.id))
                    },
                    Some(MseTrackKind::Video) => {
                        (&*VIDEO_SRC_PAD_TEMPLATE, format!("video_src_{}", spec.id))
                    },
                    Some(MseTrackKind::Text) => {
                        (&*TEXT_SRC_PAD_TEMPLATE, format!("text_src_{}", spec.id))
                    },
                    None => (&*GENERIC_SRC_PAD_TEMPLATE, format!("src_{}", spec.id)),
                };

                let track_id = spec.id;
                let ghost_pad = gstreamer::GhostPad::builder_from_template(template)
                    .name(&pad_name)
                    .query_function(|pad, parent, query| {
                        Self::catch_panic_pad_function(
                            parent,
                            || false,
                            |src| src.handle_pad_query(pad, query),
                        )
                    })
                    .event_function(move |pad, parent, event| {
                        Self::catch_panic_pad_function(
                            parent,
                            || false,
                            |src| src.handle_pad_event(track_id, pad, event),
                        )
                    })
                    .build();

                let target_pad = spec
                    .appsrc
                    .static_pad("src")
                    .expect("AppSrc always has a static src pad");
                if let Err(err) = ghost_pad.set_target(Some(&target_pad)) {
                    gstreamer::warning!(
                        self.cat,
                        obj = &*bin,
                        "Could not set target on GhostPad {}: {:?}",
                        pad_name,
                        err
                    );
                    continue;
                }
                if let Err(err) = ghost_pad.set_active(true) {
                    gstreamer::warning!(
                        self.cat,
                        obj = &*bin,
                        "Could not activate GhostPad {}: {:?}",
                        pad_name,
                        err
                    );
                }
                if let Err(err) = bin.add_pad(&ghost_pad) {
                    gstreamer::warning!(
                        self.cat,
                        obj = &*bin,
                        "Could not add GhostPad {} to bin: {:?}",
                        pad_name,
                        err
                    );
                    continue;
                }

                added_pads.push((spec.id, ghost_pad, spec.appsrc));
            }

            if fire_no_more_pads {
                bin.no_more_pads();
            }

            // Now that urisourcebin/decodebin3 have linked the GhostPads, sync each AppSrc
            // state with the parent bin and drain any buffered pre-exposure chunks.
            let mut drained_chunks: Vec<(gstreamer_app::AppSrc, VecDeque<Vec<u8>>)> =
                Vec::with_capacity(added_pads.len());
            {
                let mut state = self.state.lock().unwrap();
                for (id, ghost_pad, appsrc) in &added_pads {
                    if let Some(track) = state.tracks.get_mut(id) {
                        track.ghost_pad = Some(ghost_pad.clone());
                        let pending = std::mem::take(&mut track.pending_buffers);
                        if !pending.is_empty() {
                            drained_chunks.push((appsrc.clone(), pending));
                        }
                    }
                }
            }

            for (_, _, appsrc) in &added_pads {
                let _ = appsrc.sync_state_with_parent();
            }

            for (appsrc, chunks) in drained_chunks {
                for chunk in chunks {
                    if self.stopping.load(Ordering::Acquire) {
                        return;
                    }
                    let _ = self.push_bytes_to_appsrc(&appsrc, chunk);
                }
            }
        }

        fn handle_pad_query(&self, pad: &gstreamer::GhostPad, query: &mut gstreamer::QueryRef) -> bool {
            match query.view_mut() {
                gstreamer::QueryViewMut::Duration(ref mut q) => {
                    if q.format() == gstreamer::Format::Time {
                        let dur = self.state.lock().unwrap().duration;
                        match dur.and_then(secs_to_clocktime) {
                            Some(ct) => q.set(ct),
                            None => q.set(gstreamer::ClockTime::NONE),
                        }
                        return true;
                    }
                    gstreamer::Pad::query_default(pad, Some(&*self.obj()), query)
                },
                gstreamer::QueryViewMut::Seeking(ref mut q) => {
                    if q.format() == gstreamer::Format::Time {
                        let state = self.state.lock().unwrap();
                        if let Some((start_s, end_s)) = state.live_seekable_range {
                            let start_ct =
                                secs_to_clocktime(start_s).unwrap_or(gstreamer::ClockTime::ZERO);
                            let end_ct = secs_to_clocktime(end_s);
                            q.set(true, Some(start_ct), end_ct);
                        } else if let Some(dur_s) = state.duration {
                            let end_ct = secs_to_clocktime(dur_s);
                            q.set(true, Some(gstreamer::ClockTime::ZERO), end_ct);
                        } else {
                            q.set(
                                true,
                                Some(gstreamer::ClockTime::ZERO),
                                gstreamer::ClockTime::NONE,
                            );
                        }
                        return true;
                    }
                    gstreamer::Pad::query_default(pad, Some(&*self.obj()), query)
                },
                _ => gstreamer::Pad::query_default(pad, Some(&*self.obj()), query),
            }
        }

        fn handle_pad_event(
            &self,
            track_id: u32,
            pad: &gstreamer::GhostPad,
            event: gstreamer::Event,
        ) -> bool {
            if self.stopping.load(Ordering::Acquire) {
                return gstreamer::Pad::event_default(pad, Some(&*self.obj()), event);
            }
            if let gstreamer::EventView::Seek(seek_ev) = event.view() {
                if let gstreamer::GenericFormattedValue::Time(Some(start_time)) = seek_ev.start() {
                    let target_secs = start_time.seconds_f64();
                    let seqnum = event.seqnum();

                    let (emit_seek, flush_appsrc) = {
                        let mut state = self.state.lock().unwrap();
                        let mut emit = false;
                        if state.last_seek_seqnum != Some(seqnum) {
                            state.last_seek_seqnum = Some(seqnum);
                            state.seek_epoch = state.seek_epoch.wrapping_add(1);
                            state.pending_seek = Some(target_secs);
                            emit = true;
                        }
                        let current_epoch = state.seek_epoch;
                        let appsrc_opt = if let Some(track) = state.tracks.get_mut(&track_id) &&
                            track.flushed_epoch < current_epoch
                        {
                            track.flushed_epoch = current_epoch;
                            track.dirty_since_flush = false;
                            track.ended = false;
                            track.pending_buffers.clear();
                            track.pending_encrypted.clear();
                            track.waiting_for_key = false;
                            track.pending_eos = false;
                            Some(track.appsrc.clone())
                        } else {
                            None
                        };
                        (emit, appsrc_opt)
                    };

                    if let Some(appsrc) = flush_appsrc {
                        appsrc.send_event(gstreamer::event::FlushStart::new());
                        appsrc.send_event(gstreamer::event::FlushStop::builder(true).build());
                    }

                    if emit_seek {
                        send_player_event(&self.observer, PlayerEvent::MseSeek(target_secs));
                    }
                    return true;
                }
            }

            gstreamer::Pad::event_default(pad, Some(&*self.obj()), event)
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ServoMediaSourceSrc {
        const NAME: &'static str = "ServoMediaSourceSrc";
        type Type = super::ServoMediaSourceSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        fn with_class(_klass: &Self::Class) -> Self {
            Self {
                cat: gstreamer::DebugCategory::new(
                    "servomediasourcesrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo MediaSource source"),
                ),
                state: Mutex::new(SourceState::default()),
                observer: Arc::new(Mutex::new(None)),
                stopping: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl ObjectImpl for ServoMediaSourceSrc {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoMediaSourceSrc {}

    impl ElementImpl for ServoMediaSourceSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo MediaSource Source",
                        "Source/Audio/Video",
                        "Feed player with Media Source Extensions (MSE) container segments",
                        "Servo developers",
                    )
                });
            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                vec![
                    AUDIO_SRC_PAD_TEMPLATE.clone(),
                    VIDEO_SRC_PAD_TEMPLATE.clone(),
                    TEXT_SRC_PAD_TEMPLATE.clone(),
                    GENERIC_SRC_PAD_TEMPLATE.clone(),
                ]
            });
            PAD_TEMPLATES.as_ref()
        }
    }

    impl BinImpl for ServoMediaSourceSrc {}

    impl URIHandlerImpl for ServoMediaSourceSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["mediasource"]
        }

        fn uri(&self) -> Option<String> {
            Some("mediasource://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(parsed) = Url::parse(uri) &&
                parsed.scheme() == "mediasource"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                &format!("Invalid MediaSource URI '{uri}'"),
            ))
        }
    }
}

glib::wrapper! {
    pub struct ServoMediaSourceSrc(ObjectSubclass<imp::ServoMediaSourceSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoMediaSourceSrc {}
unsafe impl Sync for ServoMediaSourceSrc {}

impl ServoMediaSourceSrc {
    pub fn set_observer(&self, observer: Arc<Mutex<GenericCallback<PlayerEvent>>>) {
        self.imp().set_observer(observer);
    }

    pub fn duration(&self) -> Option<f64> {
        self.imp().duration()
    }

    pub fn has_explicit_duration(&self) -> bool {
        self.imp().has_explicit_duration()
    }

    pub fn prepare_stop(&self) {
        self.imp().prepare_stop();
    }

    pub fn live_seekable_range(&self) -> Option<(f64, f64)> {
        self.imp().live_seekable_range()
    }

    pub fn pads_finalized(&self) -> bool {
        self.imp().pads_finalized()
    }

    pub fn notify_seek(&self, target_secs: f64) {
        self.imp().notify_seek(target_secs);
    }

    pub fn is_seek_in_range(&self, time: f64) -> bool {
        self.imp().is_seek_in_range(time)
    }

    pub fn add_source_buffer(&self, id: u32, mime_type: &str) -> Result<(), PlayerError> {
        self.imp().add_source_buffer(id, mime_type)
    }

    pub fn remove_source_buffer(&self, id: u32) -> Result<(), PlayerError> {
        self.imp().remove_source_buffer(id)
    }

    pub fn commit_tracks(&self, tracks: &[MseTrackDescriptor]) -> Result<(), PlayerError> {
        self.imp().commit_tracks(tracks)
    }

    pub fn push_data(&self, source_buffer_id: u32, data: Vec<u8>) -> Result<(), PlayerError> {
        self.imp().push_data(source_buffer_id, data)
    }

    pub fn flush_track(&self, source_buffer_id: u32) -> Result<(), PlayerError> {
        self.imp().flush_track(source_buffer_id)
    }

    pub fn end_of_stream(&self, error: Option<MseEndOfStreamError>) -> Result<(), PlayerError> {
        self.imp().end_of_stream(error)
    }

    pub fn set_duration(&self, duration: Option<f64>) -> Result<(), PlayerError> {
        self.imp().set_duration(duration)
    }

    pub fn set_live_seekable_range(&self, start: f64, end: f64) -> Result<(), PlayerError> {
        self.imp().set_live_seekable_range(start, end)
    }

    pub fn set_clearkey_store(&self, store: Option<ClearKeyStore>) -> Result<(), PlayerError> {
        self.imp().set_clearkey_store(store)
    }

    pub fn notify_keys_updated(&self) -> Result<(), PlayerError> {
        self.imp().notify_keys_updated()
    }
}

pub fn register_servo_media_source_src() -> Result<(), glib::BoolError> {
    let mut err_out: Option<glib::BoolError> = None;
    REGISTER_MSE_SRC.call_once(|| {
        if let Err(err) = gstreamer::Element::register(
            None,
            "servomediasourcesrc",
            gstreamer::Rank::NONE,
            ServoMediaSourceSrc::static_type(),
        ) {
            err_out = Some(err);
        }
    });
    match err_out {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
