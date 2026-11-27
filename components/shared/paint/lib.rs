/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The interface to the `paint` crate, which helps to break dependency cycles.

use std::collections::HashMap;
use std::fmt::{Debug, Error, Formatter};

use crossbeam_channel::Sender;
use embedder_traits::{AnimationState, EventLoopWaker};
use euclid::{Rect, Scale, Size2D};
use log::warn;
use malloc_size_of_derive::MallocSizeOf;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use servo_base::Epoch;
use servo_base::id::{PainterId, PipelineId, WebViewId};
use smallvec::SmallVec;
use strum::IntoStaticStr;
use style_traits::CSSPixel;
use surfman::{Adapter, Connection};
use webrender_api::{DocumentId, FontInstancePlatformOptions, FontVariation};

pub mod display_list;
pub mod rendering_context;
pub mod viewport_description;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bitflags::bitflags;
use display_list::PaintDisplayListInfo;
use embedder_traits::ScreenGeometry;
use euclid::default::Size2D as UntypedSize2D;
use profile_traits::mem::{OpaqueSender, ReportsChan};
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{
    self, GenericCallback, GenericReceiver, GenericSender, GenericSharedMemory, SendError,
};
pub use webrender_api::ExternalImageSource;
use webrender_api::units::{DevicePixel, LayoutVector2D, TexelRect};
use webrender_api::{
    BuiltDisplayList, BuiltDisplayListDescriptor, ExternalImage, ExternalImageData,
    ExternalImageHandler, ExternalImageId, ExternalScrollId, FontInstanceFlags, FontInstanceKey,
    FontKey, ImageData, ImageDescriptor, ImageKey, NativeFontHandle,
    PipelineId as WebRenderPipelineId,
};

use crate::viewport_description::ViewportDescription;

/// Sends messages to `Paint`.
#[derive(Clone)]
pub struct PaintProxy {
    pub sender: Sender<Result<PaintMessage, SendError>>,
    /// Access to [`Self::sender`] that is possible to send across an IPC
    /// channel. These messages are routed via the router thread to
    /// [`Self::sender`].
    pub cross_process_paint_api: CrossProcessPaintApi,
    pub event_loop_waker: Box<dyn EventLoopWaker>,
}

impl OpaqueSender<PaintMessage> for PaintProxy {
    fn send(&self, message: PaintMessage) {
        PaintProxy::send(self, message)
    }
}

impl PaintProxy {
    pub fn send(&self, msg: PaintMessage) {
        self.route_msg(Ok(msg))
    }

    /// Helper method to route a deserialized IPC message to the receiver.
    ///
    /// This method is a temporary solution, and will be removed when migrating
    /// to `GenericChannel`.
    pub fn route_msg(&self, msg: Result<PaintMessage, SendError>) {
        if let Err(err) = self.sender.send(msg) {
            warn!("Failed to send response ({:?}).", err);
        }
        self.event_loop_waker.wake();
    }
}

/// Messages from (or via) the constellation thread to `Paint`.
#[derive(Deserialize, IntoStaticStr, Serialize)]
pub enum PaintMessage {
    /// Alerts `Paint` that the given pipeline has changed whether it is running animations.
    ChangeRunningAnimationsState(WebViewId, PipelineId, AnimationState),
    /// Updates the frame tree for the given webview.
    SetFrameTreeForWebView(WebViewId, SendableFrameTree),
    /// Set whether to use less resources by stopping animations.
    SetThrottled(WebViewId, PipelineId, bool),
    /// WebRender has produced a new frame. This message informs `Paint` that
    /// the frame is ready. It contains a bool to indicate if it needs to composite, the
    /// `DocumentId` of the new frame and the `PainterId` of the associated painter.
    NewWebRenderFrameReady(PainterId, DocumentId, bool),
    /// Script or the Constellation is notifying the renderer that a Pipeline has finished
    /// shutting down. The renderer will not discard the Pipeline until both report that
    /// they have fully shut it down, to avoid recreating it due to any subsequent
    /// messages.
    PipelineExited(WebViewId, PipelineId, PipelineExitSource),
    /// Inform WebRender of the existence of this pipeline.
    SendInitialTransaction(WebViewId, WebRenderPipelineId),
    /// Scroll the given node ([`ExternalScrollId`]) by the provided delta. This
    /// will only adjust the node's scroll position and will *not* do panning in
    /// the pinch zoom viewport.
    ScrollNodeByDelta(
        WebViewId,
        WebRenderPipelineId,
        LayoutVector2D,
        ExternalScrollId,
    ),
    /// Scroll the WebView's viewport by the given delta. This will also do panning
    /// in the pinch zoom viewport if possible and the remaining delta will be used
    /// to scroll the root layer.
    ScrollViewportByDelta(WebViewId, LayoutVector2D),
    /// Update the rendering epoch of the given `Pipeline`.
    UpdateEpoch {
        /// The [`WebViewId`] that this display list belongs to.
        webview_id: WebViewId,
        /// The [`PipelineId`] of the `Pipeline` to update.
        pipeline_id: PipelineId,
        /// The new [`Epoch`] value.
        epoch: Epoch,
    },
    /// Inform WebRender of a new display list for the given pipeline.
    SendDisplayList {
        /// The [`WebViewId`] that this display list belongs to.
        webview_id: WebViewId,
        /// A descriptor of this display list used to construct this display list from raw data.
        display_list_descriptor: BuiltDisplayListDescriptor,
        /// A [`GenericReceiver`] used to send the [`PaintDisplayListInfo`].
        display_list_info_receiver: GenericReceiver<PaintDisplayListInfo>,
        /// A [`GenericReceiver`] used to send the serialized  version of `DisplayListPayload.
        display_list_data_receiver: GenericReceiver<SerializableDisplayListPayload>,
    },
    /// Ask the renderer to generate a frame for the current set of display lists
    /// from the given `PainterId`s that have been sent to the renderer.
    GenerateFrame(Vec<PainterId>),
    /// Create a new image key. The result will be returned via the
    /// provided channel sender.
    GenerateImageKey(WebViewId, GenericSender<ImageKey>),
    /// The same as the above but it will be forwarded to the pipeline instead
    /// of send via a channel.
    GenerateImageKeysForPipeline(WebViewId, PipelineId),
    /// Perform a resource update operation.
    UpdateImages(PainterId, SmallVec<[ImageUpdate; 1]>),
    /// Perform a video image resource update operation with an optional
    /// backpressure acknowledgment callback reporting drained raw update bytes.
    UpdateVideoImages(
        PainterId,
        SmallVec<[ImageUpdate; 1]>,
        Option<GenericCallback<usize>>,
    ),
    /// Pause all pipeline display list processing for the given pipeline until the
    /// following image updates have been received. This is used to ensure that canvas
    /// elements have had a chance to update their rendering and send the image update to
    /// the renderer before their associated display list is actually displayed.
    DelayNewFrameForCanvas(WebViewId, PipelineId, Epoch, Vec<ImageKey>),

