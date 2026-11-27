/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::cformat;
use script_bindings::reflector::reflect_dom_object;
use servo_media::player::mse::{
    AppendMode as MseAppendMode, BUFFERED_ADJACENCY_TOLERANCE_SECS, EVICTION_PROTECT_AFTER_SECS,
    EVICTION_PROTECT_BEFORE_SECS, MseError, MseTrackBuffer, MseTrackType,
};
use servo_media::player::{MseTrackDescriptor, MseTrackKind, Player};
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::ReadyState as MediaSourceReadyState;
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediasource::{MediaSource, mse_debug_enabled};
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    media_source: Dom<MediaSource>,
    source_buffer_id: u32,
    mime_type: DomRefCell<String>,
    updating: Cell<bool>,
    removed: Cell<bool>,
    generation_id: Cell<u32>,
    #[ignore_malloc_size_of = "servo_media"]
    #[no_trace]
    track_buffer: DomRefCell<MseTrackBuffer>,
    last_pushed_init_id: Cell<Option<u64>>,
    last_pushed_media_id: Cell<u64>,
    last_pushed_end_pts: Cell<Option<f64>>,
    pushed_media_ids: DomRefCell<HashSet<u64>>,
    player_needs_data: Cell<bool>,
}

impl SourceBuffer {
    pub(crate) fn new_inherited(
        media_source: &MediaSource,
        source_buffer_id: u32,
        mime_type: String,
    ) -> Self {
        let track_buffer = if mime_type.trim().to_ascii_lowercase().starts_with("audio/") {
            MseTrackBuffer::new_audio()
        } else {
            MseTrackBuffer::new_video()
        };
        Self {
            eventtarget: EventTarget::new_inherited(),
            media_source: Dom::from_ref(media_source),
            source_buffer_id,
            mime_type: DomRefCell::new(mime_type),
            updating: Cell::new(false),
            removed: Cell::new(false),
            generation_id: Cell::new(0),
            track_buffer: DomRefCell::new(track_buffer),
            last_pushed_init_id: Cell::new(None),
            last_pushed_media_id: Cell::new(0),
            last_pushed_end_pts: Cell::new(None),
            pushed_media_ids: DomRefCell::new(HashSet::new()),
            player_needs_data: Cell::new(true),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        media_source: &MediaSource,
        source_buffer_id: u32,
        mime_type: String,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(
                media_source,
                source_buffer_id,
                mime_type,
            )),
            global,
        )
    }

    pub(crate) fn source_buffer_id(&self) -> u32 {
        self.source_buffer_id
    }

    pub(crate) fn is_updating(&self) -> bool {
        self.updating.get()
    }

    pub(crate) fn is_removed(&self) -> bool {
        self.removed.get()
    }

    pub(crate) fn mark_removed(&self) {
        self.removed.set(true);
        self.generation_id.set(self.generation_id.get().wrapping_add(1));
        self.updating.set(false);
        self.track_buffer.borrow_mut().abort();
    }

    pub(crate) fn buffered_ranges(&self) -> Vec<Range<f64>> {
        self.track_buffer.borrow().buffered()
    }

    pub(crate) fn highest_end_timestamp(&self) -> f64 {
        self.track_buffer.borrow().highest_end_timestamp()
    }

    pub(crate) fn set_playback_position(&self, position: f64) {
        self.track_buffer.borrow_mut().set_playback_position(position);
    }

    pub(crate) fn has_init_segment(&self) -> bool {
        self.track_buffer.borrow().active_init_segment().is_some()
    }

    pub(crate) fn seek_keyframe_start(&self, target: f64) -> Option<f64> {
        self.track_buffer.borrow().seek_keyframe_start(target)
    }

    pub(crate) fn pushed_to_end(&self) -> bool {
        let tb = self.track_buffer.borrow();
        let Some(last) = tb.media_segments().last() else {
            return true;
        };
        self.last_pushed_end_pts
            .get()
            .is_some_and(|end| end + BUFFERED_ADJACENCY_TOLERANCE_SECS >= last.end_pts)
    }

    pub(crate) fn track_descriptors(&self) -> Vec<MseTrackDescriptor> {
        let tb = self.track_buffer.borrow();
        let Some(init) = tb.active_init_segment() else {
            return Vec::new();
        };
        let mime_type = self.mime_type.borrow().clone();
        init.tracks
            .iter()
            .map(|track| MseTrackDescriptor {
                source_buffer_id: self.source_buffer_id,
                track_id: track.track_id,
                kind: match track.track_type {
                    MseTrackType::Video => MseTrackKind::Video,
                    MseTrackType::Audio => MseTrackKind::Audio,
                    MseTrackType::Text | MseTrackType::Unknown => MseTrackKind::Text,
                },
                mime_type: mime_type.clone(),
                codec: track.codec.clone(),
            })
            .collect()
    }

    pub(crate) fn set_player_needs_data(&self, needs_data: bool, player: Option<&Arc<Mutex<dyn Player>>>) {
        self.player_needs_data.set(needs_data);
        if needs_data {
            if let Some(player) = player {
                self.push_pending_to_player(player);
            }
        }
    }

    /// Pushes any unpushed initialization and media segments to the player in timeline order,
    /// handling ABR initialization segment switches without re-pushing older segments.
    pub(crate) fn push_pending_to_player(&self, player: &Arc<Mutex<dyn Player>>) {
        if !self.player_needs_data.get() {
            return;
        }

        let chunks_to_push: Vec<(Vec<u8>, u64, Option<(u64, f64)>)> = {
            let tb = self.track_buffer.borrow();
            let pushed_ids = self.pushed_media_ids.borrow();
            let mut chunks: Vec<(Vec<u8>, u64, Option<(u64, f64)>)> = Vec::new();
            let mut cur_init_id = self.last_pushed_init_id.get();

            if let Some(mut cur_end) = self.last_pushed_end_pts.get() {
                for seg in tb.media_segments() {
                    if seg.end_pts <= cur_end + 1e-6 || pushed_ids.contains(&seg.id) {
                        continue;
                    }
                    if seg.start_pts > cur_end + BUFFERED_ADJACENCY_TOLERANCE_SECS {
                        break;
                    }
                    if cur_init_id != Some(seg.init_id) {
                        if let Some(init) = tb.init_segments().get(&seg.init_id) {
                            if !init.bytes.is_empty() {
                                chunks.push(((*init.bytes).clone(), init.id, None));
                            }
                        }
                        cur_init_id = Some(seg.init_id);
                    }
                    if !seg.bytes.is_empty() {
                        chunks.push((
                            (*seg.bytes).clone(),
                            seg.init_id,
                            Some((seg.id, seg.end_pts)),
                        ));
                    }
                    cur_end = cur_end.max(seg.end_pts);
                }
            } else {
                let seek_segs = tb.segments_for_seek_abr(tb.playback_position());
                if seek_segs.is_empty() {
                    if let Some(active_init) = tb.active_init_segment() {
                        if cur_init_id != Some(active_init.id) && !active_init.bytes.is_empty() {
                            chunks.push(((*active_init.bytes).clone(), active_init.id, None));
                        }
                    }
                } else {
                    for (init, seg) in seek_segs {
                        if pushed_ids.contains(&seg.id) {
                            continue;
                        }
                        if cur_init_id != Some(init.id) {
                            if !init.bytes.is_empty() {
                                chunks.push(((*init.bytes).clone(), init.id, None));
                            }
                            cur_init_id = Some(init.id);
                        }
                        if !seg.bytes.is_empty() {
                            chunks.push((
                                (*seg.bytes).clone(),
                                init.id,
                                Some((seg.id, seg.end_pts)),
                            ));
                        }
                    }
                }
            }

            chunks
        };

        if chunks_to_push.is_empty() {
            return;
        }

        let guard = player.lock().unwrap();
        let mut pushed_ids = self.pushed_media_ids.borrow_mut();
        for (bytes, init_id, media_info) in chunks_to_push {
            if let Err(err) = guard.mse_push_data(self.source_buffer_id, bytes) {
                warn!(
                    "mse_push_data failed for SourceBuffer {}: {err:?}",
                    self.source_buffer_id
                );
                break;
            }
            self.last_pushed_init_id.set(Some(init_id));
            if let Some((media_id, end_pts)) = media_info {
                pushed_ids.insert(media_id);
                self.last_pushed_media_id
                    .set(self.last_pushed_media_id.get().max(media_id));
                let next_end = self
                    .last_pushed_end_pts
                    .get()
                    .map_or(end_pts, |prev| prev.max(end_pts));
                self.last_pushed_end_pts.set(Some(next_end));
            }
        }
    }

    /// Flushes this track in the player and re-pushes matching `InitSegment`(s)
    /// plus contiguous `MediaSegment`s from the nearest keyframe at/before `target`,
    /// preserving ABR `InitSegment` switches along the contiguous run.
    pub(crate) fn flush_and_push_for_seek(
        &self,
        player: &Arc<Mutex<dyn Player>>,
        target: f64,
    ) -> bool {
        self.player_needs_data.set(true);

        let seek_chunks: Vec<(Vec<u8>, u64, Option<(u64, f64)>)> = {
            let tb = self.track_buffer.borrow();
            let segs = tb.segments_for_seek_abr(target);
            let mut chunks = Vec::with_capacity(segs.len() + 1);
            let mut cur_init_id: Option<u64> = None;
            for (init, seg) in segs {
                if cur_init_id != Some(init.id) {
                    if !init.bytes.is_empty() {
                        chunks.push(((*init.bytes).clone(), init.id, None));
                    }
                    cur_init_id = Some(init.id);
                }
                if !seg.bytes.is_empty() {
                    chunks.push((
                        (*seg.bytes).clone(),
                        init.id,
                        Some((seg.id, seg.end_pts)),
                    ));
                }
            }
            chunks
        };

        self.last_pushed_init_id.set(None);
        self.last_pushed_media_id.set(0);
        self.last_pushed_end_pts.set(None);
        self.pushed_media_ids.borrow_mut().clear();

        let guard = player.lock().unwrap();
        if let Err(err) = guard.mse_flush_track(self.source_buffer_id) {
            warn!(
                "mse_flush_track failed for SourceBuffer {}: {err:?}",
                self.source_buffer_id
            );
        }

        if seek_chunks.is_empty() {
            return false;
        }

        let mut pushed_any_media = false;
        let mut pushed_ids = self.pushed_media_ids.borrow_mut();
        for (bytes, init_id, media_info) in seek_chunks {
            if let Err(err) = guard.mse_push_data(self.source_buffer_id, bytes) {
                warn!(
                    "mse_push_data on seek failed for SourceBuffer {}: {err:?}",
                    self.source_buffer_id
                );
                break;
            }
            self.last_pushed_init_id.set(Some(init_id));
            if let Some((media_id, end_pts)) = media_info {
                pushed_ids.insert(media_id);
                self.last_pushed_media_id
                    .set(self.last_pushed_media_id.get().max(media_id));
                let next_end = self
                    .last_pushed_end_pts
                    .get()
                    .map_or(end_pts, |prev| prev.max(end_pts));
                self.last_pushed_end_pts.set(Some(next_end));
                pushed_any_media = true;
            }
        }
        pushed_any_media
    }

    pub(crate) fn sync_to_player(&self, player: &Arc<Mutex<dyn Player>>) {
        {
            let guard = player.lock().unwrap();
            let _ = guard.mse_add_source_buffer(self.source_buffer_id, &self.mime_type.borrow());
        }
        self.last_pushed_init_id.set(None);
        self.last_pushed_media_id.set(0);
        self.last_pushed_end_pts.set(None);
        self.pushed_media_ids.borrow_mut().clear();
        self.player_needs_data.set(true);
        self.push_pending_to_player(player);
    }

    pub(crate) fn flush_end_of_stream(&self, player: Option<&Arc<Mutex<dyn Player>>>) {
        if self.track_buffer.borrow_mut().end_of_stream().is_ok() {
            if let Some(player) = player {
                self.push_pending_to_player(player);
            }
        }
    }

    pub(crate) fn abort_internal(&self) {
        self.generation_id.set(self.generation_id.get().wrapping_add(1));
        self.track_buffer.borrow_mut().abort();
        if self.updating.replace(false) {
            self.queue_abort_and_updateend();
        }
    }

    fn queue_abort_and_updateend(&self) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(sourcebuffer_abort_events: move |cx| {
                let this = this.root();
                if this.is_removed() {
                    return;
                }
                let target = this.upcast::<EventTarget>();
                target.fire_event(cx, Atom::from("abort"));
                target.fire_event(cx, Atom::from("updateend"));
            }));
    }

    fn check_not_removed_or_updating(&self) -> ErrorResult {
        if self.is_removed() {
            return Err(Error::InvalidState(Some(
                "SourceBuffer has been removed from MediaSource".into(),
            )));
        }
        if self.updating.get() {
            return Err(Error::InvalidState(Some(
                "SourceBuffer is currently updating".into(),
            )));
        }
        Ok(())
    }

    fn can_accept_append_bytes(&self, incoming_len: usize) -> ErrorResult {
        let tb = self.track_buffer.borrow();
        let quota = tb.quota_bytes();
        let used = tb.used_bytes();
        if incoming_len > quota {
            return Err(Error::QuotaExceeded {
                quota: Some(Finite::wrap(quota as f64)),
                requested: Some(Finite::wrap(incoming_len as f64)),
            });
        }
        if used.saturating_add(incoming_len) <= quota {
            return Ok(());
        }

        let playback_pos = tb.playback_position();
        let protect_start = (playback_pos - EVICTION_PROTECT_BEFORE_SECS).max(0.0);
        let protect_end = playback_pos + EVICTION_PROTECT_AFTER_SECS;

        let segments = tb.media_segments();
        let mut evictable = 0usize;
        let mut i = 0;
        while i < segments.len() {
            let start = i;
            i += 1;
            while i < segments.len()
                && !segments[i].is_keyframe
                && segments[i].start_pts
                    <= segments[i - 1].end_pts + BUFFERED_ADJACENCY_TOLERANCE_SECS
            {
                i += 1;
            }
            let g_start = segments[start].start_pts;
            let g_end = segments[i - 1].end_pts;
            if g_end <= protect_start || g_start >= protect_end {
                evictable += segments[start..i].iter().map(|s| s.byte_len).sum::<usize>();
            }
        }

        if used.saturating_sub(evictable).saturating_add(incoming_len) > quota {
            return Err(Error::QuotaExceeded {
                quota: Some(Finite::wrap(quota as f64)),
                requested: Some(Finite::wrap(incoming_len as f64)),
            });
        }
        Ok(())
    }

    fn run_async_append(&self, cx: &mut JSContext, bytes: Vec<u8>, generation_id: u32) {
        if generation_id != self.generation_id.get() || self.is_removed() || !self.updating.get() {
            return;
        }

        let target = self.upcast::<EventTarget>();
        target.fire_event(cx, Atom::from("updatestart"));

        if generation_id != self.generation_id.get() || self.is_removed() || !self.updating.get() {
            return;
        }

        let append_result = if bytes.is_empty() {
            Ok(Default::default())
        } else {
            self.track_buffer.borrow_mut().append(&bytes)
        };

        match append_result {
            Ok(res) => {
                if mse_debug_enabled() {
                    let buffered = self.track_buffer.borrow().buffered();
                    eprintln!(
                        "[mse-debug] SourceBuffer {} append ok (bytes={}, inits={}, media={}, evicted={}, buffered={buffered:?})",
                        self.source_buffer_id,
                        bytes.len(),
                        res.init_segments_parsed,
                        res.media_segments_parsed,
                        res.evicted_bytes
                    );
                    if res.init_segments_parsed == 0
                        && res.media_segments_parsed == 0
                        && res.evicted_bytes == 0
                        && !bytes.is_empty()
                    {
                        let head_len = bytes.len().min(64);
                        let hex_head: String = bytes[..head_len]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        eprintln!(
                            "[mse-debug] sb {} unparsed chunk (len={}) head={hex_head}",
                            self.source_buffer_id,
                            bytes.len()
                        );
                    }
                }
                for (init_data_type, init_data) in res.encrypted_init_data {
                    self.media_source
                        .notify_encrypted_init_data(init_data_type, init_data);
                }
                if let Some(player) = self.media_source.attached_player() {
                    self.push_pending_to_player(&player);
                    self.media_source.maybe_commit_tracks(&player);
                }
                self.media_source.notify_buffers_updated(cx);
                self.updating.set(false);
                target.fire_event(cx, Atom::from("update"));
                target.fire_event(cx, Atom::from("updateend"));
            },
            Err(err) => {
                if mse_debug_enabled() {
                    eprintln!(
                        "[mse-debug] SourceBuffer {} append FAILED (bytes={}): {err}",
                        self.source_buffer_id,
                        bytes.len()
                    );
                }
                warn!(
                    "SourceBuffer {} appendBuffer failed: {err}",
                    self.source_buffer_id
                );
                self.updating.set(false);
                target.fire_event(cx, Atom::from("error"));
                target.fire_event(cx, Atom::from("updateend"));
                if matches!(err, MseError::InvalidData(_) | MseError::MissingInitSegment) {
                    self.media_source.on_decode_error(cx);
                }
            },
        }
    }

    fn run_async_remove(&self, cx: &mut JSContext, start: f64, end: f64, generation_id: u32) {
        if generation_id != self.generation_id.get() || self.is_removed() || !self.updating.get() {
            return;
        }

        let target = self.upcast::<EventTarget>();
        target.fire_event(cx, Atom::from("updatestart"));

        if generation_id != self.generation_id.get() || self.is_removed() || !self.updating.get() {
            return;
        }

        let (remove_result, playback_pos) = {
            let mut tb = self.track_buffer.borrow_mut();
            let pos = tb.playback_position();
            (tb.remove_with_removed_ids(start, end), pos)
        };
        match remove_result {
            Ok(removed_segments) => {
                let removed_future_pushed = {
                    let mut pushed_ids = self.pushed_media_ids.borrow_mut();
                    let mut affected = false;
                    for (id, _seg_start, seg_end) in &removed_segments {
                        if pushed_ids.remove(id) && *seg_end > playback_pos {
                            affected = true;
                        }
                    }
                    affected
                };
                if self.track_buffer.borrow().media_segments().is_empty() {
                    self.last_pushed_init_id.set(None);
                    self.last_pushed_media_id.set(0);
                    self.last_pushed_end_pts.set(None);
                    self.pushed_media_ids.borrow_mut().clear();
                    if removed_future_pushed {
                        if let Some(player) = self.media_source.attached_player() {
                            let _ = player.lock().unwrap().mse_flush_track(self.source_buffer_id);
                        }
                    }
                } else if removed_future_pushed {
                    if let Some(player) = self.media_source.attached_player() {
                        let _ = self.flush_and_push_for_seek(&player, playback_pos);
                    } else {
                        self.last_pushed_end_pts.set(None);
                    }
                }
                self.media_source.notify_buffers_updated(cx);
                self.updating.set(false);
                target.fire_event(cx, Atom::from("update"));
                target.fire_event(cx, Atom::from("updateend"));
            },
            Err(err) => {
                warn!(
                    "SourceBuffer {} remove({start}, {end}) failed: {err}",
                    self.source_buffer_id
                );
                self.updating.set(false);
                target.fire_event(cx, Atom::from("error"));
                target.fire_event(cx, Atom::from("updateend"));
            },
        }
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> AppendMode {
        match self.track_buffer.borrow().append_mode() {
            MseAppendMode::Segments => AppendMode::Segments,
            MseAppendMode::Sequence => AppendMode::Sequence,
        }
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, value: AppendMode) -> ErrorResult {
        self.check_not_removed_or_updating()?;
        self.media_source.reopen_if_ended();
        let mode = match value {
            AppendMode::Segments => MseAppendMode::Segments,
            AppendMode::Sequence => MseAppendMode::Sequence,
        };
        self.track_buffer.borrow_mut().set_append_mode(mode);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        if self.is_removed() {
            return Err(Error::InvalidState(Some(
                "SourceBuffer has been removed from MediaSource".into(),
            )));
        }
        let mut container = TimeRangesContainer::default();
        for range in self.track_buffer.borrow().buffered() {
            let _ = container.add(range.start, range.end);
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), container))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.track_buffer.borrow().timestamp_offset())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, value: Finite<f64>) -> ErrorResult {
        self.check_not_removed_or_updating()?;
        self.media_source.reopen_if_ended();
        self.track_buffer
            .borrow_mut()
            .set_timestamp_offset(*value)
            .map_err(|e| Error::Type(cformat!("{e}")))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.track_buffer.borrow().append_window_start())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, value: Finite<f64>) -> ErrorResult {
        self.check_not_removed_or_updating()?;
        let start = *value;
        let end = self.track_buffer.borrow().append_window_end();
        if start < 0.0 || start >= end {
            return Err(Error::Type(cformat!(
                "Invalid appendWindowStart ({start}) for appendWindowEnd ({end})"
            )));
        }
        self.track_buffer
            .borrow_mut()
            .set_append_window(start, end)
            .map_err(|e| Error::Type(cformat!("{e}")))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.track_buffer.borrow().append_window_end()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, value: f64) -> ErrorResult {
        self.check_not_removed_or_updating()?;
        let start = self.track_buffer.borrow().append_window_start();
        if value.is_nan() || value <= start {
            return Err(Error::Type(cformat!(
                "Invalid appendWindowEnd ({value}) for appendWindowStart ({start})"
            )));
        }
        self.track_buffer
            .borrow_mut()
            .set_append_window(start, value)
            .map_err(|e| Error::Type(cformat!("{e}")))
    }

    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);
    event_handler!(update, GetOnupdate, SetOnupdate);
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);
    event_handler!(error, GetOnerror, SetOnerror);
    event_handler!(abort, GetOnabort, SetOnabort);

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        self.check_not_removed_or_updating()?;
        if self.media_source.ready_state() == MediaSourceReadyState::Closed {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is 'closed'".into(),
            )));
        }

        let bytes = get_buffer_source_copy((&data).into());
        if mse_debug_enabled() {
            eprintln!(
                "[mse-debug] SourceBuffer {} appendBuffer queued (bytes={})",
                self.source_buffer_id,
                bytes.len()
            );
        }
        self.media_source.reopen_if_ended();
        self.can_accept_append_bytes(bytes.len())?;

        self.updating.set(true);
        let generation_id = self.generation_id.get();
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(sourcebuffer_append: move |cx| {
                this.root().run_async_append(cx, bytes, generation_id);
            }));

        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> ErrorResult {
        if self.is_removed() {
            return Err(Error::InvalidState(Some(
                "SourceBuffer has been removed from MediaSource".into(),
            )));
        }
        if self.media_source.ready_state() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is not 'open'".into(),
            )));
        }
        self.abort_internal();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, type_: DOMString) -> ErrorResult {
        let type_str = type_.str();
        if type_str.is_empty() {
            return Err(Error::Type(cformat!("Empty MIME type in changeType")));
        }
        self.check_not_removed_or_updating()?;
        if !MediaSource::is_type_supported_str(&type_str) {
            return Err(Error::NotSupported(Some(format!(
                "Unsupported MIME type in changeType: {}",
                &*type_str
            ))));
        }

        self.media_source.reopen_if_ended();
        *self.mime_type.borrow_mut() = type_str.to_string();
        self.track_buffer.borrow_mut().change_type();
        self.last_pushed_init_id.set(None);

        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, start: Finite<f64>, end: f64) -> ErrorResult {
        self.check_not_removed_or_updating()?;

        let start = *start;
        let duration = self.media_source.current_duration();
        if start < 0.0 || (!duration.is_nan() && start > duration) {
            return Err(Error::Type(cformat!(
                "Invalid remove start ({start}) for duration ({duration})"
            )));
        }
        if end.is_nan() || end <= start {
            return Err(Error::Type(cformat!(
                "Invalid remove range [{start}, {end}]"
            )));
        }

        self.media_source.reopen_if_ended();
        self.updating.set(true);
        let generation_id = self.generation_id.get();
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(sourcebuffer_remove: move |cx| {
                this.root().run_async_remove(cx, start, end, generation_id);
            }));

        Ok(())
    }
}
