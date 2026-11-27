/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time;

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::cenc::ClearKeyStore;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{
    MseEndOfStreamError, MseTrackDescriptor, PlaybackState, Player, PlayerError, PlayerEvent,
    SeekLock, SeekLockMsg, StreamType,
};
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};

use super::BACKEND_BASE_TIME;
use crate::media_source_src::{ServoMediaSourceSrc, register_servo_media_source_src};
use crate::media_stream::GStreamerMediaStream;
use crate::media_stream_source::{ServoMediaStreamSrc, register_servo_media_stream_src};
use crate::render::GStreamerRender;
use crate::source::{ServoSrc, register_servo_src};

const DEFAULT_MUTED: bool = false;
const DEFAULT_PAUSED: bool = true;
const DEFAULT_CAN_RESUME: bool = false;
const DEFAULT_PLAYBACK_RATE: f64 = 1.0;
const DEFAULT_VOLUME: f64 = 1.0;
const DEFAULT_TIME_RANGES: Vec<Range<f64>> = vec![];

const MAX_BUFFER_SIZE: i32 = 500 * 1024 * 1024;

fn sanitize_gst_duration(dur: Option<gstreamer::ClockTime>) -> Option<time::Duration> {
    let dur = dur?;
    // Demuxers on unbounded/live streams can emit i64::MAX ns (~9.22e12 s) instead of
    // GST_CLOCK_TIME_NONE; treat anything >= 1e12 s as unknown/live.
    if dur.nseconds() >= (i64::MAX as u64) || dur.seconds() >= 1_000_000_000_000 {
        return None;
    }
    let nanos = (dur.nseconds() % 1_000_000_000) as u32;
    Some(time::Duration::new(dur.seconds(), nanos))
}

/// Schedules `gst_player.play()` off `GstPlay`'s `GMainContext` thread so sync-emitted
/// `PlaySignalAdapter` handlers (such as `seek-done` or `state-changed`) do not call
/// `g_main_context_invoke_full` synchronously and self-deadlock on `GstPlay`'s mutex.
fn resume_gst_player_async(
    weak_player: glib::WeakRef<gstreamer_play::Play>,
    stopping: Arc<AtomicBool>,
) {
    let _ = std::thread::Builder::new()
        .name("GstPlayResume".to_owned())
        .spawn(move || {
            if !stopping.load(Ordering::Acquire) &&
                let Some(gst_player) = weak_player.upgrade() &&
                !stopping.load(Ordering::Acquire)
            {
                gst_player.play();
            }
        });
}

fn metadata_from_media_info(media_info: &gstreamer_play::PlayMediaInfo) -> Result<Metadata, ()> {
    let duration = sanitize_gst_duration(media_info.duration());

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();

    let format = media_info
        .container_format()
        .unwrap_or_else(|| glib::GString::from(""))
        .to_string();

    for stream_info in media_info.stream_list() {
        let stream_type = stream_info.stream_type();
        match stream_type.as_str() {
            "audio" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                audio_tracks.push(codec);
            },
            "video" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                video_tracks.push(codec);
            },
            _ => {},
        }
    }

    let mut width: u32 = 0;
    let height: u32 = if media_info.number_of_video_streams() > 0 {
        let first_video_stream = &media_info.video_streams()[0];
        width = first_video_stream.width() as u32;
        first_video_stream.height() as u32
    } else {
        0
    };

    let is_seekable = media_info.is_seekable();
    let is_live = media_info.is_live();
    let title = media_info.title().map(|s| s.as_str().to_string());

    Ok(Metadata {
        duration,
        width,
        height,
        format,
        is_seekable,
        audio_tracks,
        video_tracks,
        is_live,
        title,
    })
}

pub struct GStreamerAudioChunk(gstreamer::buffer::MappedBuffer<gstreamer::buffer::Readable>);
impl AsRef<[f32]> for GStreamerAudioChunk {
    fn as_ref(&self) -> &[f32] {
        self.0.as_ref().as_slice_of::<f32>().unwrap_or_default()
    }
}

#[derive(PartialEq)]
enum PlayerSource {
    Seekable(ServoSrc),
    Stream(ServoMediaStreamSrc),
    MediaSource(ServoMediaSourceSrc),
}

struct PlayerInner {
    // Drop PlaySignalAdapter before Play so GstPlaySignalAdapter never tears down
    // against an already-disposed GstPlay instance.
    signal_adapter: Option<gstreamer_play::PlaySignalAdapter>,
    player: Option<gstreamer_play::Play>,
    source: Option<PlayerSource>,
    video_sink: gstreamer_app::AppSink,
    audio_sink: Option<gstreamer_app::AppSink>,
    signal_ids: Vec<glib::SignalHandlerId>,
    pipeline_source_setup_id: Option<glib::SignalHandlerId>,
    stopping: Arc<AtomicBool>,
    input_size: u64,
    seekable: bool,
    seeking: bool,
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    cat: gstreamer::DebugCategory,
    enough_data: Arc<AtomicBool>,
}