    /// Generate a new batch of font keys which can be used to allocate
    /// keys asynchronously.
    GenerateFontKeys(
        usize,
        usize,
        GenericSender<(Vec<FontKey>, Vec<FontInstanceKey>)>,
        PainterId,
    ),
    /// Add a font with the given data and font key.
    AddFont(PainterId, FontKey, Arc<GenericSharedMemory>, u32),
    /// Add a system font with the given font key and handle.
    AddSystemFont(PainterId, FontKey, NativeFontHandle),
    /// Add an instance of a font with the given instance key.
    AddFontInstance(
        PainterId,
        FontInstanceKey,
        FontKey,
        f32,
        FontInstanceFlags,
        FontInstancePlatformOptions,
        Vec<FontVariation>,
    ),
    /// Remove the given font resources from our WebRender instance.
    RemoveFonts(PainterId, Vec<FontKey>, Vec<FontInstanceKey>),
    /// Measure the current memory usage associated with `Paint`.
    /// The report must be sent on the provided channel once it's complete.
    CollectMemoryReport(ReportsChan),
    /// A top-level frame has parsed a viewport metatag and is sending the new constraints.
    Viewport(WebViewId, ViewportDescription),
    /// Let `Paint` know that the given WebView is ready to have a screenshot taken
    /// after the given pipeline's epochs have been rendered.
    ScreenshotReadinessReponse(WebViewId, FxHashMap<PipelineId, Epoch>),
}

impl Debug for PaintMessage {
    fn fmt(&self, formatter: &mut Formatter) -> Result<(), Error> {
        let string: &'static str = self.into();
        write!(formatter, "{string}")
    }
}

#[derive(Deserialize, Serialize)]
pub struct SendableFrameTree {
    pub pipeline: CompositionPipeline,
    pub children: Vec<SendableFrameTree>,
}

/// The subset of the pipeline that is needed for layer composition.
#[derive(Clone, Deserialize, Serialize)]
pub struct CompositionPipeline {
    pub id: PipelineId,
    pub webview_id: WebViewId,
}

/// A serializable version of `DisplayListPayload`.
#[derive(Serialize, Deserialize)]
pub struct SerializableDisplayListPayload {
    /// Serde encoded bytes of the display list' `DisplayItems` and their supporting data.
    #[serde(with = "serde_bytes")]
    pub items_data: Vec<u8>,

    #[serde(with = "serde_bytes")]
    pub spatial_tree: Vec<u8>,
}

pub const MAX_PENDING_RAW_IMAGE_UPDATE_BYTES: usize = 32 * 1024 * 1024;
static IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE: AtomicBool = AtomicBool::new(false);
static PENDING_RAW_IMAGE_UPDATE_BYTES: AtomicUsize = AtomicUsize::new(0);

pub fn enable_in_process_raw_image_budget() {
    IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.store(true, Ordering::Release);
}

pub fn pending_raw_image_update_bytes() -> usize {
    PENDING_RAW_IMAGE_UPDATE_BYTES.load(Ordering::Acquire)
}

pub fn raw_image_update_budget_exceeded() -> bool {
    IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.load(Ordering::Acquire) &&
        PENDING_RAW_IMAGE_UPDATE_BYTES.load(Ordering::Acquire) >=
            MAX_PENDING_RAW_IMAGE_UPDATE_BYTES
}

