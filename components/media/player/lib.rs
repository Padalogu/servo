/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ops::Range;
use std::time::Duration;

extern crate servo_media_streams as streams;
extern crate servo_media_traits;

pub mod audio;
pub mod cenc;
pub mod context;
pub mod metadata;
pub mod mse;
pub mod video;

use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{self, GenericSender};
use servo_media_traits::MediaInstance;
use streams::registry::MediaStreamId;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PlaybackState {
    Stopped,
    Buffering,
    Paused,
    Playing,
}

#[derive(Debug, PartialEq)]
pub enum PlayerError {
    /// Backend specific error.
    Backend(String),
    /// Could not push buffer contents to the player.
    BufferPushFailed,
    /// The player cannot consume more data.
    EnoughData,
    /// Setting End Of Stream failed.
    EOSFailed,
    /// The media stream is not seekable.
    NonSeekableStream,
    /// Tried to seek out of range.
    SeekOutOfRange,
    /// Setting an audio or video stream failed.
    /// Possibly because the type of source is not PlayerSource::Stream.
    SetStreamFailed,
    // Setting an audio or video track failed.
    SetTrackFailed,
}

pub type SeekLockMsg = (bool, GenericSender<()>);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SeekLock {
    pub lock_channel: GenericSender<SeekLockMsg>,
}

impl SeekLock {
    pub fn unlock(&self, result: bool) {
        let (ack_sender, ack_recv) =
            generic_channel::channel::<()>().expect("Could not create IPC channel");
        self.lock_channel.send((result, ack_sender)).unwrap();
        ack_recv.recv().unwrap()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PlayerEvent {
    EndOfStream,
    /// The player has enough data. The client should stop pushing data into.
    EnoughData,
    Error(String),
    VideoFrameUpdated,
    MetadataUpdated(metadata::Metadata),
    // The `None` value means the duration is unknown, in which case this is likely a live stream.
    DurationChanged(Option<Duration>),
    /// The internal player queue is running out of data. The client should start
    /// pushing more data.
    NeedData,
    PositionChanged(f64),
    /// The player needs the data to perform a seek to the given offset in bytes.
    /// The next push_data should get the buffers from the new offset.
    /// The player will be blocked until the user unlocks it through
    /// the given SeekLock instance.
    /// This event is only received for seekable stream types.
    SeekData(u64, SeekLock),
    /// The player has performed a seek to the given time offset in seconds.
    SeekDone(f64),
    StateChanged(PlaybackState),
    /// MSE player requests media data starting at the given presentation time in seconds.
    MseSeek(f64),
    /// MSE player needs more data for the given SourceBuffer ID.
    MseNeedData(u32),
    /// MSE player has enough buffered data for the given SourceBuffer ID.
    MseEnoughData(u32),
    /// EME: initialization data encountered in the media container.
    Encrypted {
        init_data_type: String,
        init_data: Vec<u8>,
    },
    /// EME: player is waiting for a decryption key to continue playback.
    WaitingForKey,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MseEndOfStreamError {
    Network,
    Decode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum MseTrackKind {
    Audio,
    Video,
    Text,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MseTrackDescriptor {
    pub source_buffer_id: u32,
    pub track_id: u64,
    pub kind: MseTrackKind,
    pub mime_type: String,
    pub codec: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum StreamType {
    /// No seeking is supported in the stream, such as a live stream.
    Stream,
    /// The stream is seekable.
    Seekable,
    /// Media Source Extensions (MSE) segmented source.
    MediaSource,
}

pub trait Player: Send + MediaInstance {
    fn play(&self) -> Result<(), PlayerError>;
    fn pause(&self) -> Result<(), PlayerError>;
    fn paused(&self) -> bool;
    fn can_resume(&self) -> bool;
    fn stop(&self) -> Result<(), PlayerError>;
    fn seek(&self, time: f64) -> Result<(), PlayerError>;
    fn seekable(&self) -> Vec<Range<f64>>;
    fn set_mute(&self, muted: bool) -> Result<(), PlayerError>;
    fn muted(&self) -> bool;
    fn set_volume(&self, volume: f64) -> Result<(), PlayerError>;
    fn volume(&self) -> f64;
    fn set_input_size(&self, size: u64) -> Result<(), PlayerError>;
    fn set_seekable(&self, seekable: bool) -> Result<(), PlayerError>;
    fn set_download_buffering_enabled(&self, enabled: bool) -> Result<(), PlayerError>;
    fn set_playback_rate(&self, playback_rate: f64) -> Result<(), PlayerError>;
    fn playback_rate(&self) -> f64;
    fn push_data(&self, data: Vec<u8>) -> Result<(), PlayerError>;
    fn end_of_stream(&self) -> Result<(), PlayerError>;
    /// Get the list of time ranges in seconds that have been buffered.
    fn buffered(&self) -> Vec<Range<f64>>;
    /// Set the stream to be played by the player.
    /// Only a single stream of the same type (audio or video) can be set.
    /// Subsequent calls with a stream of the same type will override the previously
    /// set stream.
    /// This method requires the player to be constructed with StreamType::Stream.
    /// It is important to give the correct value of `only_stream` indicating
    /// that the audio or video stream being set is the only one expected.
    /// Subsequent calls to `set_stream` after the `only_stream` flag has been
    /// set to true will fail.
    fn set_stream(&self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError>;
    /// If player's rendering draws using GL textures
    fn render_use_gl(&self) -> bool;
    fn set_audio_track(&self, stream_index: i32, enabled: bool) -> Result<(), PlayerError>;
    fn set_video_track(&self, stream_index: i32, enabled: bool) -> Result<(), PlayerError>;

    /// Register a new MSE SourceBuffer with the player.
    fn mse_add_source_buffer(&self, _id: u32, _mime_type: &str) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Remove an MSE SourceBuffer from the player.
    fn mse_remove_source_buffer(&self, _id: u32) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Push container segment bytes for the given MSE SourceBuffer ID.
    fn mse_push_data(&self, _source_buffer_id: u32, _data: Vec<u8>) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Flush queued data for the given MSE SourceBuffer ID (e.g., on seek or remove).
    fn mse_flush_track(&self, _source_buffer_id: u32) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Signal MSE end-of-stream with an optional error.
    fn mse_end_of_stream(&self, _error: Option<MseEndOfStreamError>) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Update the MSE media duration in seconds (`None` for unknown/live).
    fn mse_set_duration(&self, _duration: Option<f64>) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Set the MSE live seekable range in seconds.
    fn mse_set_live_seekable_range(&self, _start: f64, _end: f64) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Commit parsed MSE track descriptors so playback/demuxing can transition to ready.
    fn mse_commit_tracks(&self, _tracks: &[MseTrackDescriptor]) -> Result<(), PlayerError> {
        Err(PlayerError::Backend("MSE not supported".into()))
    }

    /// Attach or detach a ClearKey decryption key store for EME playback.
    fn set_clearkey_store(&self, _store: Option<cenc::ClearKeyStore>) -> Result<(), PlayerError> {
        Ok(())
    }

    /// Notify the player that new decryption keys were added to the ClearKeyStore.
    fn notify_keys_updated(&self) -> Result<(), PlayerError> {
        Ok(())
    }
}