impl PlayerInner {
    fn gst_player(&self) -> Result<&gstreamer_play::Play, PlayerError> {
        self.player
            .as_ref()
            .ok_or_else(|| PlayerError::Backend("Player is stopped".into()))
    }

    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Set input_size to proxy its value, since it
        // could be set by the user before calling .setup().
        self.input_size = size;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_size(if size > 0 {
                size as i64
            } else {
                -1 // live source
            });
        }
        Ok(())
    }

    pub fn set_seekable(&mut self, seekable: bool) -> Result<(), PlayerError> {
        self.seekable = seekable;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_seekable(seekable);
        }
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<(), PlayerError> {
        self.muted.set(muted);
        self.gst_player()?.set_mute(muted);
        Ok(())
    }

    pub fn muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_playback_rate(&mut self, playback_rate: f64) -> Result<(), PlayerError> {
        if !matches!(
            self.stream_type,
            StreamType::Seekable | StreamType::MediaSource
        ) {
            return Err(PlayerError::NonSeekableStream);
        }

        if self.playback_rate.get() == playback_rate {
            return Ok(());
        }

        self.playback_rate.set(playback_rate);

        // The new playback rate will not be passed to the pipeline if the
        // current GstPlay state is less than GST_STATE_PAUSED, which will be
        // set immediately before the initial gstreamer_play_MESSAGE_MEDIA_INFO_UPDATED
        // message is posted to bus.
        if self.last_metadata.is_some() {
            self.gst_player()?.set_rate(playback_rate);
        }
        Ok(())
    }

    pub fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    pub fn play(&mut self) -> Result<(), PlayerError> {
        if !self.paused.get() {
            if !self.seeking && self.play_state != gstreamer_play::PlayState::Playing {
                self.gst_player()?.play();
            }
            return Ok(());
        }

        self.paused.set(false);
        self.can_resume.set(false);
        self.gst_player()?.play();
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.paused.get() {
            return Ok(());
        }

        self.paused.set(true);
        self.can_resume.set(true);
        self.gst_player()?.pause();
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.paused.get()
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume.get()
    }

    pub fn end_of_stream(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(ref mut source) => match source {
                PlayerSource::Seekable(source) => source
                    .push_end_of_stream()
                    .map(|_| ())
                    .map_err(|_| PlayerError::EOSFailed),
                PlayerSource::MediaSource(source) => source.end_of_stream(None),
                PlayerSource::Stream(_) => Ok(()),
            },
            _ => Ok(()),
        }
    }

    pub fn seek(&mut self, time: f64) -> Result<(), PlayerError> {
        if !matches!(
            self.stream_type,
            StreamType::Seekable | StreamType::MediaSource
        ) {
            return Err(PlayerError::NonSeekableStream);
        }
        if !time.is_finite() || time < 0.0 {
            return Err(PlayerError::SeekOutOfRange);
        }

        if let Some(PlayerSource::MediaSource(ref source)) = self.source {
            if !source.is_seek_in_range(time) {
                if let Some(ref player) = self.player {
                    gstreamer::warning!(self.cat, obj = player, "Trying to seek out of range");
                }
                return Err(PlayerError::SeekOutOfRange);
            }
            if !source.pads_finalized() {
                source.notify_seek(time);
                return Ok(());
            }
        } else if let Some(ref metadata) = self.last_metadata &&
            let Some(ref duration) = metadata.duration &&
            duration < &time::Duration::from_secs_f64(time)
        {
            if let Some(ref player) = self.player {
                gstreamer::warning!(self.cat, obj = player, "Trying to seek out of range");
            }
            return Err(PlayerError::SeekOutOfRange);
        }

        let seek_ct = gstreamer::ClockTime::try_from_seconds_f64(time)
            .map_err(|_| PlayerError::SeekOutOfRange)?;
        self.seeking = true;
        self.gst_player()?.seek(seek_ct);
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        self.volume.set(volume);
        self.gst_player()?.set_volume(volume);
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed) {
                return Err(PlayerError::EnoughData);
            }
            return source
                .push_buffer(data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        Err(PlayerError::BufferPushFailed)
    }

    pub fn set_src(&mut self, source: PlayerSource) {
        self.source = Some(source);
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        let mut buffered_ranges = vec![];
        let Some(ref player) = self.player else {
            return buffered_ranges;
        };
        let pipeline = player.pipeline();

        if self.stream_type == StreamType::MediaSource {
            let mut buffering_time = gstreamer::query::Buffering::new(gstreamer::Format::Time);
            if pipeline.query(&mut buffering_time) {
                for (start, end) in buffering_time.ranges() {
                    if let (
                        gstreamer::GenericFormattedValue::Time(Some(start)),
                        gstreamer::GenericFormattedValue::Time(Some(end)),
                    ) = (start, end)
                    {
                        buffered_ranges.push(Range {
                            start: start.seconds_f64(),
                            end: end.seconds_f64(),
                        });
                    }
                }
                if !buffered_ranges.is_empty() {
                    return buffered_ranges;
                }
            }
        }

        let duration = self
            .last_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
            .or_else(|| {
                if let Some(PlayerSource::MediaSource(ref source)) = self.source {
                    source
                        .duration()
                        .and_then(|d| time::Duration::try_from_secs_f64(d).ok())
                } else {
                    None
                }
            });

        let Some(duration) = duration else {
            return buffered_ranges;
        };

        let mut buffering = gstreamer::query::Buffering::new(gstreamer::Format::Percent);
        if pipeline.query(&mut buffering) {
            let ranges = buffering.ranges();
            for (start, end) in ranges {
                let start = (if let gstreamer::GenericFormattedValue::Percent(start) = start {
                    start.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                let end = (if let gstreamer::GenericFormattedValue::Percent(end) = end {
                    end.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                buffered_ranges.push(Range { start, end });
            }
        }

        buffered_ranges
    }

    pub fn seekable(&self) -> Vec<Range<f64>> {
        if let Some(PlayerSource::MediaSource(ref source)) = self.source {
            if let Some((start, end)) = source.live_seekable_range() {
                return vec![Range { start, end }];
            }
            if let Some(duration) = source.duration() {
                return vec![Range {
                    start: 0.0,
                    end: duration,
                }];
            }
            return self.buffered();
        }

        // if the servosrc is seekable, we should return the duration of the media
        if let Some(metadata) = self.last_metadata.as_ref() &&
            metadata.is_seekable &&
            let Some(duration) = metadata.duration
        {
            return vec![Range {
                start: 0.0,
                end: duration.as_secs_f64(),
            }];
        }

        // if the servosrc is not seekable, we should return the buffered range
        self.buffered()
    }

    fn set_stream(&mut self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError> {
        debug_assert!(self.stream_type == StreamType::Stream);
        let Some(PlayerSource::Stream(ref source)) = self.source else {
            return Err(PlayerError::SetStreamFailed);
        };

        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let Some(stream) = stream.as_mut_any().downcast_mut::<GStreamerMediaStream>() else {
            return Err(PlayerError::SetStreamFailed);
        };

        let playbin = self
            .gst_player()?
            .pipeline()
            .dynamic_cast::<gstreamer::Pipeline>()
            .unwrap();
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        let player = self.gst_player()?;
        player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        let player = self.gst_player()?;
        player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        player.set_video_track_enabled(enabled);
        Ok(())
    }

    fn mse_source(&self) -> Result<&ServoMediaSourceSrc, PlayerError> {
        match self.source {
            Some(PlayerSource::MediaSource(ref source)) => Ok(source),
            _ => Err(PlayerError::Backend(
                "Player is not initialized with StreamType::MediaSource".into(),
            )),
        }
    }

    fn mse_add_source_buffer(&mut self, id: u32, mime_type: &str) -> Result<(), PlayerError> {
        self.mse_source()?.add_source_buffer(id, mime_type)
    }

    fn mse_remove_source_buffer(&mut self, id: u32) -> Result<(), PlayerError> {
        self.mse_source()?.remove_source_buffer(id)
    }

    fn mse_push_data(&mut self, source_buffer_id: u32, data: Vec<u8>) -> Result<(), PlayerError> {
        self.mse_source()?.push_data(source_buffer_id, data)
    }

    fn mse_flush_track(&mut self, source_buffer_id: u32) -> Result<(), PlayerError> {
        self.mse_source()?.flush_track(source_buffer_id)
    }

    fn mse_end_of_stream(&mut self, error: Option<MseEndOfStreamError>) -> Result<(), PlayerError> {
        self.mse_source()?.end_of_stream(error)
    }

    fn mse_set_duration(&mut self, duration: Option<f64>) -> Result<(), PlayerError> {
        self.mse_source()?.set_duration(duration)
    }

    fn mse_set_live_seekable_range(&mut self, start: f64, end: f64) -> Result<(), PlayerError> {
        self.mse_source()?.set_live_seekable_range(start, end)
    }

    fn mse_commit_tracks(&mut self, tracks: &[MseTrackDescriptor]) -> Result<(), PlayerError> {
        self.mse_source()?.commit_tracks(tracks)
    }

    fn set_clearkey_store(&mut self, store: Option<ClearKeyStore>) -> Result<(), PlayerError> {
        match self.source {
            Some(PlayerSource::MediaSource(ref source)) => source.set_clearkey_store(store)?,
            Some(PlayerSource::Seekable(ref source)) => {
                source
                    .set_clearkey_store(store)
                    .map_err(|_| PlayerError::BufferPushFailed)?;
            },
            _ => {},
        }
        Ok(())
    }

    fn notify_keys_updated(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(PlayerSource::MediaSource(ref source)) => source.notify_keys_updated()?,
            Some(PlayerSource::Seekable(ref source)) => {
                source
                    .notify_keys_updated()
                    .map_err(|_| PlayerError::BufferPushFailed)?;
            },
            _ => {},
        }
        Ok(())
    }
}

macro_rules! notify(
    ($observer:expr_2021, $event:expr_2021) => {
        $observer.lock().unwrap().send($event)
    };
);

struct SeekChannel {
    sender: SeekLock,
    recv: GenericReceiver<SeekLockMsg>,
}

impl SeekChannel {
    fn new() -> Self {
        let (sender, recv) =
            generic_channel::channel::<SeekLockMsg>().expect("Couldn't create IPC channel");
        Self {
            sender: SeekLock {
                lock_channel: sender,
            },
            recv,
        }
    }

    fn sender(&self) -> SeekLock {
        self.sender.clone()
    }

    fn _await(&self) -> SeekLockMsg {
        self.recv.recv().unwrap()
    }
}

pub struct GStreamerPlayer {
    /// The player unique ID.
    id: usize,
    /// The ID of the client context this player belongs to.
    context_id: ClientContextId,
    /// Channel to communicate with the owner GStreamerBackend instance.
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
    inner: RefCell<Option<Arc<Mutex<PlayerInner>>>>,
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    /// Indicates whether the setup was succesfully performed and
    /// we are ready to consume a/v data.
    is_ready: Arc<Once>,
    /// Indicates whether the type of media stream to be played is a live stream.
    stream_type: StreamType,
    /// Decorator used to setup the video sink and process the produced frames.
    render: Arc<Mutex<GStreamerRender>>,
    /// Whether to enable download buffering. This is only enabled heuristically for some media,
    /// because it causes issues in certain situations. See <https://github.com/servo/servo/issues/23241>.
    download_buffering_enabled: AtomicBool,
    /// Whether `stop()` has already torn down this player.
    stopped: AtomicBool,
    /// Optional EME ClearKey store applied to MSE media sources.
    clearkey_store: Arc<Mutex<Option<ClearKeyStore>>>,
}

impl GStreamerPlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: usize,
        context_id: &ClientContextId,
        backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
        stream_type: StreamType,
        observer: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
    ) -> GStreamerPlayer {
        let _ = gstreamer::DebugCategory::new(
            "servoplayer",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo player"),
        );

        Self {
            id,
            context_id: *context_id,
            backend_chan,
            inner: RefCell::new(None),
            observer: Arc::new(Mutex::new(observer)),
            audio_renderer,
            video_renderer,
            is_ready: Arc::new(Once::new()),
            stream_type,
            render: Arc::new(Mutex::new(GStreamerRender::new(gl_context))),
            download_buffering_enabled: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            clearkey_store: Arc::new(Mutex::new(None)),
        }
    }

    fn setup(&self) -> Result<(), PlayerError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(PlayerError::Backend("Player is stopped".into()));
        }
        if self.inner.borrow().is_some() {
            return Ok(());
        }

        // Check that we actually have the elements that we
        // need to make this work.
        for element in ["playbin3", "decodebin3", "queue"] {
            if gstreamer::ElementFactory::find(element).is_none() {
                return Err(PlayerError::Backend(format!(
                    "Missing dependency: {}",
                    element
                )));
            }
        }

        let player = gstreamer_play::Play::default();
        let signal_adapter = gstreamer_play::PlaySignalAdapter::new_sync_emit(&player);
        let pipeline = player.pipeline();

        // FIXME(#282): The progressive downloading breaks playback on Windows and Android.
        if !cfg!(any(target_os = "windows", target_os = "android")) &&
            self.download_buffering_enabled.load(Ordering::Relaxed)
        {
            // Set player to perform progressive downloading. This will make the
            // player store the downloaded media in a local temporary file for
            // faster playback of already-downloaded chunks.
            let flags = pipeline.property_value("flags");
            let flags_class = match glib::FlagsClass::with_type(flags.type_()) {
                Some(flags) => flags,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let flags_class = match flags_class.builder_with_value(flags) {
                Some(class) => class,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let Some(flags) = flags_class.set_by_nick("download").build() else {
                return Err(PlayerError::Backend(
                    "FlagsClass creation failed".to_owned(),
                ));
            };
            pipeline.set_property_from_value("flags", &flags);
        }

        // Set max size for the player buffer.
        pipeline.set_property("buffer-size", MAX_BUFFER_SIZE);

        // Set player position interval update to 0.25 seconds.
        let mut config = player.config();
        config.set_position_update_interval(250u32);
        player
            .set_config(config)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        let stopping = Arc::new(AtomicBool::new(false));
        let mut audio_sink_opt = None;

        if let Some(ref audio_renderer) = self.audio_renderer {
            let audio_sink =
                gstreamer::ElementFactory::make("appsink")
                    .build()
                    .map_err(|error| {
                        PlayerError::Backend(format!("appsink creation failed: {error:?}"))
                    })?;

            pipeline.set_property("audio-sink", &audio_sink);

            let audio_sink = audio_sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();

            let weak_audio_renderer = Arc::downgrade(audio_renderer);
            let stopping_audio = stopping.clone();

            audio_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll(|_| Ok(gstreamer::FlowSuccess::Ok))
                    .new_sample(move |audio_sink| {
                        if stopping_audio.load(Ordering::Acquire) {
                            return Err(gstreamer::FlowError::Flushing);
                        }
                        let sample = audio_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;
                        let audio_info = sample
                            .caps()
                            .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                            .ok_or(gstreamer::FlowError::Error)?;
                        let positions =
                            audio_info.positions().ok_or(gstreamer::FlowError::Error)?;

                        let Some(audio_renderer) = weak_audio_renderer.upgrade() else {
                            return Err(gstreamer::FlowError::Flushing);
                        };

                        for position in positions.iter() {
                            if stopping_audio.load(Ordering::Acquire) {
                                return Err(gstreamer::FlowError::Flushing);
                            }
                            let buffer = buffer.clone();
                            let map = match buffer.into_mapped_buffer_readable() {
                                Ok(map) => map,
                                _ => {
                                    return Err(gstreamer::FlowError::Error);
                                },
                            };
                            let chunk = Box::new(GStreamerAudioChunk(map));
                            let channel = position.to_mask() as u32;

                            audio_renderer.lock().unwrap().render(chunk, channel);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
            audio_sink_opt = Some(audio_sink);
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        let uri = match self.stream_type {
            StreamType::Stream => {
                register_servo_media_stream_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediastreamsrc registration error: {error:?}"
                    ))
                })?;
                "mediastream://".to_value()
            },
            StreamType::Seekable => {
                register_servo_src().map_err(|error| {
                    PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                })?;
                "servosrc://".to_value()
            },
            StreamType::MediaSource => {
                register_servo_media_source_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediasourcesrc registration error: {error:?}"
                    ))
                })?;
                "mediasource://".to_value()
            },
        };
        player.set_property("uri", &uri);
        // Explicitly assert default mute/volume on GstPlay so PipeWire/WirePlumber
        // stream-restore never leaves a fresh unmuted player muted or attenuated.
        player.set_mute(DEFAULT_MUTED);
        player.set_volume(DEFAULT_VOLUME);

        // No video_renderers no video
        if self.video_renderer.is_none() {
            player.set_video_track_enabled(false);
        }

        *self.inner.borrow_mut() = Some(Arc::new(Mutex::new(PlayerInner {
            signal_adapter: Some(signal_adapter.clone()),
            player: Some(player),
            source: None,
            video_sink,
            audio_sink: audio_sink_opt,
            signal_ids: Vec::new(),
            pipeline_source_setup_id: None,
            stopping: stopping.clone(),
            input_size: 0,
            seekable: true,
            seeking: false,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
        })));

        let inner = self.inner.borrow();
        let inner = inner.as_ref().unwrap();
        let mut signal_ids = Vec::with_capacity(7);

        let observer = self.observer.clone();
        let stopping_eos = stopping.clone();
        // Handle `end-of-stream` signal.
        signal_ids.push(signal_adapter.connect_end_of_stream(move |_| {
            if !stopping_eos.load(Ordering::Acquire) {
                let _ = notify!(observer, PlayerEvent::EndOfStream);
            }
        }));

        let observer = self.observer.clone();
        let stopping_err = stopping.clone();
        // Handle `error` signal
        signal_ids.push(signal_adapter.connect_error(move |_self, error, _details| {
            if !stopping_err.load(Ordering::Acquire) {
                let _ = notify!(observer, PlayerEvent::Error(error.to_string()));
            }
        }));

        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        // Handle `state-changed` signal.
        signal_ids.push(signal_adapter.connect_state_changed(move |_, play_state| {
            let Some(inner_arc) = weak_inner.upgrade() else {
                return;
            };
            let (resume_player, emit_state) = {
                let mut inner = inner_arc.lock().unwrap();
                if inner.stopping.load(Ordering::Acquire) {
                    return;
                }
                inner.play_state = play_state;

                // If `play()` was called before pads finalized / prerolled, ensure GstPlay
                // does not stay stuck in Paused once preroll completes. Do not interrupt an
                // in-flight seek before `SeekDone` fires.
                let resume = if play_state == gstreamer_play::PlayState::Paused &&
                    !inner.paused.get() &&
                    !inner.seeking
                {
                    inner.player.as_ref().map(|p| (p.downgrade(), inner.stopping.clone()))
                } else {
                    None
                };

                let state = match play_state {
                    gstreamer_play::PlayState::Buffering => Some(PlaybackState::Buffering),
                    gstreamer_play::PlayState::Stopped => Some(PlaybackState::Stopped),
                    gstreamer_play::PlayState::Paused => Some(PlaybackState::Paused),
                    gstreamer_play::PlayState::Playing => Some(PlaybackState::Playing),
                    _ => None,
                };
                (resume, state)
            };

            if let Some((weak_player, stopping_resume)) = resume_player {
                resume_gst_player_async(weak_player, stopping_resume);
            }
            if let Some(v) = emit_state {
                let _ = notify!(observer, PlayerEvent::StateChanged(v));
            }
        }));

        let observer = self.observer.clone();
        let stopping_pos = stopping.clone();
        // Handle `position-update` signal.
        signal_ids.push(signal_adapter.connect_position_updated(move |_, position| {
            if !stopping_pos.load(Ordering::Acquire) &&
                let Some(seconds) = position.map(|p| p.seconds_f64())
            {
                let _ = notify!(observer, PlayerEvent::PositionChanged(seconds));
            }
        }));

        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        let stopping_seek = stopping.clone();
        // Handle `seek-done` signal.
        signal_ids.push(signal_adapter.connect_seek_done(move |_, position| {
            let resume_player = weak_inner.upgrade().and_then(|inner_arc| {
                let mut inner = inner_arc.lock().unwrap();
                if inner.stopping.load(Ordering::Acquire) {
                    return None;
                }
                inner.seeking = false;
                if !inner.paused.get() && inner.play_state != gstreamer_play::PlayState::Playing {
                    inner.player.as_ref().map(|p| (p.downgrade(), inner.stopping.clone()))
                } else {
                    None
                }
            });
            if !stopping_seek.load(Ordering::Acquire) {
                let _ = notify!(observer, PlayerEvent::SeekDone(position.seconds_f64()));
            }
            if let Some((weak_player, stopping_resume)) = resume_player {
                resume_gst_player_async(weak_player, stopping_resume);
            }
        }));

        // Handle `media-info-updated` signal.
        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        signal_ids.push(signal_adapter.connect_media_info_updated(move |_, info| {
            let Ok(mut metadata) = metadata_from_media_info(info) else {
                return;
            };
            let Some(inner_arc) = weak_inner.upgrade() else {
                return;
            };

            let (resume_player, metadata_to_emit, send_pause_event) = {
                let mut inner_guard = inner_arc.lock().unwrap();
                let inner = &mut *inner_guard;
                if inner.stopping.load(Ordering::Acquire) {
                    return;
                }

                if inner.stream_type == StreamType::MediaSource &&
                    let Some(PlayerSource::MediaSource(ref src)) = inner.source &&
                    src.has_explicit_duration()
                {
                    let mse_dur = src
                        .duration()
                        .and_then(|d| time::Duration::try_from_secs_f64(d).ok());
                    metadata.duration = mse_dur;
                    metadata.is_live = mse_dur.is_none();
                }

                if inner.last_metadata.as_ref() == Some(&metadata) {
                    return;
                }

                // TODO: Workaround to generate expected `paused` state change event.
                // <https://github.com/servo/servo/issues/40740>
                let mut send_pause_event = false;

                if inner.last_metadata.is_none() {
                    if metadata.is_seekable && inner.playback_rate.get() != DEFAULT_PLAYBACK_RATE
                    {
                        // The `paused` state change event will be fired after the
                        // seek initiated by the playback rate change has
                        // completed.
                        if let Some(ref player) = inner.player {
                            player.set_rate(inner.playback_rate.get());
                        }
                    } else if inner.play_state == gstreamer_play::PlayState::Paused &&
                        inner.paused.get()
                    {
                        // We need to send the paused event for non seekable push streams.
                        send_pause_event = true;
                    }
                }

                let resume = if !inner.paused.get() &&
                    !inner.seeking &&
                    inner.play_state != gstreamer_play::PlayState::Playing
                {
                    inner.player.as_ref().map(|p| (p.downgrade(), inner.stopping.clone()))
                } else {
                    None
                };

                inner.last_metadata = Some(metadata.clone());
                if let Some(ref player) = inner.player {
                    gstreamer::info!(
                        inner.cat,
                        obj = player,
                        "Metadata updated: {:?}",
                        metadata
                    );
                }
                (resume, metadata, send_pause_event)
            };

            if let Some((weak_player, stopping_resume)) = resume_player {
                resume_gst_player_async(weak_player, stopping_resume);
            }
            let _ = notify!(observer, PlayerEvent::MetadataUpdated(metadata_to_emit));

            if send_pause_event {
                let _ = notify!(observer, PlayerEvent::StateChanged(PlaybackState::Paused));
            }
        }));

        // Handle `duration-changed` signal.
        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        signal_ids.push(signal_adapter.connect_duration_changed(move |_, duration| {
            let Some(inner_arc) = weak_inner.upgrade() else {
                return;
            };
            let mut duration = sanitize_gst_duration(duration);

            let duration_to_emit = {
                let mut inner_guard = inner_arc.lock().unwrap();
                let inner = &mut *inner_guard;
                if inner.stopping.load(Ordering::Acquire) {
                    return;
                }
                if inner.stream_type == StreamType::MediaSource &&
                    let Some(PlayerSource::MediaSource(ref src)) = inner.source &&
                    src.has_explicit_duration()
                {
                    duration = src
                        .duration()
                        .and_then(|d| time::Duration::try_from_secs_f64(d).ok());
                }

                let is_mse = inner.stream_type == StreamType::MediaSource;
                if let Some(ref mut metadata) = inner.last_metadata &&
                    metadata.duration != duration
                {
                    metadata.duration = duration;
                    metadata.is_live = is_mse && duration.is_none();
                    if let Some(ref player) = inner.player {
                        gstreamer::info!(
                            inner.cat,
                            obj = player,
                            "Duration changed: {:?}",
                            duration
                        );
                    }
                    Some(duration)
                } else {
                    None
                }
            };

            if let Some(duration) = duration_to_emit {
                let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
            }
        }));

        inner.lock().unwrap().signal_ids = signal_ids;

        if let Some(video_renderer) = self.video_renderer.clone() {
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);
                let stopping_video = inner.lock().unwrap().stopping.clone();

                move |sample: gstreamer::Sample| {
                    if stopping_video.load(Ordering::Acquire) {
                        return Err(gstreamer::FlowError::Flushing);
                    }
                    let Some(frame) = render.lock().unwrap().get_frame_from_sample(sample) else {
                        return Err(gstreamer::FlowError::Error);
                    };

                    match weak_video_renderer.upgrade() {
                        Some(video_renderer) => {
                            if stopping_video.load(Ordering::Acquire) {
                                return Err(gstreamer::FlowError::Flushing);
                            }
                            video_renderer.lock().unwrap().render(frame);
                        },
                        _ => {
                            return Err(gstreamer::FlowError::Flushing);
                        },
                    };

                    if !stopping_video.load(Ordering::Acquire) {
                        let _ = notify!(observer, PlayerEvent::VideoFrameUpdated);
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                }
            };

            // Set video_sink callbacks.
            let stopping_preroll = inner.lock().unwrap().stopping.clone();
            let stopping_sample = stopping_preroll.clone();
            inner.lock().unwrap().video_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll({
                        let render_sample = render_sample.clone();
                        move |video_sink| {
                            if stopping_preroll.load(Ordering::Acquire) {
                                return Err(gstreamer::FlowError::Flushing);
                            }
                            render_sample(
                                video_sink
                                    .pull_preroll()
                                    .map_err(|_| gstreamer::FlowError::Eos)?,
                            )
                        }
                    })
                    .new_sample(move |video_sink| {
                        if stopping_sample.load(Ordering::Acquire) {
                            return Err(gstreamer::FlowError::Flushing);
                        }
                        render_sample(
                            video_sink
                                .pull_sample()
                                .map_err(|_| gstreamer::FlowError::Eos)?,
                        )
                    })
                    .build(),
            );
        };

        let (receiver, error_handler_id) = {
            let weak_inner = Arc::downgrade(inner);
            let mut inner = inner.lock().unwrap();
            let gst_player = inner
                .player
                .as_ref()
                .ok_or_else(|| PlayerError::Backend("Player is stopped".into()))?
                .clone();
            let pipeline = gst_player.pipeline();

            let (sender, receiver) = mpsc::channel();

            let sender = Arc::new(Mutex::new(sender));
            let sender_clone = sender.clone();
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            let clearkey_store = self.clearkey_store.clone();
            let pipeline_source_setup_id = pipeline.connect("source-setup", false, move |args| {
                let Some(inner_arc) = weak_inner.upgrade() else {
                    return None;
                };
                let source = args[1].get::<gstreamer::Element>().unwrap();

                let mut inner = inner_arc.lock().unwrap();
                if inner.stopping.load(Ordering::Acquire) {
                    return None;
                }
                let source = match inner.stream_type {
                    StreamType::Seekable => {
                        let servosrc = source
                            .dynamic_cast::<ServoSrc>()
                            .expect("Source element is expected to be a ServoSrc!");

                        servosrc.set_observer(observer.clone());
                        let pending_store = clearkey_store.lock().unwrap().clone();
                        if pending_store.is_some() {
                            let _ = servosrc.set_clearkey_store(pending_store);
                        }

                        if inner.input_size > 0 {
                            servosrc.set_size(inner.input_size as i64);
                        }
                        servosrc.set_seekable(inner.seekable);

                        let sender_clone = sender.clone();
                        let is_ready = is_ready_clone.clone();
                        let observer_ = observer.clone();
                        let observer__ = observer.clone();
                        let observer___ = observer.clone();
                        let servosrc_ = servosrc.clone();
                        let enough_data_ = inner.enough_data.clone();
                        let enough_data__ = inner.enough_data.clone();
                        let stopping_need = inner.stopping.clone();
                        let stopping_enough = inner.stopping.clone();
                        let stopping_seek = inner.stopping.clone();
                        let seek_channel = Arc::new(Mutex::new(SeekChannel::new()));
                        servosrc.set_callbacks(
                            gstreamer_app::AppSrcCallbacks::builder()
                                .need_data(move |_, _| {
                                    if stopping_need.load(Ordering::Acquire) {
                                        return;
                                    }
                                    // We block the caller of the setup method until we get
                                    // the first need-data signal, so we ensure that we
                                    // don't miss any data between the moment the client
                                    // calls setup and the player is actually ready to
                                    // get any data.
                                    is_ready.call_once(|| {
                                        let _ = sender_clone.lock().unwrap().send(Ok(()));
                                    });

                                    enough_data_.store(false, Ordering::Relaxed);
                                    let _ = notify!(observer_, PlayerEvent::NeedData);
                                })
                                .enough_data(move |_| {
                                    if stopping_enough.load(Ordering::Acquire) {
                                        return;
                                    }
                                    enough_data__.store(true, Ordering::Relaxed);
                                    let _ = notify!(observer__, PlayerEvent::EnoughData);
                                })
                                .seek_data(move |_, offset| {
                                    if stopping_seek.load(Ordering::Acquire) {
                                        return false;
                                    }
                                    let (ret, ack_channel) = if servosrc_.set_seek_offset(offset) {
                                        let _ = notify!(
                                            observer___,
                                            PlayerEvent::SeekData(
                                                offset,
                                                seek_channel.lock().unwrap().sender()
                                            )
                                        );
                                        let (ret, ack_channel) =
                                            seek_channel.lock().unwrap()._await();
                                        servosrc_.set_seek_done();
                                        (ret, Some(ack_channel))
                                    } else {
                                        (true, None)
                                    };

                                    if let Some(ack_channel) = ack_channel {
                                        let _ = ack_channel.send(());
                                    }
                                    ret
                                })
                                .build(),
                        );

                        PlayerSource::Seekable(servosrc)
                    },
                    StreamType::Stream => {
                        let media_stream_src = source
                            .dynamic_cast::<ServoMediaStreamSrc>()
                            .expect("Source element is expected to be a ServoMediaStreamSrc!");
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Stream(media_stream_src)
                    },
                    StreamType::MediaSource => {
                        let media_source_src = source
                            .dynamic_cast::<ServoMediaSourceSrc>()
                            .expect("Source element is expected to be a ServoMediaSourceSrc!");
                        media_source_src.set_observer(observer.clone());
                        let pending_store = clearkey_store.lock().unwrap().clone();
                        if pending_store.is_some() {
                            let _ = media_source_src.set_clearkey_store(pending_store);
                        }
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::MediaSource(media_source_src)
                    },
                };

                inner.set_src(source);

                None
            });
            inner.pipeline_source_setup_id = Some(pipeline_source_setup_id);

            let weak_player = gst_player.downgrade();
            let error_handler_id = signal_adapter.connect_error(move |_, error, _details| {
                let _ = notify!(sender_clone, Err(PlayerError::Backend(error.to_string())));
                if let Some(player) = weak_player.upgrade() {
                    player.stop();
                }
            });

            gst_player.pause();

            (receiver, error_handler_id)
        };

        let result = receiver
            .recv_timeout(time::Duration::from_secs(10))
            .unwrap_or_else(|_| {
                Err(PlayerError::Backend(
                    "Timed out waiting for GStreamer source setup".into(),
                ))
            });
        if let Some(ref sa) = inner.lock().unwrap().signal_adapter {
            glib::signal::signal_handler_disconnect(sa, error_handler_id);
        }
        result
    }
}