pub fn count_raw_image_update_bytes(updates: &[ImageUpdate]) -> usize {
    updates
        .iter()
        .filter_map(|update| match update {
            ImageUpdate::UpdateImage(_, _, SerializableImageData::Raw(shm), _) => Some(shm.len()),
            _ => None,
        })
        .sum()
}

pub fn release_raw_image_update_bytes(bytes: usize) {
    if bytes == 0 {
        return;
    }
    let _ = PENDING_RAW_IMAGE_UPDATE_BYTES.try_update(
        Ordering::AcqRel,
        Ordering::Acquire,
        |current| Some(current.saturating_sub(bytes)),
    );
}

fn try_reserve_raw_image_update_bytes(bytes: usize) -> bool {
    if !IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.load(Ordering::Acquire) || bytes == 0 {
        return true;
    }
    PENDING_RAW_IMAGE_UPDATE_BYTES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            let next = current.saturating_add(bytes);
            if current == 0 || next <= MAX_PENDING_RAW_IMAGE_UPDATE_BYTES {
                Some(next)
            } else {
                None
            }
        })
        .is_ok()
}

fn filter_raw_image_updates_for_in_process_budget(
    updates: &mut SmallVec<[ImageUpdate; 1]>,
) -> (usize, usize) {
    if !IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.load(Ordering::Acquire) {
        return (0, 0);
    }
    let mut reserved_bytes = 0usize;
    let mut dropped_bytes = 0usize;
    updates.retain(|update| match update {
        ImageUpdate::UpdateImage(_, _, SerializableImageData::Raw(shm), _) => {
            let bytes = shm.len();
            if try_reserve_raw_image_update_bytes(bytes) {
                reserved_bytes = reserved_bytes.saturating_add(bytes);
                true
            } else {
                dropped_bytes = dropped_bytes.saturating_add(bytes);
                false
            }
        },
        _ => true,
    });
    (reserved_bytes, dropped_bytes)
}

/// A mechanism to send messages from ScriptThread to the parent process' WebRender instance.
#[derive(Clone, Deserialize, MallocSizeOf, Serialize)]
pub struct CrossProcessPaintApi(GenericCallback<PaintMessage>);

impl CrossProcessPaintApi {
    /// Create a new [`CrossProcessPaintApi`] struct.
    pub fn new(callback: GenericCallback<PaintMessage>) -> Self {
        CrossProcessPaintApi(callback)
    }

    /// Create a new [`CrossProcessPaintApi`] struct that does not have a listener on the other
    /// end to use for unit testing.
    pub fn dummy() -> Self {
        Self::dummy_with_callback(None)
    }

    /// Create a new [`CrossProcessPaintApi`] struct for unit testing with an optional callback
    /// that can respond to `PaintMessage`s.
    pub fn dummy_with_callback(
        callback: Option<Box<dyn Fn(PaintMessage) + Send + 'static>>,
    ) -> Self {
        let callback = GenericCallback::new(move |msg| {
            if let Some(ref handler) = callback &&
                let Ok(paint_message) = msg
            {
                handler(paint_message);
            }
        })
        .unwrap();
        Self(callback)
    }

    /// Inform WebRender of the existence of this pipeline.
    pub fn send_initial_transaction(&self, webview_id: WebViewId, pipeline: WebRenderPipelineId) {
        if let Err(e) = self
            .0
            .send(PaintMessage::SendInitialTransaction(webview_id, pipeline))
        {
            warn!("Error sending initial transaction: {}", e);
        }
    }

    /// Scroll the given node ([`ExternalScrollId`]) by the provided delta. This
    /// will only adjust the node's scroll position and will *not* do panning in
    /// the pinch zoom viewport.
    pub fn scroll_node_by_delta(
        &self,
        webview_id: WebViewId,
        pipeline_id: WebRenderPipelineId,
        delta: LayoutVector2D,
        scroll_id: ExternalScrollId,
    ) {
        if let Err(error) = self.0.send(PaintMessage::ScrollNodeByDelta(
            webview_id,
            pipeline_id,
            delta,
            scroll_id,
        )) {
            warn!("Error scrolling node: {error}");
        }
    }

    /// Scroll the WebView's viewport by the given delta. This will also do panning
    /// in the pinch zoom viewport if possible and the remaining delta will be used
    /// to scroll the root layer.
    ///
    /// Note the value provided here is in `DeviceIndependentPixels` and will first be
    /// converted to `DevicePixels` by the renderer.
    pub fn scroll_viewport_by_delta(&self, webview_id: WebViewId, delta: LayoutVector2D) {
        if let Err(error) = self
            .0
            .send(PaintMessage::ScrollViewportByDelta(webview_id, delta))
        {
            warn!("Error scroll viewport: {error}");
        }
    }

    pub fn delay_new_frame_for_canvas(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        canvas_epoch: Epoch,
        image_keys: Vec<ImageKey>,
    ) {
        if let Err(error) = self.0.send(PaintMessage::DelayNewFrameForCanvas(
            webview_id,
            pipeline_id,
            canvas_epoch,
            image_keys,
        )) {
            warn!("Error delaying frames for canvas image updates {error:?}");
        }
    }

