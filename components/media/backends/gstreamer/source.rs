/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use servo_base::generic_channel::GenericCallback;
use servo_media_player::PlayerEvent;
use servo_media_player::cenc::{
    self, CencDecryptError, ClearKeyStore, TrackEncryptionInfo,
};
use url::Url;

const MAX_SRC_QUEUE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB.

// Implementation sub-module of the GObject
mod imp {
    use std::sync::LazyLock;

    use super::*;

    macro_rules! inner_appsrc_proxy {
        ($fn_name:ident, $return_type:ty) => {
            pub fn $fn_name(&self) -> $return_type {
                self.appsrc.$fn_name()
            }
        };

        ($fn_name:ident, $arg1:ident, $arg1_type:ty, $return_type:ty) => {
            pub fn $fn_name(&self, $arg1: $arg1_type) -> $return_type {
                self.appsrc.$fn_name($arg1)
            }
        };
    }

    #[derive(Debug, Default)]
    struct Position {
        offset: u64,
        requested_offset: u64,
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    enum ProgressiveMode {
        #[default]
        Probing,
        IsoBmffInit,
        EncryptedFmp4,
        Passthrough,
    }

    enum PendingSegment {
        Clear(Vec<u8>),
        Encrypted(Vec<u8>),
    }

    #[derive(Default)]
    struct FilterStep {
        clear_chunks: Vec<Vec<u8>>,
        events: Vec<PlayerEvent>,
        error: Option<String>,
    }

    #[derive(Default)]
    struct ProgressiveCencState {
        mode: ProgressiveMode,
        raw_buffer: Vec<u8>,
        init_prefix: Vec<u8>,
        media_prefix: Vec<u8>,
        pending_moof: Option<Vec<u8>>,
        encryption_info: HashMap<u32, TrackEncryptionInfo>,
        clearkey_store: Option<ClearKeyStore>,
        pending_segments: VecDeque<PendingSegment>,
        waiting_for_key: bool,
        pending_eos: bool,
    }

    fn next_top_level_box(buf: &[u8]) -> Result<Option<(usize, [u8; 4])>, ()> {
        if buf.len() < 8 {
            return Ok(None);
        }
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let fourcc = [buf[4], buf[5], buf[6], buf[7]];
        let total_size = if size32 == 1 {
            if buf.len() < 16 {
                return Ok(None);
            }
            let largesize = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            if largesize < 16 {
                return Err(());
            }
            usize::try_from(largesize).map_err(|_| ())?
        } else if size32 == 0 {
            return Err(());
        } else {
            if size32 < 8 {
                return Err(());
            }
            size32 as usize
        };
        if buf.len() < total_size {
            return Ok(None);
        }
        Ok(Some((total_size, fourcc)))
    }

    fn extract_or_synthesize_cenc_pssh(
        init_bytes: &[u8],
        enc_map: &HashMap<u32, TrackEncryptionInfo>,
    ) -> Option<Vec<u8>> {
        let mut pssh = cenc::extract_cenc_pssh_boxes(init_bytes);
        if pssh.is_empty() {
            let mut kids: Vec<[u8; 16]> = Vec::new();
            for info in enc_map.values() {
                if info.default_is_protected && !kids.contains(&info.default_kid) {
                    kids.push(info.default_kid);
                }
            }
            if !kids.is_empty() {
                pssh = cenc::synthesize_clearkey_pssh_v1(&kids);
            }
        }
        if pssh.is_empty() {
            None
        } else {
            Some(pssh)
        }
    }

    fn non_empty_pssh(bytes: &[u8]) -> Option<Vec<u8>> {
        let pssh = cenc::extract_cenc_pssh_boxes(bytes);
        if pssh.is_empty() { None } else { Some(pssh) }
    }

    impl ProgressiveCencState {
        fn reset_for_seek(&mut self, offset: u64) {
            self.raw_buffer.clear();
            self.media_prefix.clear();
            self.pending_moof = None;
            self.pending_segments.clear();
            self.waiting_for_key = false;
            self.pending_eos = false;
            if offset == 0 {
                self.mode = ProgressiveMode::Probing;
                self.init_prefix.clear();
                self.encryption_info.clear();
            } else if !self.encryption_info.is_empty() {
                self.mode = ProgressiveMode::EncryptedFmp4;
            }
        }

        fn clear_buffers_for_stop(&mut self) {
            self.raw_buffer.clear();
            self.init_prefix.clear();
            self.media_prefix.clear();
            self.pending_moof = None;
            self.pending_segments.clear();
            self.waiting_for_key = false;
            self.pending_eos = false;
        }