macro_rules! inner_player_proxy_getter {
    ($fn_name:ident, $return_type:ty, $default_value:expr_2021) => {
        fn $fn_name(&self) -> $return_type {
            if self.setup().is_err() {
                return $default_value;
            }

            let inner = self.inner.borrow();
            let inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };
}

macro_rules! inner_player_proxy {
    ($fn_name:ident, $return_type:ty) => {
        fn $fn_name(&self) -> Result<$return_type, PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1)
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty, $arg2:ident, $arg2_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type, $arg2: $arg2_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1, $arg2)
        }
    };
}

impl Player for GStreamerPlayer {
    inner_player_proxy!(play, ());
    inner_player_proxy!(pause, ());
    inner_player_proxy_getter!(paused, bool, DEFAULT_PAUSED);
    inner_player_proxy_getter!(can_resume, bool, DEFAULT_CAN_RESUME);

    fn stop(&self) -> Result<(), PlayerError> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Some(inner_arc) = self.inner.borrow_mut().take() else {
            return Ok(());
        };

        // Release `inner` lock before disconnecting signals / stopping `gst_player`
        // because `PlaySignalAdapter::new_sync_emit` invokes signal handlers on
        // GstPlay's internal thread that lock `inner`.
        let (
            gst_player,
            signal_adapter,
            pipeline,
            source,
            video_sink,
            audio_sink,
            signal_ids,
            pipeline_signal_id,
        ) = {
            let mut inner = inner_arc.lock().unwrap();
            inner.stopping.store(true, Ordering::Release);
            inner.paused.set(true);
            inner.can_resume.set(false);
            inner.last_metadata = None;
            let gst_player = inner.player.take();
            let signal_adapter = inner.signal_adapter.take();
            let pipeline = gst_player.as_ref().map(|p| p.pipeline());
            let source = inner.source.take();
            let video_sink = inner.video_sink.clone();
            let audio_sink = inner.audio_sink.take();
            let signal_ids = std::mem::take(&mut inner.signal_ids);
            let pipeline_signal_id = inner.pipeline_source_setup_id.take();
            (
                gst_player,
                signal_adapter,
                pipeline,
                source,
                video_sink,
                audio_sink,
                signal_ids,
                pipeline_signal_id,
            )
        };