    /// Inform the renderer that the rendering epoch has advanced. This typically happens after
    /// a new display list is sent and/or canvas and animated images are updated.
    pub fn update_epoch(&self, webview_id: WebViewId, pipeline_id: PipelineId, epoch: Epoch) {
        if let Err(error) = self.0.send(PaintMessage::UpdateEpoch {
            webview_id,
            pipeline_id,
            epoch,
        }) {
            warn!("Error updating epoch for pipeline: {error:?}");
        }
    }

    /// Inform WebRender of a new display list for the given pipeline.
    /// We send the `PaintDisplayListInfo` and `DisplayListPayload` separately to not overwhelm
    /// the ipc_channel (see <https://github.com/servo/servo/pull/36484>)
    #[servo_tracing::instrument(skip_all)]
    pub fn send_display_list(
        &self,
        webview_id: WebViewId,
        display_list_info: &PaintDisplayListInfo,
        list: BuiltDisplayList,
    ) {
        let (display_list_data, display_list_descriptor) = list.into_data();
        let (display_list_data_sender, display_list_data_receiver) =
            generic_channel::channel().unwrap();
        let (display_list_info_sender, display_list_info_receiver) =
            generic_channel::channel().unwrap();
        if let Err(e) = self.0.send(PaintMessage::SendDisplayList {
            webview_id,
            display_list_descriptor,
            display_list_info_receiver,
            display_list_data_receiver,
        }) {
            warn!("Error sending display list: {}", e);
        }

        if let Err(error) = display_list_info_sender.send(display_list_info.clone()) {
            warn!("Error sending display list info: {error}. Not sending the rest");
            return;
        }
        let display_list_data = SerializableDisplayListPayload {
            items_data: display_list_data.items_data,
            spatial_tree: display_list_data.spatial_tree,
        };

        if let Err(error) = display_list_data_sender.send(display_list_data) {
            warn!("Error sending display list: {error}");
        }
    }

    /// Ask the Servo renderer to generate a new frame after having new display lists.
    pub fn generate_frame(&self, painter_ids: Vec<PainterId>) {
        if let Err(error) = self.0.send(PaintMessage::GenerateFrame(painter_ids)) {
            warn!("Error generating frame: {error}");
        }
    }

    /// Create a new image key. Blocks until the key is available.
    pub fn generate_image_key_blocking(&self, webview_id: WebViewId) -> Option<ImageKey> {
        let (sender, receiver) = generic_channel::channel().unwrap();
        self.0
            .send(PaintMessage::GenerateImageKey(webview_id, sender))
            .ok()?;
        receiver.recv().ok()
    }

    /// Sends a message to `Paint` for creating new image keys.
    /// `Paint` will then send a batch of keys over the constellation to the script_thread
    /// and the appropriate pipeline.
    pub fn generate_image_key_async(&self, webview_id: WebViewId, pipeline_id: PipelineId) {
        if let Err(e) = self.0.send(PaintMessage::GenerateImageKeysForPipeline(
            webview_id,
            pipeline_id,
        )) {
            warn!("Could not send image keys to Paint {}", e);
        }
    }

    pub fn add_image(
        &self,
        key: ImageKey,
        descriptor: ImageDescriptor,
        data: SerializableImageData,
        is_animated_image: bool,
    ) {
        self.update_images(
            key.into(),
            [ImageUpdate::AddImage(
                key,
                descriptor,
                data,
                is_animated_image,
            )]
            .into(),
        );
    }

    pub fn update_image(
        &self,
        key: ImageKey,
        descriptor: ImageDescriptor,
        data: SerializableImageData,
        epoch: Option<Epoch>,
    ) {
        self.update_images(
            key.into(),
            [ImageUpdate::UpdateImage(key, descriptor, data, epoch)].into(),
        );
    }

    pub fn delete_image(&self, key: ImageKey) {
        self.update_images(key.into(), [ImageUpdate::DeleteImage(key)].into());
    }

    /// Perform an image resource update operation.
    pub fn update_images(&self, painter_id: PainterId, mut updates: SmallVec<[ImageUpdate; 1]>) {
        let (reserved_bytes, _) = filter_raw_image_updates_for_in_process_budget(&mut updates);
        if updates.is_empty() {
            return;
        }
        if let Err(e) = self.0.send(PaintMessage::UpdateImages(painter_id, updates)) {
            release_raw_image_update_bytes(reserved_bytes);
            warn!("error sending image updates: {}", e);
        }
    }

    /// Perform a video image resource update operation with an optional backpressure
    /// acknowledgment callback invoked when `Paint` drains the message.
    ///
    /// Returns `true` if the message was sent without dropping any raw `UpdateImage`
    /// updates against the in-process budget. `AddImage` and `DeleteImage` updates are
    /// never dropped.
    pub fn update_video_images(
        &self,
        painter_id: PainterId,
        mut updates: SmallVec<[ImageUpdate; 1]>,
        ack: Option<GenericCallback<usize>>,
    ) -> bool {
        let (reserved_bytes, dropped_bytes) =
            filter_raw_image_updates_for_in_process_budget(&mut updates);
        if updates.is_empty() {
            return false;
        }
        if let Err(e) = self
            .0
            .send(PaintMessage::UpdateVideoImages(painter_id, updates, ack))
        {
            release_raw_image_update_bytes(reserved_bytes);
            warn!("error sending video image updates: {}", e);
            return false;
        }
        dropped_bytes == 0
    }