        fn drain_into(&mut self, step: &mut FilterStep) {
            while let Some(front) = self.pending_segments.front() {
                match front {
                    PendingSegment::Clear(_) => {
                        if let Some(PendingSegment::Clear(bytes)) = self.pending_segments.pop_front()
                        {
                            step.clear_chunks.push(bytes);
                        }
                    },
                    PendingSegment::Encrypted(seg) => {
                        let Some(ref store) = self.clearkey_store else {
                            if !self.waiting_for_key {
                                self.waiting_for_key = true;
                                step.events.push(PlayerEvent::WaitingForKey);
                            }
                            break;
                        };
                        match cenc::decrypt_media_segment(seg, &self.encryption_info, store) {
                            Ok(clear_bytes) => {
                                self.pending_segments.pop_front();
                                self.waiting_for_key = false;
                                step.clear_chunks.push(clear_bytes);
                            },
                            Err(CencDecryptError::MissingKey(_)) => {
                                if !self.waiting_for_key {
                                    self.waiting_for_key = true;
                                    step.events.push(PlayerEvent::WaitingForKey);
                                }
                                break;
                            },
                            Err(CencDecryptError::InvalidData(msg)) => {
                                self.pending_segments.pop_front();
                                step.error = Some(format!("CENC decryption error: {msg}"));
                                break;
                            },
                        }
                    },
                }
            }
        }