        if let Some(ref sa) = signal_adapter {
            for id in signal_ids {
                glib::signal::signal_handler_disconnect(sa, id);
            }
        }
        if let (Some(pipe), Some(id)) = (&pipeline, pipeline_signal_id) {
            glib::signal::signal_handler_disconnect(pipe, id);
        }

        video_sink.set_callbacks(gstreamer_app::AppSinkCallbacks::builder().build());
        if let Some(ref audio_sink) = audio_sink {
            audio_sink.set_callbacks(gstreamer_app::AppSinkCallbacks::builder().build());
        }

        match source {
            Some(PlayerSource::MediaSource(src)) => src.prepare_stop(),
            Some(PlayerSource::Seekable(src)) => {
                src.prepare_stop();
                src.set_callbacks(gstreamer_app::AppSrcCallbacks::builder().build());
            },
            _ => {},
        }

        drop(inner_arc);

        if gst_player.is_some() || signal_adapter.is_some() || pipeline.is_some() {
            let (tx_done, rx_done) = mpsc::channel();
            if std::thread::Builder::new()
                .name("GStreamerPlayerTeardown".to_owned())
                .spawn(move || {
                    if let Some(ref player) = gst_player {
                        player.stop();
                    }
                    // Drop PlaySignalAdapter before Play so GstPlaySignalAdapter never
                    // tears down against an already-disposed GstPlay instance.
                    drop(signal_adapter);
                    drop(gst_player);
                    drop(pipeline);
                    let _ = tx_done.send(());
                })
                .is_ok()
            {
                let _ = rx_done.recv_timeout(time::Duration::from_secs(2));
            }
        }