    pub fn remove_unused_font_resources(
        &self,
        painter_id: PainterId,
        keys: Vec<FontKey>,
        instance_keys: Vec<FontInstanceKey>,
    ) {
        if keys.is_empty() && instance_keys.is_empty() {
            return;
        }
        let _ = self
            .0
            .send(PaintMessage::RemoveFonts(painter_id, keys, instance_keys));
    }

    pub fn add_font_instance(
        &self,
        font_instance_key: FontInstanceKey,
        font_key: FontKey,
        size: f32,
        flags: FontInstanceFlags,
        options: FontInstancePlatformOptions,
        variations: Vec<FontVariation>,
    ) {
        let _x = self.0.send(PaintMessage::AddFontInstance(
            font_key.into(),
            font_instance_key,
            font_key,
            size,
            flags,
            options,
            variations,
        ));
    }

    pub fn add_font(&self, font_key: FontKey, data: Arc<GenericSharedMemory>, index: u32) {
        let _ = self.0.send(PaintMessage::AddFont(
            font_key.into(),
            font_key,
            data,
            index,
        ));
    }

    pub fn add_system_font(&self, font_key: FontKey, handle: NativeFontHandle) {
        let _ = self.0.send(PaintMessage::AddSystemFont(
            font_key.into(),
            font_key,
            handle,
        ));
    }

    pub fn fetch_font_keys(
        &self,
        number_of_font_keys: usize,
        number_of_font_instance_keys: usize,
        painter_id: PainterId,
    ) -> (Vec<FontKey>, Vec<FontInstanceKey>) {
        let (sender, receiver) = generic_channel::channel().expect("Could not create IPC channel");
        let _ = self.0.send(PaintMessage::GenerateFontKeys(
            number_of_font_keys,
            number_of_font_instance_keys,
            sender,
            painter_id,
        ));
        receiver.recv().unwrap()
    }

    pub fn viewport(&self, webview_id: WebViewId, description: ViewportDescription) {
        let _ = self.0.send(PaintMessage::Viewport(webview_id, description));
    }

    pub fn pipeline_exited(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        source: PipelineExitSource,
    ) {
        let _ = self.0.send(PaintMessage::PipelineExited(
            webview_id,
            pipeline_id,
            source,
        ));
    }
}

#[derive(Clone)]
pub struct PainterSurfmanDetails {
    pub connection: Connection,
    pub adapter: Adapter,
}

#[derive(Clone, Default)]
pub struct PainterSurfmanDetailsMap(Arc<Mutex<HashMap<PainterId, PainterSurfmanDetails>>>);

impl PainterSurfmanDetailsMap {
    pub fn get(&self, painter_id: PainterId) -> Option<PainterSurfmanDetails> {
        let map = self.0.lock().expect("poisoned");
        map.get(&painter_id).cloned()
    }

    pub fn insert(&self, painter_id: PainterId, details: PainterSurfmanDetails) {
        let mut map = self.0.lock().expect("poisoned");
        let existing = map.insert(painter_id, details);
        assert!(existing.is_none())
    }

    pub fn remove(&self, painter_id: PainterId) {
        let mut map = self.0.lock().expect("poisoned");
        map.remove(&painter_id);
    }
}

/// This trait is used as a bridge between the different GL clients
/// in Servo that handles WebRender ExternalImages and the WebRender
/// ExternalImageHandler API.
//
/// This trait is used to notify lock/unlock messages and get the
/// required info that WR needs.
pub trait WebRenderExternalImageApi {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, UntypedSize2D<i32>);
    fn unlock(&mut self, id: u64);
}

/// Type of WebRender External Image Handler.
#[derive(Clone, Copy)]
pub enum WebRenderImageHandlerType {
    WebGl,
    Media,
    WebGpu,
}

/// List of WebRender external images to be shared among all external image
/// consumers (WebGL, Media, WebGPU).
/// It ensures that external image identifiers are unique.
#[derive(Default)]
struct WebRenderExternalImageIdManagerInner {
    /// Map of all generated external images.
    external_images: FxHashMap<ExternalImageId, WebRenderImageHandlerType>,
    /// Id generator for the next external image identifier.
    next_image_id: u64,
}

#[derive(Default, Clone)]
pub struct WebRenderExternalImageIdManager(Arc<RwLock<WebRenderExternalImageIdManagerInner>>);

impl WebRenderExternalImageIdManager {
    pub fn next_id(&mut self, handler_type: WebRenderImageHandlerType) -> ExternalImageId {
        let mut inner = self.0.write();
        inner.next_image_id += 1;
        let key = ExternalImageId(inner.next_image_id);
        inner.external_images.insert(key, handler_type);
        key
    }

    pub fn remove(&mut self, key: &ExternalImageId) {
        self.0.write().external_images.remove(key);
    }