        fn process_chunk(&mut self, data: Vec<u8>) -> FilterStep {
            let mut step = FilterStep::default();
            if data.is_empty() {
                return step;
            }
            if self.mode == ProgressiveMode::Passthrough &&
                self.pending_segments.is_empty() &&
                self.raw_buffer.is_empty()
            {
                step.clear_chunks.push(data);
                return step;
            }

            self.raw_buffer.extend_from_slice(&data);

            if self.mode == ProgressiveMode::Probing {
                if self.raw_buffer.len() < 8 {
                    return step;
                }
                let size32 = u32::from_be_bytes([
                    self.raw_buffer[0],
                    self.raw_buffer[1],
                    self.raw_buffer[2],
                    self.raw_buffer[3],
                ]);
                let fourcc = [
                    self.raw_buffer[4],
                    self.raw_buffer[5],
                    self.raw_buffer[6],
                    self.raw_buffer[7],
                ];
                let is_iso_bmff =
                    (size32 == 1 || size32 >= 8) && matches!(&fourcc, b"ftyp" | b"moov" | b"styp");
                if !is_iso_bmff {
                    self.mode = ProgressiveMode::Passthrough;
                    step.clear_chunks.push(std::mem::take(&mut self.raw_buffer));
                    return step;
                }
                self.mode = ProgressiveMode::IsoBmffInit;
            }

            let mut consumed = 0usize;
            while consumed < self.raw_buffer.len() {
                match self.mode {
                    ProgressiveMode::IsoBmffInit => {
                        let box_info = match next_top_level_box(&self.raw_buffer[consumed..]) {
                            Ok(Some(v)) => v,
                            Ok(None) => break,
                            Err(()) => {
                                let mut flush = std::mem::take(&mut self.init_prefix);
                                flush.extend_from_slice(&self.raw_buffer[consumed..]);
                                consumed = self.raw_buffer.len();
                                self.mode = ProgressiveMode::Passthrough;
                                if !flush.is_empty() {
                                    step.clear_chunks.push(flush);
                                }
                                break;
                            },
                        };
                        let (rel_end, fourcc) = box_info;
                        let box_bytes = &self.raw_buffer[consumed..consumed + rel_end];
                        consumed += rel_end;

                        match &fourcc {
                            b"moov" => {
                                let mut init_bytes = std::mem::take(&mut self.init_prefix);
                                init_bytes.extend_from_slice(box_bytes);
                                if let Some((clear_init, enc_map)) =
                                    cenc::rewrite_init_segment_to_clear(&init_bytes)
                                {
                                    if let Some(init_data) =
                                        extract_or_synthesize_cenc_pssh(&init_bytes, &enc_map)
                                    {
                                        step.events.push(PlayerEvent::Encrypted {
                                            init_data_type: "cenc".to_owned(),
                                            init_data,
                                        });
                                    }
                                    self.encryption_info = enc_map;
                                    self.mode = ProgressiveMode::EncryptedFmp4;
                                    self.pending_segments
                                        .push_back(PendingSegment::Clear(clear_init));
                                } else {
                                    self.mode = ProgressiveMode::Passthrough;
                                    step.clear_chunks.push(init_bytes);
                                    if consumed < self.raw_buffer.len() {
                                        step.clear_chunks
                                            .push(self.raw_buffer[consumed..].to_vec());
                                        consumed = self.raw_buffer.len();
                                    }
                                    break;
                                }
                            },
                            b"mdat" => {
                                let mut flush = std::mem::take(&mut self.init_prefix);
                                flush.extend_from_slice(box_bytes);
                                if consumed < self.raw_buffer.len() {
                                    flush.extend_from_slice(&self.raw_buffer[consumed..]);
                                    consumed = self.raw_buffer.len();
                                }
                                self.mode = ProgressiveMode::Passthrough;
                                step.clear_chunks.push(flush);
                                break;
                            },
                            _ => {
                                self.init_prefix.extend_from_slice(box_bytes);
                            },
                        }
                    },
                    ProgressiveMode::EncryptedFmp4 => {
                        let box_info = match next_top_level_box(&self.raw_buffer[consumed..]) {
                            Ok(Some(v)) => v,
                            Ok(None) => break,
                            Err(()) => {
                                if let Some(prev) = self.pending_moof.take() {
                                    self.pending_segments
                                        .push_back(PendingSegment::Encrypted(prev));
                                }
                                let mut rem = std::mem::take(&mut self.media_prefix);
                                rem.extend_from_slice(&self.raw_buffer[consumed..]);
                                consumed = self.raw_buffer.len();
                                self.mode = ProgressiveMode::Passthrough;
                                if !rem.is_empty() {
                                    self.pending_segments.push_back(PendingSegment::Clear(rem));
                                }
                                break;
                            },
                        };
                        let (rel_end, fourcc) = box_info;
                        let box_bytes = &self.raw_buffer[consumed..consumed + rel_end];
                        consumed += rel_end;

                        match &fourcc {
                            b"styp" | b"free" | b"skip" | b"emsg" | b"prft" | b"sidx"
                            | b"pssh" => {
                                self.media_prefix.extend_from_slice(box_bytes);
                            },
                            b"moof" => {
                                if let Some(prev) = self.pending_moof.take() {
                                    if let Some(init_data) = non_empty_pssh(&prev) {
                                        step.events.push(PlayerEvent::Encrypted {
                                            init_data_type: "cenc".to_owned(),
                                            init_data,
                                        });
                                    }
                                    self.pending_segments
                                        .push_back(PendingSegment::Encrypted(prev));
                                }
                                let mut seg = std::mem::take(&mut self.media_prefix);
                                seg.extend_from_slice(box_bytes);
                                self.pending_moof = Some(seg);
                            },
                            b"mdat" => {
                                if let Some(mut seg) = self.pending_moof.take() {
                                    seg.extend_from_slice(box_bytes);
                                    if let Some(init_data) = non_empty_pssh(&seg) {
                                        step.events.push(PlayerEvent::Encrypted {
                                            init_data_type: "cenc".to_owned(),
                                            init_data,
                                        });
                                    }
                                    self.pending_segments
                                        .push_back(PendingSegment::Encrypted(seg));
                                } else {
                                    let mut seg = std::mem::take(&mut self.media_prefix);
                                    seg.extend_from_slice(box_bytes);
                                    self.pending_segments.push_back(PendingSegment::Clear(seg));
                                }
                            },
                            _ => {
                                if let Some(prev) = self.pending_moof.take() {
                                    self.pending_segments
                                        .push_back(PendingSegment::Encrypted(prev));
                                }
                                if !self.media_prefix.is_empty() {
                                    let prefix = std::mem::take(&mut self.media_prefix);
                                    if let Some(init_data) = non_empty_pssh(&prefix) {
                                        step.events.push(PlayerEvent::Encrypted {
                                            init_data_type: "cenc".to_owned(),
                                            init_data,
                                        });
                                    }
                                    self.pending_segments
                                        .push_back(PendingSegment::Encrypted(prefix));
                                }
                                self.pending_segments
                                    .push_back(PendingSegment::Clear(box_bytes.to_vec()));
                            },
                        }
                    },
                    ProgressiveMode::Probing | ProgressiveMode::Passthrough => break,
                }
            }

            if consumed > 0 {
                self.raw_buffer.drain(..consumed);
            }
            self.drain_into(&mut step);
            step
        }

