/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use mime::Mime;
use script_bindings::cell::DomRefCell;
use script_bindings::cformat;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::player::{MseEndOfStreamError, MseTrackDescriptor, Player};
use servo_media::{ServoMedia, SupportsMediaType};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmlmediaelement::{HTMLMediaElement, MediaElementWeakRef};
use crate::dom::mediasource::sourcebuffer::SourceBuffer;
use crate::dom::mediasource::sourcebufferlist::SourceBufferList;
use crate::dom::timeranges::TimeRangesContainer;
use crate::dom::window::Window;

const MAX_SOURCE_BUFFERS: usize = 16;

pub(crate) fn mse_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ROVELO_MSE_DEBUG").is_some())
}

#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    source_buffers: Dom<SourceBufferList>,
    active_source_buffers: Dom<SourceBufferList>,
    ready_state: Cell<ReadyState>,
    duration: Cell<f64>,
    live_seekable_range: Cell<Option<(f64, f64)>>,
    next_source_buffer_id: Cell<u32>,
    tracks_committed: Cell<bool>,
    media_element: DomRefCell<Option<MediaElementWeakRef>>,
}

impl MediaSource {
    pub(crate) fn new_inherited(
        source_buffers: &SourceBufferList,
        active_source_buffers: &SourceBufferList,
    ) -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            source_buffers: Dom::from_ref(source_buffers),
            active_source_buffers: Dom::from_ref(active_source_buffers),
            ready_state: Cell::new(ReadyState::Closed),
            duration: Cell::new(f64::NAN),
            live_seekable_range: Cell::new(None),
            next_source_buffer_id: Cell::new(1),
            tracks_committed: Cell::new(false),
            media_element: DomRefCell::new(None),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        let source_buffers = SourceBufferList::new(cx, global);
        let active_source_buffers = SourceBufferList::new(cx, global);
        reflect_dom_object_with_proto(
            cx,
            Box::new(Self::new_inherited(
                &source_buffers,
                &active_source_buffers,
            )),
            global,
            proto,
        )
    }

    pub(crate) fn ready_state(&self) -> ReadyState {
        self.ready_state.get()
    }

    pub(crate) fn current_duration(&self) -> f64 {
        self.duration.get()
    }

    pub(crate) fn set_duration_from_media(&self, duration: f64) {
        if duration.is_nan() || duration < 0.0 {
            return;
        }
        let duration = if duration >= 1e12 {
            f64::INFINITY
        } else {
            duration
        };
        self.duration.set(duration);
        if let Some(player) = self.attached_player() {
            let player_dur = if duration.is_finite() {
                Some(duration)
            } else {
                None
            };
            let _ = player.lock().unwrap().mse_set_duration(player_dur);
        }
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.media_element
            .borrow()
            .as_ref()
            .is_some_and(MediaElementWeakRef::is_alive)
    }

    pub(crate) fn attach_to_media_element(&self, element: &HTMLMediaElement) -> bool {
        if self.is_attached() {
            if mse_debug_enabled() {
                eprintln!("[mse-debug] MediaSource::attach_to_media_element rejected (already attached)");
            }
            return false;
        }
        *self.media_element.borrow_mut() = Some(MediaElementWeakRef::new(element));
        self.ready_state.set(ReadyState::Open);
        self.tracks_committed.set(false);
        if mse_debug_enabled() {
            eprintln!("[mse-debug] MediaSource::attach_to_media_element -> open, queuing sourceopen");
        }
        self.queue_event(Atom::from("sourceopen"));
        true
    }

    pub(crate) fn detach_from_media_element(&self) {
        *self.media_element.borrow_mut() = None;
        if self.ready_state.get() == ReadyState::Closed {
            return;
        }
        if mse_debug_enabled() {
            eprintln!("[mse-debug] MediaSource::detach_from_media_element -> closed");
        }
        self.ready_state.set(ReadyState::Closed);
        self.duration.set(f64::NAN);
        self.live_seekable_range.set(None);
        self.tracks_committed.set(false);

        for buffer in self.source_buffers.snapshot() {
            buffer.mark_removed();
        }
        self.active_source_buffers.clear_with_events();
        self.source_buffers.clear_with_events();
        self.queue_event(Atom::from("sourceclose"));
    }

    pub(crate) fn attached_media_element(&self) -> Option<DomRoot<HTMLMediaElement>> {
        self.media_element
            .borrow()
            .as_ref()
            .and_then(MediaElementWeakRef::root)
    }

    pub(crate) fn attached_player(&self) -> Option<Arc<Mutex<dyn Player>>> {
        self.attached_media_element()
            .and_then(|element| element.mse_player())
    }

    pub(crate) fn sync_to_player(&self, player: &Arc<Mutex<dyn Player>>) {
        let dur = self.duration.get();
        if !dur.is_nan() {
            let _ = player
                .lock()
                .unwrap()
                .mse_set_duration(if dur.is_finite() { Some(dur) } else { None });
        }
        if let Some((start, end)) = self.live_seekable_range.get() {
            let _ = player.lock().unwrap().mse_set_live_seekable_range(start, end);
        }
        for sb in self.source_buffers.snapshot() {
            sb.sync_to_player(player);
        }
        self.tracks_committed.set(false);
        self.maybe_commit_tracks(player);
        if self.ready_state.get() == ReadyState::Ended {
            let _ = player.lock().unwrap().mse_end_of_stream(None);
        }
    }

    pub(crate) fn reopen_if_ended(&self) {
        if self.ready_state.get() == ReadyState::Ended {
            self.ready_state.set(ReadyState::Open);
            self.queue_event(Atom::from("sourceopen"));
        }
    }

    pub(crate) fn maybe_commit_tracks(&self, player: &Arc<Mutex<dyn Player>>) {
        if self.tracks_committed.get() {
            return;
        }
        let buffers = self.source_buffers.snapshot();
        if buffers.is_empty() {
            return;
        }
        // Wait until every SourceBuffer has parsed an init segment so playbin
        // receives all A/V tracks together before firing no_more_pads().
        if !buffers.iter().all(|sb| sb.has_init_segment()) {
            if mse_debug_enabled() {
                let pending: Vec<u32> = buffers
                    .iter()
                    .filter(|sb| !sb.has_init_segment())
                    .map(|sb| sb.source_buffer_id())
                    .collect();
                eprintln!(
                    "[mse-debug] MediaSource::maybe_commit_tracks waiting for init on sb={pending:?}"
                );
            }
            return;
        }
        let descriptors: Vec<MseTrackDescriptor> = buffers
            .iter()
            .flat_map(|sb| sb.track_descriptors())
            .collect();
        if descriptors.is_empty() {
            return;
        }
        if mse_debug_enabled() {
            eprintln!(
                "[mse-debug] MediaSource::maybe_commit_tracks committing {} tracks: {:?}",
                descriptors.len(),
                descriptors
            );
        }
        if let Err(err) = player.lock().unwrap().mse_commit_tracks(&descriptors) {
            warn!("mse_commit_tracks failed: {err:?}");
        } else {
            self.tracks_committed.set(true);
        }
    }

    pub(crate) fn notify_buffers_updated(&self, cx: &mut JSContext) {
        if let Some(element) = self.attached_media_element() {
            element.on_mse_buffers_updated(cx);
        }
    }

    pub(crate) fn notify_encrypted_init_data(&self, init_data_type: String, init_data: Vec<u8>) {
        if let Some(element) = self.attached_media_element() {
            element.queue_encrypted_event(init_data_type, init_data);
        }
    }

    pub(crate) fn on_decode_error(&self, cx: &mut JSContext) {
        if self.ready_state.get() == ReadyState::Open {
            let _ = self.end_of_stream_internal(Some(EndOfStreamError::Decode), Some(cx));
        }
    }

    pub(crate) fn set_playback_position(&self, position: f64) {
        for sb in self.source_buffers.snapshot() {
            sb.set_playback_position(position);
        }
    }

    pub(crate) fn handle_mse_need_data(&self, source_buffer_id: u32) {
        let player = self.attached_player();
        for sb in self.source_buffers.snapshot() {
            if sb.source_buffer_id() == source_buffer_id {
                sb.set_player_needs_data(true, player.as_ref());
                break;
            }
        }
    }

    pub(crate) fn handle_mse_enough_data(&self, source_buffer_id: u32) {
        for sb in self.source_buffers.snapshot() {
            if sb.source_buffer_id() == source_buffer_id {
                sb.set_player_needs_data(false, None);
                break;
            }
        }
    }

    pub(crate) fn seek_keyframe_floor(&self, target: f64) -> Option<f64> {
        self.source_buffers
            .snapshot()
            .iter()
            .filter_map(|sb| sb.seek_keyframe_start(target))
            .reduce(f64::min)
    }

    pub(crate) fn handle_mse_seek(&self, player: &Arc<Mutex<dyn Player>>, target: f64) {
        self.set_playback_position(target);
        let mut pushed_any = false;
        let mut all_pushed = true;
        let mut all_pushed_to_end = true;
        for sb in self.source_buffers.snapshot() {
            if sb.flush_and_push_for_seek(player, target) {
                pushed_any = true;
                if !sb.pushed_to_end() {
                    all_pushed_to_end = false;
                }
            } else {
                all_pushed = false;
                all_pushed_to_end = false;
            }
        }
        if self.ready_state.get() == ReadyState::Ended &&
            pushed_any &&
            all_pushed &&
            all_pushed_to_end
        {
            let _ = player.lock().unwrap().mse_end_of_stream(None);
        }
    }

    /// W3C MSE Section 2.4.1 `HTMLMediaElement.buffered`: intersection across activeSourceBuffers.
    pub(crate) fn html_buffered(&self) -> TimeRangesContainer {
        let buffers = if !self.active_source_buffers.is_empty() {
            self.active_source_buffers.snapshot()
        } else {
            self.source_buffers.snapshot()
        };
        if buffers.is_empty() {
            return TimeRangesContainer::default();
        }

        let mut per_buffer: Vec<Vec<Range<f64>>> = buffers
            .iter()
            .map(|sb| sb.buffered_ranges())
            .collect();

        if per_buffer.iter().any(|r| r.is_empty()) {
            return TimeRangesContainer::default();
        }

        if self.ready_state.get() == ReadyState::Ended {
            let highest_end = per_buffer
                .iter()
                .filter_map(|ranges| ranges.last().map(|r| r.end))
                .fold(0.0f64, f64::max);
            for ranges in &mut per_buffer {
                if let Some(last) = ranges.last_mut() {
                    if highest_end > last.end {
                        last.end = highest_end;
                    }
                }
            }
        }

        let mut intersection = per_buffer[0].clone();
        for other in &per_buffer[1..] {
            intersection = intersect_ranges(&intersection, other);
            if intersection.is_empty() {
                return TimeRangesContainer::default();
            }
        }

        let mut container = TimeRangesContainer::default();
        for r in intersection {
            let _ = container.add(r.start, r.end);
        }
        container
    }

    /// W3C MSE `HTMLMediaElement.seekable`.
    pub(crate) fn html_seekable(&self) -> TimeRangesContainer {
        let mut container = TimeRangesContainer::default();
        let duration = self.duration.get();
        if duration.is_nan() {
            return container;
        }
        if duration.is_infinite() {
            if let Some((start, end)) = self.live_seekable_range.get() {
                let _ = container.add(start, end);
                return container;
            }
            let buffered = self.html_buffered();
            if !buffered.is_empty() {
                if let Ok(end) = buffered.end(buffered.len() - 1) {
                    let _ = container.add(0.0, end);
                }
            }
            return container;
        }
        let _ = container.add(0.0, duration);
        container
    }

    pub(crate) fn is_type_supported_str(type_str: &str) -> bool {
        let supported = Self::is_type_supported_inner(type_str);
        if mse_debug_enabled() {
            eprintln!("[mse-debug] MediaSource.isTypeSupported({type_str:?}) -> {supported}");
        }
        supported
    }

    fn is_type_supported_inner(type_str: &str) -> bool {
        let trimmed = type_str.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Ok(mime) = trimmed.parse::<Mime>() else {
            return false;
        };

        let container = format!(
            "{}/{}",
            mime.type_().as_str().to_ascii_lowercase(),
            mime.subtype().as_str().to_ascii_lowercase()
        );

        if !matches!(
            container.as_str(),
            "video/mp4"
                | "audio/mp4"
                | "video/webm"
                | "audio/webm"
                | "audio/mpeg"
                | "audio/aac"
        ) {
            return false;
        }

        let Some(codecs_param) = mime.get_param("codecs") else {
            return false;
        };
        let codecs: Vec<String> = codecs_param
            .as_str()
            .split(',')
            .map(|c| c.trim().to_ascii_lowercase())
            .collect();
        if codecs.is_empty() || codecs.iter().any(|c| c.is_empty()) {
            return false;
        }

        // Verify container/codec compatibility per W3C MSE Byte Stream Format Registry.
        // Reject AV1 (av01*) and HEVC (hev1*/hvc1*) because no hardware/dav1d/HEVC
        // MSE decoder pipeline is verified here, ensuring sites like YouTube pick H.264/VP9.
        let codecs_valid_for_container = codecs.iter().all(|codec| match container.as_str() {
            "video/mp4" => {
                codec.starts_with("avc1")
                    || codec.starts_with("avc3")
                    || codec.starts_with("mp4a")
                    || codec.starts_with("vp09")
                    || codec == "opus"
                    || codec == "flac"
                    || codec == "mp3"
            },
            "audio/mp4" => {
                codec.starts_with("mp4a") || codec == "opus" || codec == "flac" || codec == "mp3"
            },
            "video/webm" => {
                matches!(
                    codec.as_str(),
                    "vp8" | "x-vp8" | "vp8.0" | "vp9" | "x-vp9" | "vp9.0" | "opus" | "x-opus"
                        | "vorbis" | "x-vorbis"
                ) || codec.starts_with("vp08")
                    || codec.starts_with("vp09")
            },
            "audio/webm" => {
                matches!(codec.as_str(), "opus" | "x-opus" | "vorbis" | "x-vorbis")
            },
            "audio/mpeg" => codec == "mp3" || codec == "mpeg" || codec.starts_with("mp4a"),
            "audio/aac" => codec == "aac" || codec.starts_with("mp4a"),
            _ => false,
        });

        if !codecs_valid_for_container {
            return false;
        }

        let media = ServoMedia::get();
        if media.can_play_type(trimmed) == SupportsMediaType::Probably {
            return true;
        }

        // GStreamer's registry scanner registers "audio/mp3" under supported_codecs
        // rather than "mp3", and "mp4a*" rather than "aac". Fall back to container
        // support when those exact canonical audio-only codecs are requested.
        if container == "audio/mpeg" && codecs.iter().all(|c| c == "mp3") {
            return media.can_play_type("audio/mpeg") != SupportsMediaType::No
                || media.can_play_type("audio/mp3") != SupportsMediaType::No;
        }
        if container == "audio/aac" && codecs.iter().all(|c| c == "aac") {
            return media.can_play_type("audio/aac") != SupportsMediaType::No;
        }

        false
    }

    fn queue_event(&self, name: Atom) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mediasource_event: move |cx| {
                let this = this.root();
                this.upcast::<EventTarget>().fire_event(cx, name);
            }));
    }

    fn any_source_buffer_updating(&self) -> bool {
        self.source_buffers
            .snapshot()
            .iter()
            .any(|sb| sb.is_updating())
    }

    fn end_of_stream_internal(
        &self,
        error: Option<EndOfStreamError>,
        cx: Option<&mut JSContext>,
    ) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is not 'open'".into(),
            )));
        }
        if self.any_source_buffer_updating() {
            return Err(Error::InvalidState(Some(
                "A SourceBuffer is currently updating".into(),
            )));
        }

        self.ready_state.set(ReadyState::Ended);

        let player = self.attached_player();
        for sb in self.source_buffers.snapshot() {
            sb.flush_end_of_stream(player.as_ref());
        }

        if let Some(ref player) = player {
            self.maybe_commit_tracks(player);
        }

        if error.is_none() {
            let highest_end = self
                .source_buffers
                .snapshot()
                .iter()
                .map(|sb| sb.highest_end_timestamp())
                .fold(0.0f64, f64::max);
            if highest_end > 0.0 || self.duration.get().is_nan() {
                let new_duration = if highest_end > 0.0 {
                    highest_end
                } else {
                    0.0
                };
                self.set_duration_from_media(new_duration);
                if let Some(element) = self.attached_media_element() {
                    element.on_mse_duration_changed(new_duration);
                }
            }
        }

        let mapped_error = error.map(|e| match e {
            EndOfStreamError::Network => MseEndOfStreamError::Network,
            EndOfStreamError::Decode => MseEndOfStreamError::Decode,
        });

        if mse_debug_enabled() {
            eprintln!(
                "[mse-debug] MediaSource::endOfStream(error={mapped_error:?}, duration={})",
                self.duration.get()
            );
        }

        if let Some(ref player) = player {
            let _ = player.lock().unwrap().mse_end_of_stream(mapped_error);
        }

        if let Some(err) = mapped_error {
            if let Some(element) = self.attached_media_element() {
                if let Some(cx) = cx {
                    element.on_mse_end_of_stream_error(cx, err);
                }
            }
        } else if let Some(element) = self.attached_media_element() {
            if let Some(cx) = cx {
                element.on_mse_buffers_updated(cx);
            }
        }

        self.queue_event(Atom::from("sourceended"));
        Ok(())
    }
}