    pub fn get(&self, key: &ExternalImageId) -> Option<WebRenderImageHandlerType> {
        self.0.read().external_images.get(key).cloned()
    }
}

/// WebRender External Image Handler implementation.
pub struct WebRenderExternalImageHandlers {
    /// WebGL handler.
    webgl_handler: Option<Box<dyn WebRenderExternalImageApi>>,
    /// Media player handler.
    media_handler: Option<Box<dyn WebRenderExternalImageApi>>,
    /// WebGPU handler.
    webgpu_handler: Option<Box<dyn WebRenderExternalImageApi>>,
    /// A [`WebRenderExternalImageIdManager`] responsible for creating new [`ExternalImageId`]s.
    /// This is shared with the WebGL, WebGPU, and hardware-accelerated media threads and
    /// all other instances of [`WebRenderExternalImageHandlers`] -- one per WebRender instance.
    id_manager: WebRenderExternalImageIdManager,
}

impl WebRenderExternalImageHandlers {
    pub fn new(id_manager: WebRenderExternalImageIdManager) -> Self {
        Self {
            webgl_handler: Default::default(),
            media_handler: Default::default(),
            webgpu_handler: Default::default(),
            id_manager,
        }
    }

    pub fn id_manager(&self) -> WebRenderExternalImageIdManager {
        self.id_manager.clone()
    }

    pub fn set_handler(
        &mut self,
        handler: Box<dyn WebRenderExternalImageApi>,
        handler_type: WebRenderImageHandlerType,
    ) {
        match handler_type {
            WebRenderImageHandlerType::WebGl => self.webgl_handler = Some(handler),
            WebRenderImageHandlerType::Media => self.media_handler = Some(handler),
            WebRenderImageHandlerType::WebGpu => self.webgpu_handler = Some(handler),
        }
    }
}

impl ExternalImageHandler for WebRenderExternalImageHandlers {
    /// Lock the external image. Then, WR could start to read the
    /// image content.
    /// The WR client should not change the image content until the
    /// unlock() call.
    fn lock(
        &mut self,
        key: ExternalImageId,
        _channel_index: u8,
        _is_composited: bool,
    ) -> ExternalImage<'_> {
        let handler_type = self
            .id_manager()
            .get(&key)
            .expect("Tried to get unknown external image");
        match handler_type {
            WebRenderImageHandlerType::WebGl => {
                let (source, size) = self.webgl_handler.as_mut().unwrap().lock(key.0);
                let texture_id = match source {
                    ExternalImageSource::NativeTexture(b) => b,
                    _ => panic!("Wrong type"),
                };
                ExternalImage {
                    uv: TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0),
                    source: ExternalImageSource::NativeTexture(texture_id),
                }
            },
            WebRenderImageHandlerType::Media => {
                let (source, size) = self.media_handler.as_mut().unwrap().lock(key.0);
                let texture_id = match source {
                    ExternalImageSource::NativeTexture(b) => b,
                    _ => panic!("Wrong type"),
                };
                ExternalImage {
                    uv: TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0),
                    source: ExternalImageSource::NativeTexture(texture_id),
                }
            },
            WebRenderImageHandlerType::WebGpu => {
                let (source, size) = self.webgpu_handler.as_mut().unwrap().lock(key.0);
                ExternalImage {
                    uv: TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0),
                    source,
                }
            },
        }
    }

    /// Unlock the external image. The WR should not read the image
    /// content after this call.
    fn unlock(&mut self, key: ExternalImageId, _channel_index: u8) {
        let handler_type = self
            .id_manager()
            .get(&key)
            .expect("Tried to get unknown external image");
        match handler_type {
            WebRenderImageHandlerType::WebGl => self.webgl_handler.as_mut().unwrap().unlock(key.0),
            WebRenderImageHandlerType::Media => self.media_handler.as_mut().unwrap().unlock(key.0),
            WebRenderImageHandlerType::WebGpu => {
                self.webgpu_handler.as_mut().unwrap().unlock(key.0)
            },
        };
    }
}

#[derive(Deserialize, Serialize)]
/// Serializable image updates that must be performed by WebRender.
pub enum ImageUpdate {
    /// Register a new image.
    AddImage(
        ImageKey,
        ImageDescriptor,
        SerializableImageData,
        bool, /* is_animated_image */
    ),
    /// Delete a previously registered image registration.
    DeleteImage(ImageKey),
    /// Update an existing image registration.
    UpdateImage(
        ImageKey,
        ImageDescriptor,
        SerializableImageData,
        Option<Epoch>,
    ),
    /// Update an [`ImageDescriptor`] for an existing image. This is used primarily
    /// to modify the data offset for image animations.
    UpdateImageForAnimation(ImageKey, ImageDescriptor),
}