        fn on_eos(&mut self) -> (FilterStep, bool) {
            let mut step = FilterStep::default();
            match self.mode {
                ProgressiveMode::Probing => {
                    if !self.raw_buffer.is_empty() {
                        step.clear_chunks.push(std::mem::take(&mut self.raw_buffer));
                    }
                },
                ProgressiveMode::IsoBmffInit => {
                    let mut rem = std::mem::take(&mut self.init_prefix);
                    rem.extend_from_slice(&std::mem::take(&mut self.raw_buffer));
                    if !rem.is_empty() {
                        step.clear_chunks.push(rem);
                    }
                },
                ProgressiveMode::EncryptedFmp4 => {
                    if let Some(prev) = self.pending_moof.take() {
                        self.pending_segments
                            .push_back(PendingSegment::Encrypted(prev));
                    }
                    if !self.media_prefix.is_empty() {
                        let prefix = std::mem::take(&mut self.media_prefix);
                        self.pending_segments
                            .push_back(PendingSegment::Encrypted(prefix));
                    }
                    if !self.raw_buffer.is_empty() {
                        self.pending_segments
                            .push_back(PendingSegment::Clear(std::mem::take(&mut self.raw_buffer)));
                    }
                },
                ProgressiveMode::Passthrough => {
                    if !self.raw_buffer.is_empty() {
                        step.clear_chunks.push(std::mem::take(&mut self.raw_buffer));
                    }
                },
            }

            self.drain_into(&mut step);
            let send_eos = self.pending_segments.is_empty() && step.error.is_none();
            self.pending_eos = !send_eos;
            (step, send_eos)
        }
    }

    // The actual data structure that stores our values. This is not accessible
    // directly from the outside.
    pub struct ServoSrc {
        cat: gstreamer::DebugCategory,
        appsrc: gstreamer_app::AppSrc,
        srcpad: gstreamer::GhostPad,
        position: Mutex<Position>,
        seeking: AtomicBool,
        seekable: AtomicBool,
        size: Mutex<Option<i64>>,
        stopping: AtomicBool,
        observer: Mutex<Option<Arc<Mutex<GenericCallback<PlayerEvent>>>>>,
        push_serial: Mutex<()>,
        cenc: Mutex<ProgressiveCencState>,
    }

    impl ServoSrc {
        pub fn set_observer(&self, observer: Arc<Mutex<GenericCallback<PlayerEvent>>>) {
            if !self.stopping.load(Ordering::Acquire) {
                *self.observer.lock().unwrap() = Some(observer);
            }
        }

        pub fn prepare_stop(&self) {
            self.stopping.store(true, Ordering::Release);
            *self.observer.lock().unwrap() = None;
            self.cenc.lock().unwrap().clear_buffers_for_stop();
            self.appsrc
                .set_callbacks(gstreamer_app::AppSrcCallbacks::builder().build());
        }

        fn emit_events(&self, events: Vec<PlayerEvent>, error: Option<String>) {
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            if events.is_empty() && error.is_none() {
                return;
            }
            let cb = self.observer.lock().unwrap().clone();
            let Some(cb) = cb else {
                return;
            };
            let guard = cb.lock().unwrap();
            for event in events {
                let _ = guard.send(event);
            }
            if let Some(msg) = error {
                let _ = guard.send(PlayerEvent::Error(msg));
            }
        }

        pub fn set_size(&self, size: i64) {
            if self.seeking.load(Ordering::Relaxed) {
                // We ignore set_size requests if we are seeking.
                // The size value is temporarily stored so it
                // is properly set once we are done seeking.
                *self.size.lock().unwrap() = Some(size);
                return;
            }

            if self.appsrc.size() == -1 {
                self.appsrc.set_size(size);
            }
        }

        pub fn set_seekable(&self, seekable: bool) {
            self.seekable.store(seekable, Ordering::Relaxed);
        }

        pub fn set_seek_offset<O: IsA<gstreamer::Object>>(&self, parent: &O, offset: u64) -> bool {
            let mut pos = self.position.lock().unwrap();

            if pos.offset == offset || pos.requested_offset != 0 {
                false
            } else {
                self.seeking.store(true, Ordering::Relaxed);
                pos.requested_offset = offset;
                gstreamer::debug!(
                    self.cat,
                    obj = parent,
                    "seeking to offset: {}",
                    pos.requested_offset
                );

                true
            }
        }

        pub fn set_seek_done(&self) {
            if !self.seeking.load(Ordering::Acquire) {
                return;
            }
            let requested_offset = {
                let mut pos = self.position.lock().unwrap();
                let target = pos.requested_offset;
                pos.offset = target;
                pos.requested_offset = 0;
                target
            };

            self.cenc.lock().unwrap().reset_for_seek(requested_offset);

            if let Some(size) = self.size.lock().unwrap().take() &&
                self.appsrc.size() == -1
            {
                self.appsrc.set_size(size);
            }

            self.seeking.store(false, Ordering::Release);
        }