        Ok(())
    }
    inner_player_proxy!(end_of_stream, ());
    inner_player_proxy!(set_input_size, size, u64);
    inner_player_proxy!(set_seekable, seekable, bool);
    inner_player_proxy!(set_mute, muted, bool);
    inner_player_proxy_getter!(muted, bool, DEFAULT_MUTED);
    inner_player_proxy!(set_playback_rate, playback_rate, f64);
    inner_player_proxy_getter!(playback_rate, f64, DEFAULT_PLAYBACK_RATE);
    inner_player_proxy!(push_data, data, Vec<u8>);
    inner_player_proxy!(seek, time, f64);
    inner_player_proxy!(set_volume, volume, f64);
    inner_player_proxy_getter!(volume, f64, DEFAULT_VOLUME);
    inner_player_proxy_getter!(buffered, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(seekable, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy!(set_stream, stream, &MediaStreamId, only_stream, bool);
    inner_player_proxy!(set_audio_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(set_video_track, stream_index, i32, enabled, bool);

    fn set_download_buffering_enabled(&self, enable: bool) -> Result<(), PlayerError> {
        // We can't use the default implementation because we need to set the
        // download_buffering_enabled flag before calling setup, which is where
        // the "download" flag is set on the pipeline. That has to happen before
        // the URI is set.
        self.download_buffering_enabled
            .store(enable, Ordering::Relaxed);
        self.setup()?;
        Ok(())
    }

    fn render_use_gl(&self) -> bool {
        self.render.lock().unwrap().is_gl()
    }

    fn mse_add_source_buffer(&self, id: u32, mime_type: &str) -> Result<(), PlayerError> {
        self.setup()?;
        let inner = self.inner.borrow();
        let mut inner = inner.as_ref().unwrap().lock().unwrap();
        inner.mse_add_source_buffer(id, mime_type)
    }

    inner_player_proxy!(mse_remove_source_buffer, id, u32);
    inner_player_proxy!(mse_push_data, source_buffer_id, u32, data, Vec<u8>);
    inner_player_proxy!(mse_flush_track, source_buffer_id, u32);
    inner_player_proxy!(mse_end_of_stream, error, Option<MseEndOfStreamError>);
    inner_player_proxy!(mse_set_duration, duration, Option<f64>);
    inner_player_proxy!(mse_set_live_seekable_range, start, f64, end, f64);

    fn mse_commit_tracks(&self, tracks: &[MseTrackDescriptor]) -> Result<(), PlayerError> {
        self.setup()?;
        let inner = self.inner.borrow();
        let mut inner = inner.as_ref().unwrap().lock().unwrap();
        inner.mse_commit_tracks(tracks)
    }

    fn set_clearkey_store(&self, store: Option<ClearKeyStore>) -> Result<(), PlayerError> {
        *self.clearkey_store.lock().unwrap() = store.clone();
        if let Some(ref inner) = *self.inner.borrow() {
            let mut inner = inner.lock().unwrap();
            inner.set_clearkey_store(store)?;
        }
        Ok(())
    }

    fn notify_keys_updated(&self) -> Result<(), PlayerError> {
        if let Some(ref inner) = *self.inner.borrow() {
            let mut inner = inner.lock().unwrap();
            inner.notify_keys_updated()?;
        }
        Ok(())
    }
}

impl MediaInstance for GStreamerPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        if !self.can_resume() {
            return Ok(());
        }

        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Drop for GStreamerPlayer {
    fn drop(&mut self) {
        let _ = self.stop();
        let (tx_ack, rx_ack) = mpsc::channel();
        if self
            .backend_chan
            .lock()
            .unwrap()
            .send(BackendMsg::Shutdown {
                context: self.context_id,
                id: self.id,
                tx_ack,
            })
            .is_ok()
        {
            let _ = rx_ack.recv_timeout(time::Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use gstreamer::prelude::*;
    use servo_base::generic_channel::GenericCallback;
    use servo_media::BackendInit;
    use servo_media_player::audio::AudioRenderer;
    use servo_media_player::context::{GlApi, GlContext, NativeDisplay, PlayerGLContext};
    use servo_media_player::mse::{MseTrackBuffer, MseTrackType};
    use servo_media_player::video::{VideoFrame, VideoFrameRenderer};
    use servo_media_player::{MseTrackDescriptor, MseTrackKind, PlaybackState, PlayerEvent, StreamType};
    use servo_media_traits::ClientContextId;

    use crate::GStreamerBackend;
    use crate::media_source_src::{ServoMediaSourceSrc, register_servo_media_source_src};

    struct HeadlessGlContext;
    impl PlayerGLContext for HeadlessGlContext {
        fn get_gl_context(&self) -> GlContext {
            GlContext::Unknown
        }

        fn get_native_display(&self) -> NativeDisplay {
            NativeDisplay::Headless
        }

        fn get_gl_api(&self) -> GlApi {
            GlApi::None
        }
    }

    struct CountingVideoRenderer {
        frames: Arc<AtomicUsize>,
    }

    impl VideoFrameRenderer for CountingVideoRenderer {
        fn render(&mut self, _frame: VideoFrame) {
            self.frames.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct CountingAudioRenderer {
        samples: Arc<AtomicUsize>,
    }

    impl AudioRenderer for CountingAudioRenderer {
        fn render(&mut self, _sample: Box<dyn AsRef<[f32]>>, _channel: u32) {
            self.samples.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn generate_fmp4_bytes(pipeline_desc: &str) -> Vec<u8> {
        let pipeline = gstreamer::parse::launch(pipeline_desc)
            .expect("failed to parse fmp4 generator pipeline")
            .downcast::<gstreamer::Pipeline>()
            .expect("expected Pipeline");
        let sink = pipeline
            .by_name("sink")
            .expect("missing appsink")
            .downcast::<gstreamer_app::AppSink>()
            .expect("expected AppSink");
        let bus = pipeline.bus().expect("missing bus");

        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("failed to start fmp4 generator pipeline");

        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out generating fMP4 bytes from {pipeline_desc}"
            );
            if let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(200)) {
                let buf = sample.buffer().expect("sample missing buffer");
                let map = buf.map_readable().expect("failed to map buffer");
                out.extend_from_slice(map.as_slice());
                continue;
            }
            if let Some(msg) = bus.pop_filtered(&[
                gstreamer::MessageType::Eos,
                gstreamer::MessageType::Error,
            ]) {
                match msg.view() {
                    gstreamer::MessageView::Eos(_) => break,
                    gstreamer::MessageView::Error(err) => {
                        panic!("fMP4 generator error: {} ({:?})", err.error(), err.debug());
                    },
                    _ => {},
                }
            }
        }

        let _ = pipeline.set_state(gstreamer::State::Null);
        assert!(!out.is_empty(), "generated empty fMP4 stream");
        out
    }

    fn track_descriptors_for(sb_id: u32, mime: &str, tb: &MseTrackBuffer) -> Vec<MseTrackDescriptor> {
        let init = tb
            .active_init_segment()
            .expect("expected parsed InitSegment");
        init.tracks
            .iter()
            .map(|t| MseTrackDescriptor {
                source_buffer_id: sb_id,
                track_id: t.track_id,
                kind: match t.track_type {
                    MseTrackType::Audio => MseTrackKind::Audio,
                    MseTrackType::Video => MseTrackKind::Video,
                    MseTrackType::Text => MseTrackKind::Text,
                    MseTrackType::Unknown => MseTrackKind::Video,
                },
                mime_type: mime.to_string(),
                codec: t.codec.clone(),
            })
            .collect()
    }

    #[test]
    fn mse_source_uri_handler_queries_and_no_more_pads_coordination() {
        gstreamer::init().unwrap();
        register_servo_media_source_src().unwrap();

        let elem = gstreamer::Element::make_from_uri(
            gstreamer::URIType::Src,
            "mediasource://",
            None,
        )
        .expect("mediasource:// URIHandler should instantiate ServoMediaSourceSrc");
        let src = elem
            .downcast::<ServoMediaSourceSrc>()
            .expect("expected ServoMediaSourceSrc");

        let no_more_pads_count = Arc::new(AtomicUsize::new(0));
        let no_more_pads_clone = no_more_pads_count.clone();
        src.connect_no_more_pads(move |_| {
            no_more_pads_clone.fetch_add(1, Ordering::SeqCst);
        });

        src.add_source_buffer(1, "video/mp4").unwrap();
        src.add_source_buffer(2, "audio/mp4").unwrap();
        src.set_duration(Some(12.5)).unwrap();

        let descs = vec![
            MseTrackDescriptor {
                source_buffer_id: 1,
                track_id: 1,
                kind: MseTrackKind::Video,
                mime_type: "video/mp4".into(),
                codec: Some("avc1".into()),
            },
            MseTrackDescriptor {
                source_buffer_id: 2,
                track_id: 2,
                kind: MseTrackKind::Audio,
                mime_type: "audio/mp4".into(),
                codec: Some("mp4a".into()),
            },
        ];

        // Commit before data: no_more_pads must NOT fire until both committed tracks push init data.
        src.commit_tracks(&descs).unwrap();
        assert_eq!(no_more_pads_count.load(Ordering::SeqCst), 0);

        src.push_data(1, vec![0u8; 16]).unwrap();
        assert_eq!(no_more_pads_count.load(Ordering::SeqCst), 0);

        src.push_data(2, vec![0u8; 16]).unwrap();
        assert_eq!(no_more_pads_count.load(Ordering::SeqCst), 1);
        assert!(src.pads_finalized());
        assert_eq!(src.pads().len(), 2);

        // Verify upstream Duration & Seeking queries on exposed GhostPad.
        let pad = src.pads().into_iter().next().unwrap();
        let mut dur_q = gstreamer::query::Duration::new(gstreamer::Format::Time);
        assert!(pad.query(&mut dur_q));
        assert_eq!(
            dur_q.result(),
            gstreamer::GenericFormattedValue::Time(Some(gstreamer::ClockTime::from_mseconds(
                12_500
            )))
        );

        let mut seek_q = gstreamer::query::Seeking::new(gstreamer::Format::Time);
        assert!(pad.query(&mut seek_q));
        let (seekable, start, end) = seek_q.result();
        assert!(seekable);
        assert_eq!(
            start,
            gstreamer::GenericFormattedValue::Time(Some(gstreamer::ClockTime::ZERO))
        );
        assert_eq!(
            end,
            gstreamer::GenericFormattedValue::Time(Some(gstreamer::ClockTime::from_mseconds(
                12_500
            )))
        );

        // Live duration (None) + live seekable range.
        src.set_duration(None).unwrap();
        src.set_live_seekable_range(5.0, 25.0).unwrap();
        let mut live_dur_q = gstreamer::query::Duration::new(gstreamer::Format::Time);
        assert!(pad.query(&mut live_dur_q));
        assert_eq!(
            live_dur_q.result(),
            gstreamer::GenericFormattedValue::Time(None)
        );
        let mut live_seek_q = gstreamer::query::Seeking::new(gstreamer::Format::Time);
        assert!(pad.query(&mut live_seek_q));
        let (live_seekable, l_start, l_end) = live_seek_q.result();
        assert!(live_seekable);
        assert_eq!(
            l_start,
            gstreamer::GenericFormattedValue::Time(Some(gstreamer::ClockTime::from_seconds(5)))
        );
        assert_eq!(
            l_end,
            gstreamer::GenericFormattedValue::Time(Some(gstreamer::ClockTime::from_seconds(25)))
        );
    }

    #[test]
    fn mse_offline_av_fmp4_playback_seek_and_eos() {
        let backend = GStreamerBackend::init();

        let v_bytes = generate_fmp4_bytes(
            "videotestsrc num-buffers=30 ! video/x-raw,width=64,height=64,framerate=15/1 ! videoconvert ! x264enc tune=zerolatency key-int-max=15 ! h264parse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );
        let a_bytes = generate_fmp4_bytes(
            "audiotestsrc num-buffers=90 ! audio/x-raw,rate=44100,channels=1 ! audioconvert ! audioresample ! avenc_aac ! aacparse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );

        let mut v_buf = MseTrackBuffer::new_video();
        v_buf
            .append(&v_bytes)
            .expect("failed to parse generated video fMP4");
        let mut a_buf = MseTrackBuffer::new_audio();
        a_buf
            .append(&a_bytes)
            .expect("failed to parse generated audio fMP4");

        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let callback = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .expect("failed to create GenericCallback");

        let video_frames = Arc::new(AtomicUsize::new(0));
        let audio_samples = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 1),
            StreamType::MediaSource,
            callback,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            Some(Arc::new(Mutex::new(CountingAudioRenderer {
                samples: audio_samples.clone(),
            }))),
            Box::new(HeadlessGlContext),
        );

        {
            let guard = player.lock().unwrap();
            guard
                .mse_add_source_buffer(1, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "audio/mp4; codecs=\"mp4a.40.2\"")
                .unwrap();
            guard.mse_set_duration(Some(2.0)).unwrap();

            let mut descs = track_descriptors_for(1, "video/mp4", &v_buf);
            descs.extend(track_descriptors_for(2, "audio/mp4", &a_buf));
            guard.mse_commit_tracks(&descs).unwrap();

            let v_init = (*v_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(1, v_init).unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }

            let a_init = (*a_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(2, a_init).unwrap();
            for seg in a_buf.media_segments() {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }
        }

        let mut saw_metadata = false;
        let mut saw_playing = false;
        let mut saw_video_frame = false;
        let mut saw_need_data = false;

        let deadline = Instant::now() + Duration::from_secs(10);
        while !saw_metadata && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PlayerEvent::MetadataUpdated(meta)) => {
                    assert!(
                        !meta.video_tracks.is_empty(),
                        "expected video track in MetadataUpdated: {meta:?}"
                    );
                    assert!(
                        !meta.audio_tracks.is_empty(),
                        "expected audio track in MetadataUpdated: {meta:?}"
                    );
                    saw_metadata = true;
                },
                Ok(PlayerEvent::MseNeedData(_)) => saw_need_data = true,
                Ok(PlayerEvent::VideoFrameUpdated) => saw_video_frame = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error before MetadataUpdated: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(saw_metadata, "timed out waiting for MetadataUpdated");

        {
            let guard = player.lock().unwrap();
            assert!(!guard.seekable().is_empty(), "expected non-empty seekable range");
            guard.play().unwrap();
        }

        let play_deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_playing && (saw_video_frame || video_frames.load(Ordering::Relaxed) > 0))
            && Instant::now() < play_deadline
        {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::VideoFrameUpdated) => saw_video_frame = true,
                Ok(PlayerEvent::MseNeedData(_)) => saw_need_data = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error during play: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(saw_playing, "timed out waiting for StateChanged(Playing)");
        assert!(
            saw_video_frame || video_frames.load(Ordering::Relaxed) > 0,
            "expected decoded video frames during playback"
        );
        let _ = saw_need_data;

        {
            let guard = player.lock().unwrap();
            guard.seek(1.0).unwrap();
        }

        let mut seek_target: Option<f64> = None;
        let mut saw_seek_done = false;
        let mut saw_eos = false;
        let mut post_seek_events: Vec<String> = Vec::new();
        let seek_deadline = Instant::now() + Duration::from_secs(10);
        while seek_target.is_none() && Instant::now() < seek_deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    post_seek_events.push(format!("pre:{ev:?}"));
                    match ev {
                        PlayerEvent::MseSeek(target) => seek_target = Some(target),
                        PlayerEvent::SeekDone(_) => saw_seek_done = true,
                        PlayerEvent::EndOfStream => saw_eos = true,
                        PlayerEvent::Error(err) => {
                            panic!("unexpected PlayerEvent::Error waiting for MseSeek: {err}");
                        },
                        _ => {},
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        let target = seek_target.expect("timed out waiting for PlayerEvent::MseSeek");
        assert!(
            (target - 1.0).abs() < 0.25,
            "unexpected MseSeek target {target}"
        );

        {
            let guard = player.lock().unwrap();
            guard.mse_flush_track(1).unwrap();
            guard.mse_flush_track(2).unwrap();

            let (v_init, v_segs) = v_buf
                .segments_for_seek(target)
                .expect("expected video segments_for_seek");
            guard.mse_push_data(1, (*v_init.bytes).clone()).unwrap();
            for seg in v_segs {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }

            let (a_init, a_segs) = a_buf
                .segments_for_seek(target)
                .expect("expected audio segments_for_seek");
            guard.mse_push_data(2, (*a_init.bytes).clone()).unwrap();
            for seg in a_segs {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }

            guard.mse_end_of_stream(None).unwrap();
        }

        let eos_deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_seek_done && saw_eos) && Instant::now() < eos_deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    post_seek_events.push(format!("post:{ev:?}"));
                    match ev {
                        PlayerEvent::SeekDone(_) => saw_seek_done = true,
                        PlayerEvent::EndOfStream => saw_eos = true,
                        PlayerEvent::Error(err) => {
                            panic!("unexpected PlayerEvent::Error after seek/EOS: {err}");
                        },
                        _ => {},
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(
            saw_seek_done,
            "timed out waiting for PlayerEvent::SeekDone (events={post_seek_events:?})"
        );
        assert!(
            saw_eos,
            "timed out waiting for PlayerEvent::EndOfStream (events={post_seek_events:?})"
        );
        assert!(
            audio_samples.load(Ordering::Relaxed) > 0,
            "expected decoded audio samples during fMP4 playback + seek"
        );

        let _ = player.lock().unwrap().stop();
        drop(player);
        drop(backend);
    }

    #[test]
    fn mse_huge_and_infinite_clocktime_queries_and_early_teardown_never_panic_or_hang() {
        gstreamer::init().unwrap();
        register_servo_media_source_src().unwrap();

        let elem = gstreamer::Element::make_from_uri(
            gstreamer::URIType::Src,
            "mediasource://",
            None,
        )
        .unwrap();
        let src = elem.downcast::<ServoMediaSourceSrc>().unwrap();
        src.add_source_buffer(1, "video/mp4").unwrap();
        src.commit_tracks(&[MseTrackDescriptor {
            source_buffer_id: 1,
            track_id: 1,
            kind: MseTrackKind::Video,
            mime_type: "video/mp4".into(),
            codec: Some("avc1".into()),
        }])
        .unwrap();
        src.push_data(1, vec![0u8; 8]).unwrap();

        let pad = src.pads().into_iter().next().unwrap();

        // Huge finite duration / live seekable range (such as f64::MAX) must never panic with GST_CLOCK_TIME_NONE.
        src.set_duration(Some(f64::MAX)).unwrap();
        let mut dur_q = gstreamer::query::Duration::new(gstreamer::Format::Time);
        assert!(pad.query(&mut dur_q));
        assert_eq!(dur_q.result(), gstreamer::GenericFormattedValue::Time(None));

        src.set_live_seekable_range(0.0, f64::MAX).unwrap();
        let mut seek_q = gstreamer::query::Seeking::new(gstreamer::Format::Time);
        assert!(pad.query(&mut seek_q));
        let (seekable, start, end) = seek_q.result();
        assert!(seekable);
        assert_eq!(
            start,
            gstreamer::GenericFormattedValue::Time(Some(gstreamer::ClockTime::ZERO))
        );
        assert_eq!(end, gstreamer::GenericFormattedValue::Time(None));

        // Infinite duration also maps cleanly to ClockTime::NONE.
        src.set_duration(Some(f64::INFINITY)).unwrap();
        let mut inf_dur_q = gstreamer::query::Duration::new(gstreamer::Format::Time);
        assert!(pad.query(&mut inf_dur_q));
        assert_eq!(
            inf_dur_q.result(),
            gstreamer::GenericFormattedValue::Time(None)
        );

        // Early teardown (stop + drop before no_more_pads() and with multiple players) must complete cleanly.
        let backend = GStreamerBackend::init();
        let make_player = |id: u32| {
            let cb = GenericCallback::new(|_| {}).unwrap();
            backend.create_player(
                &ClientContextId::build(1, id),
                StreamType::MediaSource,
                cb,
                None,
                None,
                Box::new(HeadlessGlContext),
            )
        };
        let p1 = make_player(1);
        {
            let guard = p1.lock().unwrap();
            guard.mse_add_source_buffer(1, "video/mp4").unwrap();
            guard
                .mse_set_live_seekable_range(0.0, f64::MAX)
                .unwrap();
            let _ = guard.stop();
        }
        drop(p1);

        let p2 = make_player(2);
        {
            let guard = p2.lock().unwrap();
            guard.mse_add_source_buffer(1, "audio/mp4").unwrap();
            let _ = guard.stop();
        }
        drop(p2);
        drop(backend);
    }

    #[test]
    fn mse_live_infinite_duration_active_playback_and_clean_teardown() {
        let backend = GStreamerBackend::init();

        let v_bytes = generate_fmp4_bytes(
            "videotestsrc num-buffers=30 ! video/x-raw,width=64,height=64,framerate=15/1 ! videoconvert ! x264enc tune=zerolatency key-int-max=15 ! h264parse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );
        let a_bytes = generate_fmp4_bytes(
            "audiotestsrc num-buffers=90 ! audio/x-raw,rate=44100,channels=1 ! audioconvert ! audioresample ! avenc_aac ! aacparse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );

        let mut v_buf = MseTrackBuffer::new_video();
        v_buf.append(&v_bytes).unwrap();
        let mut a_buf = MseTrackBuffer::new_audio();
        a_buf.append(&a_bytes).unwrap();

        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let callback = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let audio_samples = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 1),
            StreamType::MediaSource,
            callback,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            Some(Arc::new(Mutex::new(CountingAudioRenderer {
                samples: audio_samples.clone(),
            }))),
            Box::new(HeadlessGlContext),
        );

        {
            let guard = player.lock().unwrap();
            guard
                .mse_add_source_buffer(1, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "audio/mp4; codecs=\"mp4a.40.2\"")
                .unwrap();
            guard.mse_set_duration(Some(f64::INFINITY)).unwrap();

            let mut descs = track_descriptors_for(1, "video/mp4", &v_buf);
            descs.extend(track_descriptors_for(2, "audio/mp4", &a_buf));
            guard.mse_commit_tracks(&descs).unwrap();

            let v_init = (*v_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(1, v_init).unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }

            let a_init = (*a_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(2, a_init).unwrap();
            for seg in a_buf.media_segments() {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }
        }

        let mut saw_metadata = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !saw_metadata && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PlayerEvent::MetadataUpdated(meta)) => {
                    assert!(
                        meta.duration.is_none(),
                        "expected None duration for live/+Infinity MSE stream, got {:?}",
                        meta.duration
                    );
                    assert!(meta.is_live);
                    saw_metadata = true;
                },
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error on live MSE stream: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(saw_metadata, "timed out waiting for live MetadataUpdated");

        player.lock().unwrap().play().unwrap();
        let mut saw_playing = false;
        let play_deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_playing && video_frames.load(Ordering::Relaxed) > 0)
            && Instant::now() < play_deadline
        {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error during live play: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(saw_playing, "timed out waiting for live StateChanged(Playing)");

        // Tear down while still actively playing without EOS (must not hang).
        let stop_start = Instant::now();
        let _ = player.lock().unwrap().stop();
        drop(player);
        drop(backend);
        assert!(
            stop_start.elapsed() < Duration::from_secs(5),
            "active MSE teardown took too long: {:?}",
            stop_start.elapsed()
        );
    }

    #[test]
    fn mse_play_before_commit_and_init_advances_position() {
        let backend = GStreamerBackend::init();

        let v_bytes = generate_fmp4_bytes(
            "videotestsrc num-buffers=45 ! video/x-raw,width=64,height=64,framerate=15/1 ! videoconvert ! x264enc tune=zerolatency key-int-max=15 ! h264parse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );
        let a_bytes = generate_fmp4_bytes(
            "audiotestsrc num-buffers=135 ! audio/x-raw,rate=44100,channels=1 ! audioconvert ! audioresample ! avenc_aac ! aacparse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );

        let mut v_buf = MseTrackBuffer::new_video();
        v_buf.append(&v_bytes).unwrap();
        let mut a_buf = MseTrackBuffer::new_audio();
        a_buf.append(&a_bytes).unwrap();

        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let callback = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let audio_samples = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 10),
            StreamType::MediaSource,
            callback,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            Some(Arc::new(Mutex::new(CountingAudioRenderer {
                samples: audio_samples.clone(),
            }))),
            Box::new(HeadlessGlContext),
        );

        {
            let guard = player.lock().unwrap();
            guard
                .mse_add_source_buffer(1, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "audio/mp4; codecs=\"mp4a.40.2\"")
                .unwrap();
            guard.mse_set_duration(Some(3.0)).unwrap();

            // Call play() BEFORE mse_commit_tracks() and BEFORE pushing init/media data.
            guard.play().unwrap();
            assert!(!guard.paused(), "player should stay unpaused before pads finalize");

            let mut descs = track_descriptors_for(1, "video/mp4", &v_buf);
            descs.extend(track_descriptors_for(2, "audio/mp4", &a_buf));
            guard.mse_commit_tracks(&descs).unwrap();

            let v_init = (*v_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(1, v_init).unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }

            let a_init = (*a_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(2, a_init).unwrap();
            for seg in a_buf.media_segments() {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }
        }

        let mut saw_metadata = false;
        let mut saw_playing = false;
        let mut max_position = 0.0_f64;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_metadata && saw_playing && max_position > 0.0) && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::MetadataUpdated(_)) => saw_metadata = true,
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::PositionChanged(pos)) => {
                    if pos > max_position {
                        max_position = pos;
                    }
                },
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error when play() precedes commit: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(saw_metadata, "timed out waiting for MetadataUpdated");
        assert!(saw_playing, "timed out waiting for StateChanged(Playing)");
        assert!(
            max_position > 0.0,
            "expected PositionChanged > 0.0 after play() before commit/init, got {max_position}"
        );
        assert!(
            video_frames.load(Ordering::Relaxed) > 0,
            "expected decoded video frames"
        );

        let teardown_start = Instant::now();
        drop(player);
        drop(backend);
        assert!(
            teardown_start.elapsed() < Duration::from_secs(5),
            "teardown took too long: {:?}",
            teardown_start.elapsed()
        );
    }

    #[test]
    fn mse_active_live_and_unprerolled_secondary_teardown_under_5s() {
        let backend = GStreamerBackend::init();

        let v_bytes = generate_fmp4_bytes(
            "videotestsrc num-buffers=30 ! video/x-raw,width=64,height=64,framerate=15/1 ! videoconvert ! x264enc tune=zerolatency key-int-max=15 ! h264parse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );
        let a_bytes = generate_fmp4_bytes(
            "audiotestsrc num-buffers=90 ! audio/x-raw,rate=44100,channels=1 ! audioconvert ! audioresample ! avenc_aac ! aacparse ! mp4mux fragment-duration=500 ! appsink name=sink",
        );

        let mut v_buf = MseTrackBuffer::new_video();
        v_buf.append(&v_bytes).unwrap();
        let mut a_buf = MseTrackBuffer::new_audio();
        a_buf.append(&a_bytes).unwrap();

        let (tx1, rx1) = mpsc::channel::<PlayerEvent>();
        let cb1 = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx1.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let audio_samples = Arc::new(AtomicUsize::new(0));
        let active_player = backend.create_player(
            &ClientContextId::build(1, 20),
            StreamType::MediaSource,
            cb1,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            Some(Arc::new(Mutex::new(CountingAudioRenderer {
                samples: audio_samples.clone(),
            }))),
            Box::new(HeadlessGlContext),
        );

        {
            let guard = active_player.lock().unwrap();
            guard
                .mse_add_source_buffer(1, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "audio/mp4; codecs=\"mp4a.40.2\"")
                .unwrap();
            guard.mse_set_duration(Some(f64::INFINITY)).unwrap();

            let mut descs = track_descriptors_for(1, "video/mp4", &v_buf);
            descs.extend(track_descriptors_for(2, "audio/mp4", &a_buf));
            guard.mse_commit_tracks(&descs).unwrap();

            let v_init = (*v_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(1, v_init).unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }

            let a_init = (*a_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(2, a_init).unwrap();
            for seg in a_buf.media_segments() {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }

            guard.play().unwrap();
        }

        let mut saw_playing = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_playing && video_frames.load(Ordering::Relaxed) > 0)
            && Instant::now() < deadline
        {
            match rx1.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error on active live player: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        assert!(saw_playing, "timed out waiting for active live playback");

        // Create an unprerolled secondary MSE player stuck in ASYNC preroll (0 pads, no data).
        let cb2 = GenericCallback::new(|_| {}).unwrap();
        let unprerolled_player = backend.create_player(
            &ClientContextId::build(1, 21),
            StreamType::MediaSource,
            cb2,
            None,
            None,
            Box::new(HeadlessGlContext),
        );
        {
            let guard = unprerolled_player.lock().unwrap();
            guard
                .mse_add_source_buffer(1, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "audio/mp4; codecs=\"mp4a.40.2\"")
                .unwrap();
            guard.mse_set_duration(Some(f64::INFINITY)).unwrap();
            let mut descs = track_descriptors_for(1, "video/mp4", &v_buf);
            descs.extend(track_descriptors_for(2, "audio/mp4", &a_buf));
            guard.mse_commit_tracks(&descs).unwrap();
            guard.play().unwrap();
        }

        // Drop both active live and unprerolled players without EOS; must finish < 5s.
        let teardown_start = Instant::now();
        drop(unprerolled_player);
        drop(active_player);
        drop(backend);
        assert!(
            teardown_start.elapsed() < Duration::from_secs(5),
            "combined live + unprerolled MSE teardown took too long: {:?}",
            teardown_start.elapsed()
        );
    }

    #[test]
    fn mse_webm_vp9_opus_default_audio_mute_volume_and_samples() {
        use servo_media_player::Player;

        let backend = GStreamerBackend::init();

        // 1. Verify default-audio (no custom AudioRenderer) unmute/volume forwarding even when
        //    starting at DEFAULT_MUTED (false) / DEFAULT_VOLUME (1.0), so stream-restore state on
        //    the underlying sink/pipeline cannot keep audio silenced.
        let (tx_be, _rx_be) = mpsc::channel();
        let cb_default = GenericCallback::new(|_| {}).unwrap();
        let default_audio_player = super::GStreamerPlayer::new(
            999,
            &ClientContextId::build(1, 30),
            Arc::new(Mutex::new(tx_be)),
            StreamType::MediaSource,
            cb_default,
            None,
            None,
            Box::new(HeadlessGlContext),
        );
        default_audio_player.setup().unwrap();
        assert!(!default_audio_player.muted());
        assert!((default_audio_player.volume() - 1.0).abs() < 1e-6);

        let pipeline = {
            let inner = default_audio_player.inner.borrow();
            inner
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .player
                .as_ref()
                .unwrap()
                .pipeline()
        };
        pipeline.set_property("mute", true);
        pipeline.set_property("volume", 0.25f64);

        default_audio_player.set_mute(false).unwrap();
        default_audio_player.set_volume(1.0).unwrap();
        let sync_deadline = Instant::now() + Duration::from_secs(2);
        while (pipeline.property::<bool>("mute")
            || (pipeline.property::<f64>("volume") - 1.0).abs() > 1e-3)
            && Instant::now() < sync_deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pipeline.property::<bool>("mute"),
            "set_mute(false) must forward to GstPlay even when cached muted state was false"
        );
        assert!(
            (pipeline.property::<f64>("volume") - 1.0).abs() < 1e-3,
            "set_volume(1.0) must forward to GstPlay even when cached volume was 1.0"
        );
        let _ = default_audio_player.stop();
        drop(pipeline);
        drop(default_audio_player);

        // 2. Verify dual-track WebM VP9 + Opus MSE playback decodes both video frames and audio samples.
        let v_bytes = generate_fmp4_bytes(
            "videotestsrc num-buffers=30 ! video/x-raw,width=64,height=64,framerate=15/1 ! videoconvert ! vp9enc deadline=1 ! webmmux streamable=true max-cluster-duration=500000000 ! appsink name=sink",
        );
        let a_bytes = generate_fmp4_bytes(
            "audiotestsrc num-buffers=90 ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! audioresample ! opusenc ! webmmux streamable=true max-cluster-duration=500000000 ! appsink name=sink",
        );

        let mut v_buf = MseTrackBuffer::new_video();
        v_buf
            .append(&v_bytes)
            .expect("failed to parse generated WebM VP9 bytes");
        let mut a_buf = MseTrackBuffer::new_audio();
        a_buf
            .append(&a_bytes)
            .expect("failed to parse generated WebM Opus bytes");
        assert!(
            !v_buf.media_segments().is_empty(),
            "expected parsed WebM VP9 media segments"
        );
        assert!(
            !a_buf.media_segments().is_empty(),
            "expected parsed WebM Opus media segments"
        );

        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let cb = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let audio_samples = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 31),
            StreamType::MediaSource,
            cb,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            Some(Arc::new(Mutex::new(CountingAudioRenderer {
                samples: audio_samples.clone(),
            }))),
            Box::new(HeadlessGlContext),
        );

        {
            let guard = player.lock().unwrap();
            guard
                .mse_add_source_buffer(1, "audio/webm; codecs=\"opus\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "video/webm; codecs=\"vp9\"")
                .unwrap();
            guard.mse_set_duration(Some(2.0)).unwrap();

            let mut descs = track_descriptors_for(1, "audio/webm", &a_buf);
            descs.extend(track_descriptors_for(2, "video/webm", &v_buf));
            guard.mse_commit_tracks(&descs).unwrap();

            let a_init = (*a_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(1, a_init).unwrap();
            for seg in a_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }

            let v_init = (*v_buf.active_init_segment().unwrap().bytes).clone();
            guard.mse_push_data(2, v_init).unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }

            guard.mse_end_of_stream(None).unwrap();
            guard.play().unwrap();
        }

        let mut saw_metadata_with_av = false;
        let mut saw_playing = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_metadata_with_av
            && saw_playing
            && video_frames.load(Ordering::Relaxed) > 0
            && audio_samples.load(Ordering::Relaxed) > 0)
            && Instant::now() < deadline
        {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::MetadataUpdated(meta)) => {
                    if !meta.video_tracks.is_empty() && !meta.audio_tracks.is_empty() {
                        saw_metadata_with_av = true;
                    }
                },
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected PlayerEvent::Error during WebM VP9+Opus playback: {err}");
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("event channel closed: {err}"),
            }
        }

        assert!(
            saw_metadata_with_av,
            "expected MetadataUpdated with both VP9 video and Opus audio tracks"
        );
        assert!(saw_playing, "timed out waiting for StateChanged(Playing)");
        assert!(
            video_frames.load(Ordering::Relaxed) > 0,
            "expected decoded VP9 video frames"
        );
        assert!(
            audio_samples.load(Ordering::Relaxed) > 0,
            "expected decoded Opus audio samples"
        );

        let teardown_start = Instant::now();
        let _ = player.lock().unwrap().stop();
        drop(player);
        drop(backend);
        assert!(teardown_start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mse_cenc_av_fmp4_waiting_for_key_and_key_rotation() {
        let ffmpeg_path = std::path::Path::new("/usr/bin/ffmpeg");
        if !ffmpeg_path.exists() {
            return;
        }
        let backend = GStreamerBackend::init();

        let key_v_hex = "00112233445566778899aabbccddeeff";
        let kid_v_hex = "11223344556677889900aabbccddeeff";
        let key_a_hex = "ffeeddccbbaa99887766554433221100";
        let kid_a_hex = "ffeeddccbbaa00998877665544332211";

        let key_v: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let kid_v: [u8; 16] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x00, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let key_a: [u8; 16] = [
            0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22,
            0x11, 0x00,
        ];
        let kid_a: [u8; 16] = [
            0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x00, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33,
            0x22, 0x11,
        ];

        let run_ffmpeg_pipe = |args: &[&str]| -> Vec<u8> {
            let out = std::process::Command::new(ffmpeg_path)
                .args(args)
                .output()
                .expect("invoke ffmpeg");
            assert!(
                out.status.success(),
                "ffmpeg failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };

        let video_bytes = run_ffmpeg_pipe(&[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=25",
            "-t",
            "0.8",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "20",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-encryption_scheme",
            "cenc-aes-ctr",
            "-encryption_key",
            key_v_hex,
            "-encryption_kid",
            kid_v_hex,
            "-f",
            "mp4",
            "pipe:1",
        ]);

        let audio_bytes = run_ffmpeg_pipe(&[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "0.8",
            "-c:a",
            "aac",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-encryption_scheme",
            "cenc-aes-ctr",
            "-encryption_key",
            key_a_hex,
            "-encryption_kid",
            kid_a_hex,
            "-f",
            "mp4",
            "pipe:1",
        ]);

        let mut v_buf = MseTrackBuffer::new_video();
        let v_res = v_buf.append(&video_bytes).unwrap();
        assert!(!v_res.encrypted_init_data.is_empty());
        assert_eq!(v_buf.active_init_segment().unwrap().tracks[0].codec.as_deref(), Some("avc1"));

        let mut a_buf = MseTrackBuffer::new_audio();
        let a_res = a_buf.append(&audio_bytes).unwrap();
        assert!(!a_res.encrypted_init_data.is_empty());
        assert_eq!(a_buf.active_init_segment().unwrap().tracks[0].codec.as_deref(), Some("mp4a"));

        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let cb = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let audio_samples = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 32),
            StreamType::MediaSource,
            cb,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            Some(Arc::new(Mutex::new(CountingAudioRenderer {
                samples: audio_samples.clone(),
            }))),
            Box::new(HeadlessGlContext),
        );

        let store = servo_media_player::cenc::ClearKeyStore::new();
        {
            let guard = player.lock().unwrap();
            guard.set_clearkey_store(Some(store.clone())).unwrap();
            guard
                .mse_add_source_buffer(1, "audio/mp4; codecs=\"mp4a.40.2\"")
                .unwrap();
            guard
                .mse_add_source_buffer(2, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard.mse_set_duration(Some(0.8)).unwrap();

            let mut descs = track_descriptors_for(1, "audio/mp4", &a_buf);
            descs.extend(track_descriptors_for(2, "video/mp4", &v_buf));
            guard.mse_commit_tracks(&descs).unwrap();

            guard
                .mse_push_data(1, (*a_buf.active_init_segment().unwrap().bytes).clone())
                .unwrap();
            for seg in a_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }
            guard
                .mse_push_data(2, (*v_buf.active_init_segment().unwrap().bytes).clone())
                .unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(2, (*seg.bytes).clone()).unwrap();
            }
            guard.mse_end_of_stream(None).unwrap();
            guard.play().unwrap();
        }

        // Expect WaitingForKey before keys are provisioned.
        let mut saw_waiting = false;
        let wait_deadline = Instant::now() + Duration::from_secs(3);
        while !saw_waiting && Instant::now() < wait_deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(PlayerEvent::WaitingForKey) => saw_waiting = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected error before keys: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(saw_waiting, "expected WaitingForKey before adding keys");

        // Provision only video key first, then audio key.
        store.set_key(kid_v, key_v);
        player.lock().unwrap().notify_keys_updated().unwrap();
        store.set_key(kid_a, key_a);
        player.lock().unwrap().notify_keys_updated().unwrap();

        let mut saw_playing = false;
        let mut saw_eos = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_playing
            && saw_eos
            && video_frames.load(Ordering::Relaxed) > 0
            && audio_samples.load(Ordering::Relaxed) > 0)
            && Instant::now() < deadline
        {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::EndOfStream) => saw_eos = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected error during AV CENC playback: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }

        assert!(saw_playing, "expected Playing for AV CENC fMP4");
        assert!(saw_eos, "expected deferred EndOfStream after all encrypted segments drained");
        assert!(video_frames.load(Ordering::Relaxed) > 0, "expected decoded video frames");
        assert!(audio_samples.load(Ordering::Relaxed) > 0, "expected decoded audio samples");

        let _ = player.lock().unwrap().stop();
        drop(player);
        drop(backend);
    }

    #[test]
    fn mse_cbcs_h264_fmp4_real_playback() {
        let ffmpeg_path = std::path::Path::new("/usr/bin/ffmpeg");
        if !ffmpeg_path.exists() {
            return;
        }
        let backend = GStreamerBackend::init();

        let out = std::process::Command::new(ffmpeg_path)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=24",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "24",
                "-keyint_min",
                "24",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
                "-f",
                "mp4",
                "pipe:1",
            ])
            .output()
            .expect("invoke ffmpeg");
        assert!(
            out.status.success(),
            "ffmpeg clear fMP4 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let clear_bytes = out.stdout;

        let kid: [u8; 16] = [
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99,
        ];
        let key: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let constant_iv: [u8; 16] = [
            0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0, 0xe0,
            0xf0, 0x00,
        ];
        let cbcs_bytes = servo_media_player::cenc::encrypt_clear_fmp4_to_cbcs(
            &clear_bytes,
            kid,
            key,
            constant_iv,
        )
        .expect("encrypt clear fmp4 to cbcs");

        let mut v_buf = MseTrackBuffer::new_video();
        let v_res = v_buf.append(&cbcs_bytes).unwrap();
        assert!(!v_res.encrypted_init_data.is_empty());
        assert_eq!(
            v_buf.active_init_segment().unwrap().tracks[0]
                .codec
                .as_deref(),
            Some("avc1")
        );

        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let cb = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 33),
            StreamType::MediaSource,
            cb,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            None,
            Box::new(HeadlessGlContext),
        );

        let store = servo_media_player::cenc::ClearKeyStore::new();
        {
            let guard = player.lock().unwrap();
            guard.set_clearkey_store(Some(store.clone())).unwrap();
            guard
                .mse_add_source_buffer(1, "video/mp4; codecs=\"avc1.42E01E\"")
                .unwrap();
            guard.mse_set_duration(Some(2.0)).unwrap();
            let descs = track_descriptors_for(1, "video/mp4", &v_buf);
            guard.mse_commit_tracks(&descs).unwrap();
            guard
                .mse_push_data(1, (*v_buf.active_init_segment().unwrap().bytes).clone())
                .unwrap();
            for seg in v_buf.media_segments() {
                guard.mse_push_data(1, (*seg.bytes).clone()).unwrap();
            }
            guard.mse_end_of_stream(None).unwrap();
            guard.play().unwrap();
        }

        let mut saw_waiting = false;
        let wait_deadline = Instant::now() + Duration::from_secs(3);
        while !saw_waiting && Instant::now() < wait_deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(PlayerEvent::WaitingForKey) => saw_waiting = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected error before cbcs key: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(saw_waiting, "expected WaitingForKey before cbcs key");

        store.set_session_key("cbcs-session", kid, key);
        player.lock().unwrap().notify_keys_updated().unwrap();

        let mut saw_playing = false;
        let mut saw_eos = false;
        let mut meta_dims = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_playing && saw_eos && video_frames.load(Ordering::Relaxed) > 0) &&
            Instant::now() < deadline
        {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::MetadataUpdated(meta)) => {
                    meta_dims = Some((meta.width, meta.height));
                },
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::EndOfStream) => saw_eos = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected error during cbcs playback: {err}")
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }

        assert!(saw_playing, "expected Playing for cbcs fMP4");
        assert!(saw_eos, "expected deferred EndOfStream for cbcs fMP4");
        assert_eq!(meta_dims, Some((320, 240)), "expected 320x240 metadata");
        assert!(
            video_frames.load(Ordering::Relaxed) > 0,
            "expected decoded video frames for cbcs fMP4"
        );

        let _ = player.lock().unwrap().stop();
        drop(player);
        drop(backend);
    }

    #[test]
    fn progressive_servosrc_cenc_and_clear_playback() {
        let ffmpeg_path = std::path::Path::new("/usr/bin/ffmpeg");
        if !ffmpeg_path.exists() {
            return;
        }
        let backend = GStreamerBackend::init();

        let out = std::process::Command::new(ffmpeg_path)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=24",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "24",
                "-keyint_min",
                "24",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
                "-f",
                "mp4",
                "pipe:1",
            ])
            .output()
            .expect("invoke ffmpeg");
        assert!(out.status.success());
        let clear_bytes = out.stdout;

        let kid: [u8; 16] = [
            0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e,
            0x2f, 0x30,
        ];
        let key: [u8; 16] = [
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e,
            0x3f, 0x40,
        ];
        let constant_iv: [u8; 16] = [0x55; 16];
        let enc_bytes = servo_media_player::cenc::encrypt_clear_fmp4_to_cbcs(
            &clear_bytes,
            kid,
            key,
            constant_iv,
        )
        .unwrap();

        // 1. Progressive encrypted fMP4 via StreamType::Seekable (ServoSrc)
        let (tx, rx) = mpsc::channel::<PlayerEvent>();
        let cb = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();

        let video_frames = Arc::new(AtomicUsize::new(0));
        let player = backend.create_player(
            &ClientContextId::build(1, 34),
            StreamType::Seekable,
            cb,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: video_frames.clone(),
            }))),
            None,
            Box::new(HeadlessGlContext),
        );

        let store = servo_media_player::cenc::ClearKeyStore::new();
        {
            let guard = player.lock().unwrap();
            guard.set_download_buffering_enabled(true).unwrap();
            guard.set_seekable(false).unwrap();
            guard.set_input_size(enc_bytes.len() as u64).unwrap();
            guard.set_clearkey_store(Some(store.clone())).unwrap();
            for chunk in enc_bytes.chunks(512) {
                guard.push_data(chunk.to_vec()).unwrap();
            }
            guard.end_of_stream().unwrap();
            guard.play().unwrap();
        }

        let mut saw_encrypted = false;
        let mut saw_waiting = false;
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while !(saw_encrypted && saw_waiting) && Instant::now() < wait_deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(PlayerEvent::Encrypted {
                    init_data_type,
                    init_data,
                }) => {
                    assert_eq!(init_data_type, "cenc");
                    let kids =
                        servo_media_player::cenc::extract_kids_from_cenc_pssh(&init_data).unwrap();
                    assert!(kids.contains(&kid));
                    saw_encrypted = true;
                },
                Ok(PlayerEvent::WaitingForKey) => saw_waiting = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected error before key: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(saw_encrypted, "expected PlayerEvent::Encrypted from ServoSrc");
        assert!(saw_waiting, "expected PlayerEvent::WaitingForKey from ServoSrc");

        store.set_session_key("prog-s1", kid, key);
        player.lock().unwrap().notify_keys_updated().unwrap();

        let mut saw_playing = false;
        let mut saw_eos = false;
        let mut meta_dims = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(saw_playing && saw_eos && video_frames.load(Ordering::Relaxed) > 0) &&
            Instant::now() < deadline
        {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::MetadataUpdated(meta)) => {
                    meta_dims = Some((meta.width, meta.height));
                },
                Ok(PlayerEvent::StateChanged(PlaybackState::Playing)) => saw_playing = true,
                Ok(PlayerEvent::EndOfStream) => saw_eos = true,
                Ok(PlayerEvent::Error(err)) => {
                    panic!("unexpected error during progressive CENC playback: {err}")
                },
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(saw_playing, "expected Playing for progressive ServoSrc CENC");
        assert!(saw_eos, "expected EndOfStream for progressive ServoSrc CENC");
        assert_eq!(meta_dims, Some((320, 240)));
        assert!(video_frames.load(Ordering::Relaxed) > 0);
        let _ = player.lock().unwrap().stop();
        drop(player);

        // 2. Clear MP4 passthrough sanity check on ServoSrc (must NOT emit Encrypted and must play).
        let (tx2, rx2) = mpsc::channel::<PlayerEvent>();
        let cb2 = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx2.send(ev);
            }
        })
        .unwrap();
        let clear_frames = Arc::new(AtomicUsize::new(0));
        let clear_player = backend.create_player(
            &ClientContextId::build(1, 35),
            StreamType::Seekable,
            cb2,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: clear_frames.clone(),
            }))),
            None,
            Box::new(HeadlessGlContext),
        );
        {
            let guard = clear_player.lock().unwrap();
            guard.set_seekable(false).unwrap();
            guard.set_input_size(clear_bytes.len() as u64).unwrap();
            for chunk in clear_bytes.chunks(512) {
                guard.push_data(chunk.to_vec()).unwrap();
            }
            guard.end_of_stream().unwrap();
            guard.play().unwrap();
        }
        let mut clear_eos = false;
        let deadline2 = Instant::now() + Duration::from_secs(10);
        while !(clear_eos && clear_frames.load(Ordering::Relaxed) > 0) && Instant::now() < deadline2
        {
            match rx2.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::Encrypted { .. }) => {
                    panic!("clear MP4 must not emit PlayerEvent::Encrypted")
                },
                Ok(PlayerEvent::EndOfStream) => clear_eos = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected clear MP4 error: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(clear_eos);
        assert!(clear_frames.load(Ordering::Relaxed) > 0);
        let _ = clear_player.lock().unwrap().stop();
        drop(clear_player);

        // 3. Non-ISO-BMFF (WebM) immediate passthrough on ServoSrc
        let webm_bytes = generate_fmp4_bytes(
            "videotestsrc num-buffers=15 ! video/x-raw,width=64,height=64,framerate=15/1 ! videoconvert ! vp8enc ! webmmux ! appsink name=sink",
        );
        let (tx3, rx3) = mpsc::channel::<PlayerEvent>();
        let cb3 = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx3.send(ev);
            }
        })
        .unwrap();
        let webm_frames = Arc::new(AtomicUsize::new(0));
        let webm_player = backend.create_player(
            &ClientContextId::build(1, 36),
            StreamType::Seekable,
            cb3,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: webm_frames.clone(),
            }))),
            None,
            Box::new(HeadlessGlContext),
        );
        {
            let guard = webm_player.lock().unwrap();
            guard.set_seekable(false).unwrap();
            guard.set_input_size(webm_bytes.len() as u64).unwrap();
            for chunk in webm_bytes.chunks(256) {
                guard.push_data(chunk.to_vec()).unwrap();
            }
            guard.end_of_stream().unwrap();
            guard.play().unwrap();
        }
        let mut webm_eos = false;
        let deadline3 = Instant::now() + Duration::from_secs(10);
        while !(webm_eos && webm_frames.load(Ordering::Relaxed) > 0) && Instant::now() < deadline3
        {
            match rx3.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::Encrypted { .. }) => {
                    panic!("WebM must not emit PlayerEvent::Encrypted")
                },
                Ok(PlayerEvent::EndOfStream) => webm_eos = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected WebM error: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(webm_eos);
        assert!(webm_frames.load(Ordering::Relaxed) > 0);
        let _ = webm_player.lock().unwrap().stop();
        drop(webm_player);

        // 4. Non-faststart MP4 (`ftyp` + `mdat` + `moov`) via ServoSrc passthrough + SeekData
        let nf_path = std::env::temp_dir().join(format!(
            "rovelo_nonfaststart_{}.mp4",
            std::process::id()
        ));
        let nf_out = std::process::Command::new(ffmpeg_path)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=15",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                nf_path.to_str().unwrap(),
            ])
            .output()
            .expect("invoke ffmpeg for non-faststart mp4");
        assert!(nf_out.status.success());
        let nf_bytes = std::fs::read(&nf_path).expect("read non-faststart mp4");
        let _ = std::fs::remove_file(&nf_path);
        assert!(nf_bytes.windows(4).position(|w| w == b"mdat").unwrap() < nf_bytes.windows(4).position(|w| w == b"moov").unwrap());

        let (tx4, rx4) = mpsc::channel::<PlayerEvent>();
        let cb4 = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx4.send(ev);
            }
        })
        .unwrap();
        let nf_frames = Arc::new(AtomicUsize::new(0));
        let nf_player = backend.create_player(
            &ClientContextId::build(1, 38),
            StreamType::Seekable,
            cb4,
            Some(Arc::new(Mutex::new(CountingVideoRenderer {
                frames: nf_frames.clone(),
            }))),
            None,
            Box::new(HeadlessGlContext),
        );
        {
            let guard = nf_player.lock().unwrap();
            guard.set_seekable(true).unwrap();
            guard.set_input_size(nf_bytes.len() as u64).unwrap();
            for chunk in nf_bytes.chunks(512) {
                guard.push_data(chunk.to_vec()).unwrap();
            }
            guard.end_of_stream().unwrap();
            guard.play().unwrap();
        }
        let mut nf_eos = false;
        let deadline4 = Instant::now() + Duration::from_secs(10);
        while !(nf_eos && nf_frames.load(Ordering::Relaxed) > 0) && Instant::now() < deadline4 {
            match rx4.recv_timeout(Duration::from_millis(50)) {
                Ok(PlayerEvent::Encrypted { .. }) => {
                    panic!("non-faststart clear MP4 must not emit Encrypted")
                },
                Ok(PlayerEvent::SeekData(offset, lock)) => {
                    lock.unlock(true);
                    let guard = nf_player.lock().unwrap();
                    for chunk in nf_bytes[offset as usize..].chunks(512) {
                        guard.push_data(chunk.to_vec()).unwrap();
                    }
                    guard.end_of_stream().unwrap();
                },
                Ok(PlayerEvent::EndOfStream) => nf_eos = true,
                Ok(PlayerEvent::Error(err)) => panic!("unexpected non-faststart MP4 error: {err}"),
                Ok(_) => {},
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(err) => panic!("channel closed: {err}"),
            }
        }
        assert!(nf_eos);
        assert!(nf_frames.load(Ordering::Relaxed) > 0);
        let _ = nf_player.lock().unwrap().stop();
        drop(nf_player);
        drop(backend);
    }
}