impl Debug for ImageUpdate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddImage(image_key, image_desc, _, is_animated_image) => f
                .debug_tuple("AddImage")
                .field(image_key)
                .field(image_desc)
                .field(is_animated_image)
                .finish(),
            Self::DeleteImage(image_key) => f.debug_tuple("DeleteImage").field(image_key).finish(),
            Self::UpdateImage(image_key, image_desc, _, epoch) => f
                .debug_tuple("UpdateImage")
                .field(image_key)
                .field(image_desc)
                .field(epoch)
                .finish(),
            Self::UpdateImageForAnimation(image_key, image_desc) => f
                .debug_tuple("UpdateAnimation")
                .field(image_key)
                .field(image_desc)
                .finish(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
/// Serialized `ImageData`.
pub enum SerializableImageData {
    /// A simple series of bytes, provided by the embedding and owned by WebRender.
    /// The format is stored out-of-band, currently in ImageDescriptor.
    Raw(GenericSharedMemory),
    /// An image owned by the embedding, and referenced by WebRender. This may
    /// take the form of a texture or a heap-allocated buffer.
    External(ExternalImageData),
}

impl From<SerializableImageData> for ImageData {
    fn from(value: SerializableImageData) -> Self {
        match value {
            SerializableImageData::Raw(shared_memory) => {
                ImageData::Raw(shared_memory.into_arc_vec())
            },
            SerializableImageData::External(image) => ImageData::External(image),
        }
    }
}

/// A trait that exposes the embedding layer's `WebView` to the Servo renderer.
/// This is to prevent a dependency cycle between the renderer and the embedding
/// layer.
pub trait WebViewTrait {
    fn id(&self) -> WebViewId;
    fn screen_geometry(&self) -> Option<ScreenGeometry>;
    fn set_animating(&self, new_value: bool);
    /// Notify the embedding layer that this `WebView`'s viewport geometry changed — its size, page
    /// or pinch zoom, or HiDPI scale — so it can refresh geometry, such as the accessibility root
    /// node, that the embedder derives from the viewport rather than from a pipeline update.
    fn notify_viewport_updated(&self);
}

/// What entity is reporting that a `Pipeline` has exited. Only when all have
/// done this will the renderer discard its details.
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
pub struct PipelineExitSource(u8);

bitflags! {
    impl PipelineExitSource: u8 {
        const Script = 1 << 0;
        const Constellation = 1 << 1;
    }
}

/// A [`PinchZoomInfos`] for a root [`Pipeline`] of an [`WebView`]. For any [`Pipeline`]
/// that is not a root, it should follow the viewport description of its pipeline since
/// pinch-zoom and resizing due to overlay UIs are not applicable there.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PinchZoomInfos {
    /// The zoom factor (or pinch-zoom).
    pub zoom_factor: Scale<f32, DevicePixel, DevicePixel>,

    /// The size relative to layout viewport.
    pub rect: Rect<f32, CSSPixel>,
}

impl PinchZoomInfos {
    /// New initial [`PinchZoomInfos`] without any pinch-zoom or resizing from a viewport size
    /// for a nested pipeline or newly initialized root pipeline.
    pub fn new_from_viewport_size(size: Size2D<f32, CSSPixel>) -> Self {
        Self {
            zoom_factor: Scale::identity(),
            rect: Rect::from_size(size),
        }
    }
}