        fn push_clear_chunks<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
            chunks: Vec<Vec<u8>>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            if chunks.is_empty() {
                return Ok(gstreamer::FlowSuccess::Ok);
            }
            if self.seeking.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) {
                return Ok(gstreamer::FlowSuccess::Ok);
            }

            let mut pos = self.position.lock().unwrap(); // will block seeking
            let mut ret: Result<gstreamer::FlowSuccess, gstreamer::FlowError> =
                Ok(gstreamer::FlowSuccess::Ok);

            for data in chunks {
                if data.is_empty() {
                    continue;
                }
                let length = u64::try_from(data.len()).unwrap();
                let buffer_starting_offset = pos.offset;
                pos.offset += length;

                gstreamer::trace!(self.cat, obj = parent, "offset: {}", pos.offset);

                // set the stream size (in bytes) to current offset if
                // size is lesser than it
                if let Ok(size) = u64::try_from(self.appsrc.size()) &&
                    pos.offset > size
                {
                    gstreamer::debug!(
                        self.cat,
                        obj = parent,
                        "Updating internal size from {} to {}",
                        size,
                        pos.offset
                    );
                    let new_size = i64::try_from(pos.offset).unwrap();
                    self.appsrc.set_size(new_size);
                }

                // Split the received vec<> into buffers that are of a
                // size basesrc suggest. It is important not to push
                // buffers that are too large, otherwise incorrect
                // buffering messages can be sent from the pipeline
                let block_size = 4096u64;
                let num_blocks = (length as f64 / block_size as f64).ceil() as u64;

                gstreamer::log!(
                    self.cat,
                    obj = parent,
                    "Splitting the received vec into {} blocks",
                    num_blocks
                );

                for i in 0..num_blocks {
                    let start = usize::try_from(i * block_size).unwrap();
                    let size = usize::try_from(block_size.min(length - start as u64)).unwrap();
                    let end = start + size;

                    let buffer_offset = buffer_starting_offset + start as u64;
                    let buffer_offset_end = buffer_offset + size as u64;

                    let subdata = Vec::from(&data[start..end]);
                    let mut buffer = gstreamer::Buffer::from_slice(subdata);
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_offset(buffer_offset);
                        buffer.set_offset_end(buffer_offset_end);
                    }

                    if self.seeking.load(Ordering::Acquire) ||
                        self.stopping.load(Ordering::Acquire)
                    {
                        gstreamer::trace!(
                            self.cat,
                            obj = parent,
                            "stopping buffer appends due to seek"
                        );
                        return Ok(gstreamer::FlowSuccess::Ok);
                    }

                    gstreamer::trace!(self.cat, obj = parent, "Pushing buffer {:?}", buffer);

                    ret = self.appsrc.push_buffer(buffer);
                    match ret {
                        Ok(_) => (),
                        Err(gstreamer::FlowError::Eos) | Err(gstreamer::FlowError::Flushing) => {
                            ret = Ok(gstreamer::FlowSuccess::Ok)
                        },
                        Err(_) => return ret,
                    }
                }
            }