fn intersect_ranges(a: &[Range<f64>], b: &[Range<f64>]) -> Vec<Range<f64>> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let start = a[i].start.max(b[j].start);
        let end = a[i].end.min(b[j].end);
        if start < end {
            out.push(start..end);
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        MediaSource::new(cx, window.as_global_scope(), proto)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.active_source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> ReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        if self.ready_state.get() == ReadyState::Closed {
            return f64::NAN;
        }
        self.duration.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, value: f64) -> ErrorResult {
        if value.is_nan() || value < 0.0 {
            return Err(Error::Type(cformat!(
                "MediaSource duration must not be NaN or negative"
            )));
        }
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is not 'open'".into(),
            )));
        }
        if self.any_source_buffer_updating() {
            return Err(Error::InvalidState(Some(
                "A SourceBuffer is currently updating".into(),
            )));
        }
        self.set_duration_from_media(value);
        if mse_debug_enabled() {
            eprintln!("[mse-debug] MediaSource::SetDuration({value})");
        }
        if let Some(element) = self.attached_media_element() {
            element.on_mse_duration_changed(value);
        }
        Ok(())
    }

    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);

    /// <https://w3c.github.io/media-source/#dom-mediasource-canconstructindedicatedworker>
    fn CanConstructInDedicatedWorker(_window: &Window) -> bool {
        false
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(&self, cx: &mut JSContext, type_: DOMString) -> Fallible<DomRoot<SourceBuffer>> {
        let type_str = type_.str();
        if type_str.is_empty() {
            return Err(Error::Type(cformat!("Empty MIME type in addSourceBuffer")));
        }
        if !Self::is_type_supported_str(&type_str) {
            return Err(Error::NotSupported(Some(format!(
                "Unsupported MIME type in addSourceBuffer: {}",
                &*type_str
            ))));
        }
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is not 'open'".into(),
            )));
        }
        if self.source_buffers.len() >= MAX_SOURCE_BUFFERS {
            return Err(Error::QuotaExceeded {
                quota: Some(Finite::wrap(MAX_SOURCE_BUFFERS as f64)),
                requested: Some(Finite::wrap((self.source_buffers.len() + 1) as f64)),
            });
        }

        let id = self.next_source_buffer_id.get();
        self.next_source_buffer_id.set(id.wrapping_add(1));

        if mse_debug_enabled() {
            eprintln!(
                "[mse-debug] MediaSource::addSourceBuffer(id={id}, type={:?})",
                &*type_str
            );
        }

        let sb = SourceBuffer::new(cx, &self.global(), self, id, type_str.to_string());
        if let Some(player) = self.attached_player() {
            if let Err(err) = player.lock().unwrap().mse_add_source_buffer(id, &type_str) {
                warn!("mse_add_source_buffer failed: {err:?}");
            }
        }
        self.source_buffers.add(&sb);
        self.active_source_buffers.add(&sb);
        Ok(sb)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, source_buffer: &SourceBuffer) -> ErrorResult {
        if !self.source_buffers.contains(source_buffer) {
            return Err(Error::NotFound(Some(
                "SourceBuffer does not belong to this MediaSource".into(),
            )));
        }

        source_buffer.abort_internal();
        source_buffer.mark_removed();

        if let Some(player) = self.attached_player() {
            let _ = player
                .lock()
                .unwrap()
                .mse_remove_source_buffer(source_buffer.source_buffer_id());
        }

        let _ = self.active_source_buffers.remove(source_buffer);
        let _ = self.source_buffers.remove(source_buffer);
        if !self.tracks_committed.get() {
            if let Some(player) = self.attached_player() {
                self.maybe_commit_tracks(&player);
            }
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, cx: &mut JSContext, error: Option<EndOfStreamError>) -> ErrorResult {
        self.end_of_stream_internal(error, Some(cx))
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, start: Finite<f64>, end: Finite<f64>) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is not 'open'".into(),
            )));
        }
        let start = *start;
        let end = *end;
        if start < 0.0 || start > end {
            return Err(Error::Type(cformat!(
                "Invalid live seekable range [{start}, {end}]"
            )));
        }
        self.live_seekable_range.set(Some((start, end)));
        if let Some(player) = self.attached_player() {
            let _ = player.lock().unwrap().mse_set_live_seekable_range(start, end);
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(Some(
                "MediaSource readyState is not 'open'".into(),
            )));
        }
        self.live_seekable_range.set(None);
        if let Some(player) = self.attached_player() {
            let _ = player.lock().unwrap().mse_set_live_seekable_range(0.0, 0.0);
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        Self::is_type_supported_str(&type_.str())
    }
}