#[cfg(test)]
mod raw_image_budget_tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use webrender_api::{IdNamespace, ImageDescriptorFlags, ImageFormat};

    use super::*;

    static BUDGET_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct BudgetResetGuard(bool);

    impl Drop for BudgetResetGuard {
        fn drop(&mut self) {
            PENDING_RAW_IMAGE_UPDATE_BYTES.store(0, Ordering::Release);
            IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.store(self.0, Ordering::Release);
        }
    }

    #[test]
    fn raw_image_update_budget_caps_pending_updates_and_preserves_control_messages() {
        let _test_lock = BUDGET_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = BudgetResetGuard(IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.load(Ordering::Acquire));
        PENDING_RAW_IMAGE_UPDATE_BYTES.store(0, Ordering::Release);
        enable_in_process_raw_image_budget();

        let (tx, rx) = mpsc::channel();
        let api = CrossProcessPaintApi::dummy_with_callback(Some(Box::new(move |msg| {
            let _ = tx.send(msg);
        })));

        let key = ImageKey::new(IdNamespace(42), 1);
        let desc = ImageDescriptor::new(1024, 1024, ImageFormat::BGRA8, ImageDescriptorFlags::empty());

        // AddImage is always delivered regardless of frame update budget.
        let init_frame = vec![0u8; 4 * 1024 * 1024];
        api.add_image(
            key,
            desc,
            SerializableImageData::Raw(GenericSharedMemory::from_vec(init_frame)),
            false,
        );
        assert_eq!(pending_raw_image_update_bytes(), 0);

        // Send 120 raw frames of 1 MiB each (120 MiB total) -> capped at 32 MiB.
        for _ in 0..120 {
            let frame = vec![7u8; 1024 * 1024];
            api.update_image(
                key,
                desc,
                SerializableImageData::Raw(GenericSharedMemory::from_vec(frame)),
                None,
            );
        }

        assert_eq!(
            pending_raw_image_update_bytes(),
            MAX_PENDING_RAW_IMAGE_UPDATE_BYTES
        );
        assert!(raw_image_update_budget_exceeded());

        // DeleteImage must still be delivered even while raw update budget is exhausted.
        api.delete_image(key);

        let mut raw_updates_received = 0usize;
        let mut adds_received = 0usize;
        let mut deletes_received = 0usize;
        while let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) {
            if let PaintMessage::UpdateImages(_, updates) = msg {
                let bytes = count_raw_image_update_bytes(&updates);
                release_raw_image_update_bytes(bytes);
                for u in updates {
                    match u {
                        ImageUpdate::AddImage(..) => adds_received += 1,
                        ImageUpdate::UpdateImage(..) => raw_updates_received += 1,
                        ImageUpdate::DeleteImage(..) => deletes_received += 1,
                        ImageUpdate::UpdateImageForAnimation(..) => {},
                    }
                }
            }
        }

        assert_eq!(adds_received, 1);
        assert_eq!(raw_updates_received, 32);
        assert_eq!(deletes_received, 1);
        assert_eq!(pending_raw_image_update_bytes(), 0);
        assert!(!raw_image_update_budget_exceeded());
    }

    #[test]
    fn update_video_images_ack_caps_pending_bytes_and_drains_on_ack() {
        let _test_lock = BUDGET_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = BudgetResetGuard(IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.load(Ordering::Acquire));
        PENDING_RAW_IMAGE_UPDATE_BYTES.store(0, Ordering::Release);
        IN_PROCESS_RAW_IMAGE_BUDGET_ACTIVE.store(false, Ordering::Release);

        let (tx, rx) = mpsc::channel();
        let api = CrossProcessPaintApi::dummy_with_callback(Some(Box::new(move |msg| {
            let _ = tx.send(msg);
        })));

        let pending_raw_bytes = Arc::new(AtomicUsize::new(0));
        let ack_pending = pending_raw_bytes.clone();
        let ack = GenericCallback::new(move |msg| {
            if let Ok(bytes) = msg {
                let _ = ack_pending.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    Some(current.saturating_sub(bytes))
                });
            }
        })
        .unwrap();

        let try_reserve = |pending: &AtomicUsize, bytes: usize| -> bool {
            pending
                .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    if current >= MAX_PENDING_RAW_IMAGE_UPDATE_BYTES {
                        return None;
                    }
                    let next = current.saturating_add(bytes);
                    if current == 0 || next <= MAX_PENDING_RAW_IMAGE_UPDATE_BYTES {
                        Some(next)
                    } else {
                        None
                    }
                })
                .is_ok()
        };

        let key = ImageKey::new(IdNamespace(42), 7);
        let desc = ImageDescriptor::new(1024, 1024, ImageFormat::BGRA8, ImageDescriptorFlags::empty());

        // AddImage is always delivered and does not consume raw UpdateImage backpressure budget.
        let init_frame = vec![0u8; 4 * 1024 * 1024];
        assert!(api.update_video_images(
            key.into(),
            [ImageUpdate::AddImage(
                key,
                desc,
                SerializableImageData::Raw(GenericSharedMemory::from_vec(init_frame)),
                false,
            )]
            .into(),
            None,
        ));
        assert_eq!(pending_raw_bytes.load(Ordering::Acquire), 0);

        // Send 64 raw frames of 1 MiB each (64 MiB total) -> capped at 32 MiB via per-renderer budget.
        for _ in 0..64 {
            let frame_bytes = 1024 * 1024;
            if try_reserve(&pending_raw_bytes, frame_bytes) {
                let frame = vec![7u8; frame_bytes];
                assert!(api.update_video_images(
                    key.into(),
                    [ImageUpdate::UpdateImage(
                        key,
                        desc,
                        SerializableImageData::Raw(GenericSharedMemory::from_vec(frame)),
                        None,
                    )]
                    .into(),
                    Some(ack.clone()),
                ));
            }
        }

        assert_eq!(
            pending_raw_bytes.load(Ordering::Acquire),
            MAX_PENDING_RAW_IMAGE_UPDATE_BYTES
        );
        assert_eq!(pending_raw_image_update_bytes(), 0);

        // DeleteImage must still be delivered even while per-renderer raw update budget is exhausted.
        assert!(api.update_video_images(
            key.into(),
            [ImageUpdate::DeleteImage(key)].into(),
            None,
        ));

        let mut raw_updates_received = 0usize;
        let mut adds_received = 0usize;
        let mut deletes_received = 0usize;
        while let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) {
            if let PaintMessage::UpdateVideoImages(_, updates, maybe_ack) = msg {
                let raw_bytes = count_raw_image_update_bytes(&updates);
                for u in updates {
                    match u {
                        ImageUpdate::AddImage(..) => adds_received += 1,
                        ImageUpdate::UpdateImage(..) => raw_updates_received += 1,
                        ImageUpdate::DeleteImage(..) => deletes_received += 1,
                        ImageUpdate::UpdateImageForAnimation(..) => {},
                    }
                }
                if raw_bytes > 0 {
                    maybe_ack.expect("raw UpdateImage must carry ack").send(raw_bytes).unwrap();
                }
            }
        }

        assert_eq!(adds_received, 1);
        assert_eq!(raw_updates_received, 32);
        assert_eq!(deletes_received, 1);
        assert_eq!(pending_raw_bytes.load(Ordering::Acquire), 0);
    }
}