            ret
        }

        pub fn push_buffer<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
            data: Vec<u8>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            if self.seeking.load(Ordering::Acquire) {
                gstreamer::debug!(self.cat, obj = parent, "seek in progress, ignored data");
                return Ok(gstreamer::FlowSuccess::Ok);
            }
            if self.stopping.load(Ordering::Acquire) {
                return Ok(gstreamer::FlowSuccess::Ok);
            }

            let _serial = self.push_serial.lock().unwrap();
            if self.seeking.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) {
                return Ok(gstreamer::FlowSuccess::Ok);
            }

            let step = self.cenc.lock().unwrap().process_chunk(data);
            let has_error = step.error.is_some();
            self.emit_events(step.events, step.error);
            if has_error {
                return Err(gstreamer::FlowError::Error);
            }
            self.push_clear_chunks(parent, step.clear_chunks)
        }

        pub fn set_clearkey_store<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
            store: Option<ClearKeyStore>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            let _serial = self.push_serial.lock().unwrap();
            if self.stopping.load(Ordering::Acquire) {
                return Ok(gstreamer::FlowSuccess::Ok);
            }
            if self.seeking.load(Ordering::Acquire) {
                self.cenc.lock().unwrap().clearkey_store = store;
                return Ok(gstreamer::FlowSuccess::Ok);
            }
            let (step, send_eos) = {
                let mut cenc = self.cenc.lock().unwrap();
                cenc.clearkey_store = store;
                let mut step = FilterStep::default();
                cenc.drain_into(&mut step);
                let send_eos =
                    cenc.pending_eos && cenc.pending_segments.is_empty() && step.error.is_none();
                if send_eos {
                    cenc.pending_eos = false;
                }
                (step, send_eos)
            };

            let has_error = step.error.is_some();
            self.emit_events(step.events, step.error);
            if has_error {
                return Err(gstreamer::FlowError::Error);
            }
            let res = self.push_clear_chunks(parent, step.clear_chunks);
            if res.is_ok() && send_eos {
                return self.appsrc.end_of_stream();
            }
            res
        }

        pub fn notify_keys_updated<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            let _serial = self.push_serial.lock().unwrap();
            if self.seeking.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) {
                return Ok(gstreamer::FlowSuccess::Ok);
            }
            let (step, send_eos) = {
                let mut cenc = self.cenc.lock().unwrap();
                let mut step = FilterStep::default();
                cenc.drain_into(&mut step);
                let send_eos =
                    cenc.pending_eos && cenc.pending_segments.is_empty() && step.error.is_none();
                if send_eos {
                    cenc.pending_eos = false;
                }
                (step, send_eos)
            };

            let has_error = step.error.is_some();
            self.emit_events(step.events, step.error);
            if has_error {
                return Err(gstreamer::FlowError::Error);
            }
            let res = self.push_clear_chunks(parent, step.clear_chunks);
            if res.is_ok() && send_eos {
                return self.appsrc.end_of_stream();
            }
            res
        }

        pub fn push_end_of_stream<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            let _serial = self.push_serial.lock().unwrap();
            if self.stopping.load(Ordering::Acquire) {
                return self.appsrc.end_of_stream();
            }
            let (step, send_eos) = self.cenc.lock().unwrap().on_eos();
            let has_error = step.error.is_some();
            self.emit_events(step.events, step.error);
            if has_error {
                return Err(gstreamer::FlowError::Error);
            }
            let res = self.push_clear_chunks(parent, step.clear_chunks);
            if res.is_ok() && send_eos {
                return self.appsrc.end_of_stream();
            }
            res
        }

        inner_appsrc_proxy!(set_callbacks, callbacks, gstreamer_app::AppSrcCallbacks, ());

        fn query(&self, pad: &gstreamer::GhostPad, query: &mut gstreamer::QueryRef) -> bool {
            gstreamer::log!(self.cat, obj = pad, "Handling query {:?}", query);

            // In order to make buffering/downloading work as we want, apart from
            // setting the appropriate flags on the player playbin,
            // the source:
            //
            // 1. Announces seekability when the media element confirmed it.
            // 2. Assumes seekable = true as default.
            // 3. Keeps assuming bandwidth limited.
            // 4. set_seekable is called when range requests are supported or not.
            let ret = match query.view_mut() {
                gstreamer::QueryViewMut::Scheduling(ref mut q) => {
                    let seekability_flag = if self.seekable.load(Ordering::Relaxed) {
                        gstreamer::SchedulingFlags::SEEKABLE
                    } else {
                        gstreamer::SchedulingFlags::SEQUENTIAL
                    };
                    q.set(
                        seekability_flag | gstreamer::SchedulingFlags::BANDWIDTH_LIMITED,
                        1,
                        -1,
                        0,
                    );
                    q.add_scheduling_modes([gstreamer::PadMode::Push]);
                    true
                },
                _ => gstreamer::Pad::query_default(pad, Some(&*self.obj()), query),
            };

            if ret {
                gstreamer::log!(self.cat, obj = pad, "Handled query {:?}", query);
            } else {
                gstreamer::info!(self.cat, obj = pad, "Didn't handle query {:?}", query);
            }
            ret
        }
    }

    // Basic declaration of our type for the GObject type system
    #[glib::object_subclass]
    impl ObjectSubclass for ServoSrc {
        const NAME: &'static str = "ServoSrc";
        type Type = super::ServoSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        // Called once at the very beginning of instantiation of each instance and
        // creates the data structure that contains all our state
        fn with_class(klass: &Self::Class) -> Self {
            let app_src = gstreamer::ElementFactory::make("appsrc")
                .build()
                .map(|elem| elem.downcast::<gstreamer_app::AppSrc>().unwrap())
                .expect("Could not create appsrc element");

            let pad_templ = klass.pad_template("src").unwrap();
            let ghost_pad = gstreamer::GhostPad::builder_from_template(&pad_templ)
                .query_function(|pad, parent, query| {
                    ServoSrc::catch_panic_pad_function(
                        parent,
                        || false,
                        |servosrc| servosrc.query(pad, query),
                    )
                })
                .build();

            Self {
                cat: gstreamer::DebugCategory::new(
                    "servosrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo source"),
                ),
                appsrc: app_src,
                srcpad: ghost_pad,
                position: Mutex::new(Default::default()),
                seeking: AtomicBool::new(false),
                seekable: AtomicBool::new(true),
                size: Mutex::new(None),
                stopping: AtomicBool::new(false),
                observer: Mutex::new(None),
                push_serial: Mutex::new(()),
                cenc: Mutex::new(ProgressiveCencState::default()),
            }
        }
    }

    // The ObjectImpl trait provides the setters/getters for GObject properties.
    // Here we need to provide the values that are internally stored back to the
    // caller, or store whatever new value the caller is providing.
    //
    // This maps between the GObject properties and our internal storage of the
    // corresponding values of the properties.
    impl ObjectImpl for ServoSrc {
        // Called right after construction of a new instance
        fn constructed(&self) {
            // Call the parent class' ::constructed() implementation first
            self.parent_constructed();

            self.obj()
                .add(&self.appsrc)
                .expect("Could not add appsrc element to bin");

            let target_pad = self.appsrc.static_pad("src");
            self.srcpad.set_target(target_pad.as_ref()).unwrap();

            self.obj()
                .add_pad(&self.srcpad)
                .expect("Could not add source pad to bin");

            self.appsrc.set_caps(None::<&gstreamer::Caps>);
            self.appsrc.set_max_bytes(MAX_SRC_QUEUE_SIZE);
            self.appsrc.set_block(false);
            self.appsrc.set_format(gstreamer::Format::Bytes);
            self.appsrc
                .set_stream_type(gstreamer_app::AppStreamType::Seekable);

            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoSrc {}

    // Implementation of gstreamer::Element virtual methods
    impl ElementImpl for ServoSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Source",
                        "Source/Audio/Video",
                        "Feed player with media data",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                let caps = gstreamer::Caps::new_any();
                let src_pad_template = gstreamer::PadTemplate::new(
                    "src",
                    gstreamer::PadDirection::Src,
                    gstreamer::PadPresence::Always,
                    &caps,
                )
                .unwrap();

                vec![src_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    // Implementation of gstreamer::Bin virtual methods
    impl BinImpl for ServoSrc {}

    impl URIHandlerImpl for ServoSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servosrc"]
        }

        fn uri(&self) -> Option<String> {
            Some("servosrc://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri) &&
                uri.scheme() == "servosrc"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{:?}'", uri,).as_str(),
            ))
        }
    }
}

// Public part of the ServoSrc type. This behaves like a normal
// GObject binding
glib::wrapper! {
    pub struct ServoSrc(ObjectSubclass<imp::ServoSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoSrc {}
unsafe impl Sync for ServoSrc {}

impl ServoSrc {
    pub fn set_observer(&self, observer: Arc<Mutex<GenericCallback<PlayerEvent>>>) {
        self.imp().set_observer(observer);
    }

    pub fn prepare_stop(&self) {
        self.imp().prepare_stop();
    }

    pub fn set_clearkey_store(
        &self,
        store: Option<ClearKeyStore>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().set_clearkey_store(self, store)
    }

    pub fn notify_keys_updated(&self) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().notify_keys_updated(self)
    }

    pub fn set_size(&self, size: i64) {
        self.imp().set_size(size);
    }

    pub fn set_seekable(&self, seekable: bool) {
        self.imp().set_seekable(seekable);
    }

    pub fn set_seek_offset(&self, offset: u64) -> bool {
        self.imp().set_seek_offset(self, offset)
    }

    pub fn set_seek_done(&self) {
        self.imp().set_seek_done();
    }

    pub fn push_buffer(
        &self,
        data: Vec<u8>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().push_buffer(self, data)
    }

    pub fn push_end_of_stream(&self) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().push_end_of_stream(self)
    }

    pub fn set_callbacks(&self, callbacks: gstreamer_app::AppSrcCallbacks) {
        self.imp().set_callbacks(callbacks)
    }
}

// Registers the type for our element, and then registers in GStreamer
// under the name "servosrc" for being able to instantiate it via e.g.
// gstreamer::ElementFactory::make().
pub fn register_servo_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servosrc",
        gstreamer::Rank::NONE,
        ServoSrc::static_type(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servosrc_seek_recovery_and_spurious_seek_done_guard() {
        gstreamer::init().unwrap();
        register_servo_src().unwrap();

        let elem = gstreamer::Element::make_from_uri(
            gstreamer::URIType::Src,
            "servosrc://",
            Some("test-servosrc"),
        )
        .unwrap()
        .downcast::<ServoSrc>()
        .unwrap();

        // Build a minimal clear fMP4 and encrypt it to cbcs via cenc::encrypt_clear_fmp4_to_cbcs.
        let box_wrap = |fourcc: &[u8; 4], payload: &[u8]| -> Vec<u8> {
            let mut v = Vec::with_capacity(8 + payload.len());
            v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
            v.extend_from_slice(fourcc);
            v.extend_from_slice(payload);
            v
        };
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
        let moov = box_wrap(
            b"moov",
            &box_wrap(
                b"trak",
                &[
                    tkhd.as_slice(),
                    box_wrap(
                        b"mdia",
                        &box_wrap(b"minf", &box_wrap(b"stbl", &box_wrap(b"stsd", &stsd_p))),
                    )
                    .as_slice(),
                ]
                .concat(),
            ),
        );

        let mut nal = vec![0x65u8];
        nal.extend_from_slice(&[0x33; 32]);
        let mut sample = Vec::new();
        sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        sample.extend_from_slice(&nal);

        let mfhd = box_wrap(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        let tfhd = box_wrap(b"tfhd", &[0, 0x02, 0, 0, 0, 0, 0, 1]);
        let mut trun_p = vec![0, 0, 0x02, 0x01];
        trun_p.extend_from_slice(&1u32.to_be_bytes());
        let off_pos = trun_p.len();
        trun_p.extend_from_slice(&0i32.to_be_bytes());
        trun_p.extend_from_slice(&(sample.len() as u32).to_be_bytes());
        let trun_dummy = box_wrap(b"trun", &trun_p);
        let moof_sz = 8 + mfhd.len() + 8 + tfhd.len() + trun_dummy.len();
        trun_p[off_pos..off_pos + 4].copy_from_slice(&((moof_sz + 8) as i32).to_be_bytes());
        let trun = box_wrap(b"trun", &trun_p);
        let traf = box_wrap(b"traf", &[tfhd, trun].concat());
        let moof = box_wrap(b"moof", &[mfhd, traf].concat());
        let mdat = box_wrap(b"mdat", &sample);

        let mut clear_fmp4 = Vec::new();
        clear_fmp4.extend_from_slice(&ftyp);
        clear_fmp4.extend_from_slice(&moov);
        clear_fmp4.extend_from_slice(&moof);
        clear_fmp4.extend_from_slice(&mdat);

        let kid = [0x42; 16];
        let key = [0x24; 16];
        let iv = [0x12; 16];
        let enc_fmp4 = cenc::encrypt_clear_fmp4_to_cbcs(&clear_fmp4, kid, key, iv).unwrap();
        let moof_off = enc_fmp4.windows(4).position(|w| w == b"moof").unwrap() - 4;

        let (tx, rx) = std::sync::mpsc::channel::<PlayerEvent>();
        let cb = GenericCallback::new(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .unwrap();
        elem.set_observer(Arc::new(Mutex::new(cb)));

        let store = ClearKeyStore::new();
        store.set_session_key("s1", kid, key);
        elem.set_clearkey_store(Some(store)).unwrap();

        // Push init + fragment -> enters EncryptedFmp4 mode.
        elem.push_buffer(enc_fmp4[..moof_off].to_vec()).unwrap();

        // Spurious set_seek_done() when no seek was initiated must NOT reset offset or wipe encryption_info.
        elem.set_seek_done();
        elem.push_buffer(enc_fmp4[moof_off..].to_vec()).unwrap();

        // Seek to mid-box (corrupt header -> Passthrough), then seek back to valid moof_off > 0:
        // reset_for_seek(moof_off) must restore EncryptedFmp4 and still decrypt moof+mdat cleanly.
        assert!(elem.set_seek_offset((moof_off + 3) as u64));
        elem.set_seek_done();
        elem.push_buffer(vec![0, 0, 0, 2, b'x', b'x', b'x', b'x'])
            .unwrap();

        assert!(elem.set_seek_offset(moof_off as u64));
        // notify_keys_updated while seeking must be a safe no-op.
        elem.notify_keys_updated().unwrap();
        elem.set_seek_done();
        elem.push_buffer(enc_fmp4[moof_off..].to_vec()).unwrap();
        elem.push_end_of_stream().unwrap();

        let mut saw_encrypted = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                PlayerEvent::Encrypted { init_data_type, .. } => {
                    assert_eq!(init_data_type, "cenc");
                    saw_encrypted = true;
                },
                PlayerEvent::Error(err) => {
                    panic!("unexpected ServoSrc error during seek recovery: {err}");
                },
                _ => {},
            }
        }
        assert!(saw_encrypted, "expected PlayerEvent::Encrypted");
    }
}
