/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use malloc_size_of_derive::MallocSizeOf;
use net::image_cache::{ImageCacheFactoryImpl, ImageCacheImpl};
use net_traits::image_cache::{
    FontResolver, ImageCache, ImageCacheFactory, ImageCacheResponseMessage, ImageCacheResult,
    ImageLoadListener, ImageOrMetadataAvailable, ImageResponse, MaskImageMode, PendingImageId,
    PendingImageResponse, RasterizationResult,
};
use net_traits::request::RequestId;
use net_traits::{
    FetchMetadata, FetchResponseMsg, FilteredMetadata, Metadata, NetworkError, ResourceFetchTiming,
    ResourceTimingType,
};
use paint_api::{CrossProcessPaintApi, ImageUpdate, PaintMessage};
// For dummy Font Resolver
use resvg::usvg::{Font, fontdb};
use servo_base::id::{PipelineId, TEST_PIPELINE_ID, TEST_WEBVIEW_ID};
use servo_url::{ImmutableOrigin, ServoUrl};
use uuid::Uuid;
use webrender_api::ImageKey;

use crate::mock_origin;

#[derive(MallocSizeOf)]
struct DummyFontResolver;

impl FontResolver for DummyFontResolver {
    fn resolve(&self, _: &Font, _: &mut Arc<fontdb::Database>) -> Option<fontdb::ID> {
        None
    }

    fn resolve_fallback(
        &self,
        _: char,
        _: &[fontdb::ID],
        _: &mut Arc<fontdb::Database>,
    ) -> Option<fontdb::ID> {
        None
    }
}

fn create_test_image_cache() -> (Arc<dyn ImageCache>, Receiver<PipelineId>) {
    let (sender, receiver) = unbounded();
    let paint_api = CrossProcessPaintApi::dummy_with_callback(Some(Box::new(move |msg| {
        if let PaintMessage::GenerateImageKeysForPipeline(_, pipeline_id) = msg {
            let _ = sender.send(pipeline_id);
        }
    })));
    let dummy_resolver = Arc::new(DummyFontResolver);

    let factory = ImageCacheFactoryImpl::new(vec![]);
    let cache = factory.create(
        TEST_WEBVIEW_ID,
        TEST_PIPELINE_ID,
        &paint_api,
        dummy_resolver.clone(),
    );
    (cache, receiver)
}

fn handle_pending_key_requests(cache: &Arc<dyn ImageCache>, receiver: &Receiver<PipelineId>) {
    while let Ok(_pipeline_id) = receiver.try_recv() {
        let keys: Vec<_> = (0..10)
            .map(|i| ImageKey::new(webrender_api::IdNamespace(42), i as u32))
            .collect();
        cache.dispatch_fill_key_cache_with_batch_of_keys(keys);
    }
}

fn create_test_listener(id: PendingImageId, sender: Sender<ImageResponse>) -> ImageLoadListener {
    let callback = Box::new(move |msg: ImageCacheResponseMessage| {
        if let ImageCacheResponseMessage::NotifyPendingImageLoadStatus(PendingImageResponse {
            response,
            ..
        }) = msg
        {
            let _ = sender.send(response);
        }
    });
    ImageLoadListener::new(callback, TEST_PIPELINE_ID, id)
}

fn jpeg_image_bytes() -> Vec<u8> {
    include_bytes!("test.jpeg").to_vec()
}

fn svg_image_bytes() -> Vec<u8> {
    br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
    <circle cx="50" cy="50" r="40" fill="red"/>
</svg>"#
        .to_vec()
}

fn create_test_metadata(mime_type: Option<mime::Mime>) -> FetchMetadata {
    let url = ServoUrl::parse("http://example.com").unwrap();
    let mut metadata = Metadata::default(url);
    metadata.set_content_type(mime_type.as_ref());
    FetchMetadata::Filtered {
        filtered: FilteredMetadata::Opaque,
        unsafe_: metadata,
    }
}

fn create_request_id() -> RequestId {
    RequestId(Uuid::nil())
}

fn create_timing() -> ResourceFetchTiming {
    ResourceFetchTiming::new(ResourceTimingType::Resource)
}

#[test]
fn test_get_cached_image_status_before_request() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let result = cache.get_cached_image_status(url, &origin, None);

    match result {
        ImageCacheResult::ReadyForRequest(id) => {
            assert!(id.0 > 0);
        },
        _ => panic!("Expected ReadyForRequest"),
    }
}

#[test]
fn test_get_cached_image_status_no_response_data() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    let result = cache.get_cached_image_status(url, &origin, None);

    match result {
        ImageCacheResult::Pending(pending_id) => {
            assert_eq!(id, pending_id);
        },
        _ => panic!("Expected Pending after initial request"),
    }
}

#[test]
fn test_notify_pending_response_with_headers() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let result = cache.get_cached_image_status(url, &origin, None);

    match result {
        ImageCacheResult::Pending(pending_id) => {
            assert_eq!(id, pending_id);
        },
        _ => panic!("Expected Pending after headers received"),
    }
}

#[test]
fn test_notify_pending_response_with_partial_chunk() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let small_chunk = vec![0u8; 10];
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), small_chunk.into()),
    );

    let result = cache.get_cached_image_status(url, &origin, None);

    match result {
        ImageCacheResult::Pending(pending_id) => {
            assert_eq!(id, pending_id);
        },
        _ => panic!("Expected Pending with insufficient data"),
    }
}

#[test]
fn test_notify_pending_response_with_metadata_chunk() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/test.jpeg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let jpeg_bytes = jpeg_image_bytes();
    let metadata_chunk = jpeg_bytes[..200.min(jpeg_bytes.len())].to_vec();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), metadata_chunk.into()),
    );

    let result = cache.get_cached_image_status(url, &origin, None);

    match result {
        ImageCacheResult::Available(ImageOrMetadataAvailable::MetadataAvailable(metadata, _)) => {
            assert!(metadata.width > 0);
            assert!(metadata.height > 0);
        },
        ImageCacheResult::Pending(_) => {},
        _ => panic!("Expected MetadataAvailable or Pending"),
    }
}

#[test]
fn test_notify_pending_response_complete() {
    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/test.jpeg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let jpeg_bytes = jpeg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), jpeg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    loop {
        handle_pending_key_requests(&cache, &key_receiver);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        if matches!(
            result,
            ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { .. })
        ) {
            break;
        }
    }

    let image = cache.get_image(url, origin, None);
    assert!(image.is_some());
    assert!(image.unwrap().as_raster_image().is_some());
}

#[test]
fn test_notify_pending_response_network_error() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Err(NetworkError::InvalidMethod)),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(
            create_request_id(),
            Err(NetworkError::InvalidMethod),
            create_timing(),
        ),
    );

    let result = cache.get_cached_image_status(url, &origin, None);
    assert!(matches!(result, ImageCacheResult::FailedToLoadOrDecode));
}

#[test]
fn test_image_listener_on_complete_response() {
    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/test.jpeg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    let (sender, receiver) = unbounded();
    let listener = create_test_listener(id, sender);

    cache.add_listener(listener);

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let jpeg_bytes = jpeg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), jpeg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    loop {
        handle_pending_key_requests(&cache, &key_receiver);
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(response) => match response {
                ImageResponse::Loaded(..) | ImageResponse::MetadataLoaded(..) => break,
                _ => {},
            },
            Err(_) => {},
        }
    }
}

#[test]
fn test_image_listener_on_network_error() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    let (sender, receiver) = unbounded();
    let listener = create_test_listener(id, sender);

    cache.add_listener(listener);

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Err(NetworkError::InvalidMethod)),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(
            create_request_id(),
            Err(NetworkError::InvalidMethod),
            create_timing(),
        ),
    );

    match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
        Ok(ImageResponse::FailedToLoadOrDecode) => {},
        Ok(_) => panic!("Expected FailedToLoadOrDecode response"),
        Err(_) => panic!("Expected to receive error response"),
    }
}

#[test]
fn test_image_listener_on_metadata_available() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/test.jpeg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    let (sender, receiver) = unbounded();
    let listener = create_test_listener(id, sender);

    cache.add_listener(listener);

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let jpeg_bytes = jpeg_image_bytes();
    let metadata_chunk = jpeg_bytes[..200.min(jpeg_bytes.len())].to_vec();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), metadata_chunk.into()),
    );

    match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
        Ok(ImageResponse::MetadataLoaded(metadata)) => {
            assert!(metadata.width > 0);
            assert!(metadata.height > 0);
        },
        Ok(_) => {},
        Err(_) => {},
    }
}

#[test]
fn test_get_image_returns_none_when_not_loaded() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.png").unwrap();
    let origin = mock_origin();

    let image = cache.get_image(url, origin, None);
    assert!(image.is_none());
}

#[test]
fn test_multiple_listeners_same_image() {
    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/test.jpeg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    let (sender1, receiver1) = unbounded();
    let listener1 = create_test_listener(id, sender1);

    let (sender2, receiver2) = unbounded();
    let listener2 = create_test_listener(id, sender2);

    cache.add_listener(listener1);
    cache.add_listener(listener2);

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let jpeg_bytes = jpeg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), jpeg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    loop {
        handle_pending_key_requests(&cache, &key_receiver);
        match receiver1.recv_timeout(Duration::from_millis(10)) {
            Ok(ImageResponse::Loaded(..) | ImageResponse::MetadataLoaded(..)) => break,
            Ok(_) => {},
            Err(_) => {},
        }
    }

    loop {
        handle_pending_key_requests(&cache, &key_receiver);
        match receiver2.recv_timeout(Duration::from_millis(10)) {
            Ok(ImageResponse::Loaded(..) | ImageResponse::MetadataLoaded(..)) => break,
            Ok(_) => {},
            Err(_) => {},
        }
    }
}

#[test]
fn test_cached_image_reuse() {
    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/test.jpeg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );

    let jpeg_bytes = jpeg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), jpeg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    loop {
        handle_pending_key_requests(&cache, &key_receiver);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        if matches!(result, ImageCacheResult::Available(_)) {
            break;
        }
    }
}

#[test]
fn test_svg_rasterization() {
    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.svg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(
            create_request_id(),
            Ok(create_test_metadata(Some(mime::IMAGE_SVG))),
        ),
    );

    let svg_bytes = svg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), svg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    let vec_img = loop {
        handle_pending_key_requests(&cache, &key_receiver);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        let ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { image, .. }) =
            result
        else {
            continue;
        };

        let net_traits::image_cache::Image::Vector(vec_img) = image else {
            panic!("Expected vector image");
        };
        break vec_img;
    };

    let size = webrender_api::units::DeviceIntSize::new(100, 100);
    cache.rasterize_vector_image(vec_img.id, size, None);
}

#[test]
fn test_rasterization_listener() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.svg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(
            create_request_id(),
            Ok(create_test_metadata(Some(mime::IMAGE_SVG))),
        ),
    );

    let svg_bytes = svg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), svg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    let vec_img = loop {
        handle_pending_key_requests(&cache, &key_receiver);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        let ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { image, .. }) =
            result
        else {
            continue;
        };

        let net_traits::image_cache::Image::Vector(vec_img) = image else {
            panic!("Expected vector image");
        };
        break vec_img;
    };

    let size = webrender_api::units::DeviceIntSize::new(100, 100);
    let notified = Arc::new(AtomicBool::new(false));
    let notified_clone = notified.clone();

    let callback = Box::new(move |msg: ImageCacheResponseMessage| {
        if let ImageCacheResponseMessage::VectorImageRasterizationComplete(_) = msg {
            notified_clone.store(true, Ordering::SeqCst);
        }
    });

    cache.rasterize_vector_image(vec_img.id, size, None);

    cache.add_rasterization_complete_listener(TEST_PIPELINE_ID, vec_img.id, size, callback);

    loop {
        handle_pending_key_requests(&cache, &key_receiver);
        if notified.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
/// Test if multiple rasterization requests rasterize the svg only once.
fn test_svg_rasterization_do_not_double_rasterize() {
    let (cache, _key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.svg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(
            create_request_id(),
            Ok(create_test_metadata(Some(mime::IMAGE_SVG))),
        ),
    );

    let svg_bytes = svg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), svg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    let vec_img = loop {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        let ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { image, .. }) =
            result
        else {
            continue;
        };

        let net_traits::image_cache::Image::Vector(vec_img) = image else {
            panic!("Expected vector image");
        };
        break vec_img;
    };

    let size = webrender_api::units::DeviceIntSize::new(100, 100);
    // Because we do not set image keys yet, the rasterization task will never finish, so we know the added tasks will stay in the queue.
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size, None),
        RasterizationResult::PendingRasterization
    ));
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size, None),
        RasterizationResult::PendingRasterization
    ));
    assert_eq!(cache.number_of_rasterize_tasks(), 1);
}

#[test]
fn test_svg_rasterize_zero_size_fallback() {
    let (cache, key_receiver) = create_test_image_cache();
    let url = ServoUrl::parse("http://example.com/image.svg").unwrap();
    let origin = mock_origin();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(
            create_request_id(),
            Ok(create_test_metadata(Some(mime::IMAGE_SVG))),
        ),
    );

    let svg_bytes = svg_image_bytes();
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), svg_bytes.into()),
    );

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    let vec_img = loop {
        handle_pending_key_requests(&cache, &key_receiver);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        let ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { image, .. }) =
            result
        else {
            continue;
        };

        let net_traits::image_cache::Image::Vector(vec_img) = image else {
            panic!("Expected vector image");
        };
        break vec_img;
    };

    let size = webrender_api::units::DeviceIntSize::new(0, 100);
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size, None),
        RasterizationResult::PendingRasterization
    ));
    let size = webrender_api::units::DeviceIntSize::new(100, 0);
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size, None),
        RasterizationResult::PendingRasterization
    ));
    let size = webrender_api::units::DeviceIntSize::new(0, 0);
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size, None),
        RasterizationResult::PendingRasterization
    ));

    assert!(matches!(
        cache.rasterize_vector_image(net_traits::image_cache::PendingImageId(99999), size, None),
        RasterizationResult::InvalidSvgParameters
    ));
}

fn create_test_image_cache_with_cap(
    max_decoded_bytes: usize,
) -> (
    Arc<ImageCacheImpl>,
    Receiver<PipelineId>,
    Receiver<ImageKey>,
) {
    let (key_req_sender, key_req_receiver) = unbounded();
    let (delete_sender, delete_receiver) = unbounded();
    let paint_api = CrossProcessPaintApi::dummy_with_callback(Some(Box::new(move |msg| {
        match msg {
            PaintMessage::GenerateImageKeysForPipeline(_, pipeline_id) => {
                let _ = key_req_sender.send(pipeline_id);
            },
            PaintMessage::UpdateImages(_, updates) => {
                for update in updates {
                    if let ImageUpdate::DeleteImage(key) = update {
                        let _ = delete_sender.send(key);
                    }
                }
            },
            _ => {},
        }
    })));
    let dummy_resolver = Arc::new(DummyFontResolver);
    let factory = ImageCacheFactoryImpl::new(vec![]).with_max_decoded_bytes(max_decoded_bytes);
    let cache = factory.create_store_cache_for_test(
        TEST_WEBVIEW_ID,
        TEST_PIPELINE_ID,
        &paint_api,
        dummy_resolver,
    );
    (cache, key_req_receiver, delete_receiver)
}

fn load_jpeg_into_cache(
    cache: &Arc<dyn ImageCache>,
    key_receiver: &Receiver<PipelineId>,
    url: &ServoUrl,
    origin: &ImmutableOrigin,
) -> (usize, ImageKey) {
    let id = match cache.get_cached_image_status(url.clone(), origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest for {url}"),
    };

    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(create_request_id(), Ok(create_test_metadata(None))),
    );
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), jpeg_image_bytes().into()),
    );
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    loop {
        handle_pending_key_requests(cache, key_receiver);
        std::thread::sleep(Duration::from_millis(10));
        if matches!(
            cache.get_cached_image_status(url.clone(), origin, None),
            ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { .. })
        ) {
            break;
        }
    }

    let raster = cache
        .get_image(url.clone(), origin.clone(), None)
        .expect("Loaded JPEG should be in cache")
        .as_raster_image()
        .expect("Loaded JPEG should be RasterImage");
    (
        raster.bytes.len(),
        raster.id.expect("Loaded JPEG should have a WebRenderImageKey"),
    )
}

#[test]
fn test_lru_decoded_byte_cap_evicts_oldest_and_emits_delete_image() {
    // test.jpeg is 320x427 RGBA8 = 546,560 bytes; 1,100,000 fits 2 images and overflows on 3rd.
    let (cache, key_receiver, delete_receiver) = create_test_image_cache_with_cap(1_100_000);
    let cache_dyn: Arc<dyn ImageCache> = cache.clone();
    let origin = mock_origin();

    let url1 = ServoUrl::parse("http://example.com/1.jpeg").unwrap();
    let url2 = ServoUrl::parse("http://example.com/2.jpeg").unwrap();
    let url3 = ServoUrl::parse("http://example.com/3.jpeg").unwrap();

    let (single_bytes, _key1) = load_jpeg_into_cache(&cache_dyn, &key_receiver, &url1, &origin);
    assert_eq!(cache.decoded_bytes(), single_bytes);

    let (_bytes2, key2) = load_jpeg_into_cache(&cache_dyn, &key_receiver, &url2, &origin);
    assert_eq!(cache.decoded_bytes(), single_bytes * 2);

    // Touch url1 so url2 becomes the least-recently-used entry.
    assert!(cache.get_image(url1.clone(), origin.clone(), None).is_some());

    let _img3 = load_jpeg_into_cache(&cache_dyn, &key_receiver, &url3, &origin);

    let deleted_key = delete_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected ImageUpdate::DeleteImage for LRU-evicted entry");
    assert_eq!(deleted_key, key2);
    assert!(delete_receiver.try_recv().is_err());

    assert_eq!(cache.decoded_bytes(), single_bytes * 2);
    assert!(cache.decoded_bytes() <= cache.max_decoded_bytes());
    assert!(cache.get_image(url2.clone(), origin.clone(), None).is_none());
    assert!(matches!(
        cache.get_cached_image_status(url2, &origin, None),
        ImageCacheResult::ReadyForRequest(_)
    ));
    assert!(cache.get_image(url1, origin.clone(), None).is_some());
    assert!(cache.get_image(url3, origin, None).is_some());
}

#[test]
fn test_lru_svg_rasterization_byte_cap_evicts_oldest_and_rerasterizes() {
    use std::sync::atomic::{AtomicBool, Ordering};

    // 100x100 (40,000 B) + 120x120 (57,600 B) fits in 150,000 B; adding 150x150 (90,000 B) overflows.
    let (cache, key_receiver, delete_receiver) = create_test_image_cache_with_cap(150_000);
    let cache_dyn: Arc<dyn ImageCache> = cache.clone();
    let origin = mock_origin();
    let url = ServoUrl::parse("http://example.com/image.svg").unwrap();

    let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
        ImageCacheResult::ReadyForRequest(id) => id,
        _ => panic!("Expected ReadyForRequest"),
    };
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponse(
            create_request_id(),
            Ok(create_test_metadata(Some(mime::IMAGE_SVG))),
        ),
    );
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseChunk(create_request_id(), svg_image_bytes().into()),
    );
    cache.notify_pending_response(
        id,
        FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
    );

    let vec_img = loop {
        handle_pending_key_requests(&cache_dyn, &key_receiver);
        std::thread::sleep(Duration::from_millis(10));
        let result = cache.get_cached_image_status(url.clone(), &origin, None);
        if let ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable {
            image: net_traits::image_cache::Image::Vector(vec_img),
            ..
        }) = result
        {
            break vec_img;
        }
    };

    let rasterize_and_wait = |size: webrender_api::units::DeviceIntSize| -> ImageKey {
        let notified = Arc::new(AtomicBool::new(false));
        let notified_clone = notified.clone();
        let callback = Box::new(move |msg: ImageCacheResponseMessage| {
            if let ImageCacheResponseMessage::VectorImageRasterizationComplete(_) = msg {
                notified_clone.store(true, Ordering::SeqCst);
            }
        });
        assert!(matches!(
            cache.rasterize_vector_image(vec_img.id, size, None),
            RasterizationResult::PendingRasterization
        ));
        cache.add_rasterization_complete_listener(TEST_PIPELINE_ID, vec_img.id, size, callback);
        while !notified.load(Ordering::SeqCst) {
            handle_pending_key_requests(&cache_dyn, &key_receiver);
            std::thread::sleep(Duration::from_millis(10));
        }
        match cache.rasterize_vector_image(vec_img.id, size, None) {
            RasterizationResult::Image(img) => {
                img.id.expect("rasterized SVG should have a WebRenderImageKey")
            },
            other => panic!("Expected RasterizationResult::Image for {size:?}, got {other:?}"),
        }
    };

    let size_100 = webrender_api::units::DeviceIntSize::new(100, 100);
    let size_120 = webrender_api::units::DeviceIntSize::new(120, 120);
    let size_150 = webrender_api::units::DeviceIntSize::new(150, 150);

    let _key_100 = rasterize_and_wait(size_100);
    let key_120 = rasterize_and_wait(size_120);
    assert_eq!(cache.decoded_bytes(), 40_000 + 57_600);

    // Touch 100x100 so 120x120 becomes the LRU entry.
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size_100, None),
        RasterizationResult::Image(_)
    ));

    let _img_150 = rasterize_and_wait(size_150);

    let deleted_key = delete_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected ImageUpdate::DeleteImage for LRU-evicted rasterized SVG");
    assert_eq!(deleted_key, key_120);
    assert!(delete_receiver.try_recv().is_err());

    assert_eq!(cache.decoded_bytes(), 40_000 + 90_000);
    assert!(cache.decoded_bytes() <= cache.max_decoded_bytes());
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size_100, None),
        RasterizationResult::Image(_)
    ));
    // Evicted size can be re-rasterized cleanly from retained VectorImageData.
    assert!(matches!(
        cache.rasterize_vector_image(vec_img.id, size_120, None),
        RasterizationResult::PendingRasterization
    ));
}

#[test]
fn test_svg_double_buffered_last_rasterized_image_and_eviction() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (cache, key_receiver, _delete_receiver) = create_test_image_cache_with_cap(512 * 1024);
    let origin = mock_origin();
    let svg_id = Uuid::new_v4();
    let size_100 = webrender_api::units::DeviceIntSize::new(100, 100);
    let mut next_key_id = 1u32;

    let decode_svg = |url_str: &str, svg_bytes: Vec<u8>| -> PendingImageId {
        let url = ServoUrl::parse(url_str).unwrap();
        let id = match cache.get_cached_image_status(url.clone(), &origin, None) {
            ImageCacheResult::ReadyForRequest(id) => id,
            _ => panic!("Expected ReadyForRequest for {url_str}"),
        };
        cache.notify_pending_response(
            id,
            FetchResponseMsg::ProcessResponse(
                create_request_id(),
                Ok(create_test_metadata(Some(mime::IMAGE_SVG))),
            ),
        );
        cache.notify_pending_response(
            id,
            FetchResponseMsg::ProcessResponseChunk(create_request_id(), svg_bytes.into()),
        );
        cache.notify_pending_response(
            id,
            FetchResponseMsg::ProcessResponseEOF(create_request_id(), Ok(()), create_timing()),
        );
        loop {
            std::thread::sleep(Duration::from_millis(10));
            if let ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable {
                image: net_traits::image_cache::Image::Vector(vec_img),
                ..
            }) = cache.get_cached_image_status(url.clone(), &origin, None)
            {
                break vec_img.id;
            }
        }
    };

    let mut fulfill_one_key = || {
        key_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Expected pending WebRender key request");
        let key = ImageKey::new(webrender_api::IdNamespace(42), next_key_id);
        next_key_id += 1;
        cache.dispatch_fill_key_cache_with_batch_of_keys(vec![key]);
    };

    // Frame 1: complete decode + rasterization with key #1.
    let id1 = decode_svg("data:image/svg+xml;base64,frame1", svg_image_bytes());
    let done1 = Arc::new(AtomicBool::new(false));
    let done1_clone = done1.clone();
    assert!(matches!(
        cache.rasterize_vector_image(id1, size_100, Some(svg_id)),
        RasterizationResult::PendingRasterization
    ));
    cache.add_rasterization_complete_listener(
        TEST_PIPELINE_ID,
        id1,
        size_100,
        Box::new(move |msg| {
            if let ImageCacheResponseMessage::VectorImageRasterizationComplete(_) = msg {
                done1_clone.store(true, Ordering::SeqCst);
            }
        }),
    );
    fulfill_one_key();
    while !done1.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let key1 = match cache.rasterize_vector_image(id1, size_100, Some(svg_id)) {
        RasterizationResult::Image(img) => img.id.expect("frame1 key should be set"),
        other => panic!("Expected RasterizationResult::Image for frame1, got {other:?}"),
    };
    assert_eq!(
        cache
            .get_last_rasterized_svg_image(&svg_id, size_100)
            .and_then(|img| img.id),
        Some(key1)
    );

    // Frame 2: start rasterization; withholds key #2 so frame2 stays deterministically pending.
    let blue_svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><circle cx="50" cy="50" r="40" fill="blue"/></svg>"#.to_vec();
    let id2 = decode_svg("data:image/svg+xml;base64,frame2", blue_svg);
    assert_ne!(id1, id2);
    let done2 = Arc::new(AtomicBool::new(false));
    let done2_clone = done2.clone();
    assert!(matches!(
        cache.rasterize_vector_image(id2, size_100, Some(svg_id)),
        RasterizationResult::PendingRasterization
    ));
    cache.add_rasterization_complete_listener(
        TEST_PIPELINE_ID,
        id2,
        size_100,
        Box::new(move |msg| {
            if let ImageCacheResponseMessage::VectorImageRasterizationComplete(_) = msg {
                done2_clone.store(true, Ordering::SeqCst);
            }
        }),
    );
    // Worker finishes resvg and asks for key #2, remaining parked until we dispatch it.
    key_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("Expected key request for pending frame2");
    assert_eq!(
        cache
            .get_last_rasterized_svg_image(&svg_id, size_100)
            .and_then(|img| img.id),
        Some(key1),
        "frame1 must remain available while frame2 is pending completion"
    );

    // Dispatch key #2 and verify double-buffered swap to key2.
    let key2_expected = ImageKey::new(webrender_api::IdNamespace(42), next_key_id);
    cache.dispatch_fill_key_cache_with_batch_of_keys(vec![key2_expected]);
    while !done2.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let key2 = match cache.rasterize_vector_image(id2, size_100, Some(svg_id)) {
        RasterizationResult::Image(img) => img.id.expect("frame2 key should be set"),
        other => panic!("Expected RasterizationResult::Image for frame2, got {other:?}"),
    };
    assert_ne!(key1, key2);
    assert_eq!(
        cache
            .get_last_rasterized_svg_image(&svg_id, size_100)
            .and_then(|img| img.id),
        Some(key2)
    );

    cache.evict_rasterized_image(&svg_id);
    assert!(cache.get_last_rasterized_svg_image(&svg_id, size_100).is_none());
}

#[test]
fn test_r8_mask_image_key_alpha_luminance_cache_and_clear() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use paint_api::SerializableImageData;
    use pixels::{CorsStatus, ImageFrame, ImageMetadata, PixelFormat, RasterImage};
    use webrender_api::{ImageDescriptor, ImageFormat};

    let (add_sender, add_receiver) = unbounded::<(ImageKey, ImageDescriptor, Vec<u8>)>();
    let (delete_sender, delete_receiver) = unbounded::<ImageKey>();
    let next_id = Arc::new(AtomicU32::new(1));
    let next_id_clone = next_id.clone();

    let paint_api = CrossProcessPaintApi::dummy_with_callback(Some(Box::new(move |msg| {
        match msg {
            PaintMessage::GenerateImageKey(_webview_id, sender) => {
                let id = next_id_clone.fetch_add(1, Ordering::Relaxed);
                let _ = sender.send(ImageKey::new(webrender_api::IdNamespace(42), id));
            },
            PaintMessage::UpdateImages(_, updates) => {
                for update in updates {
                    match update {
                        ImageUpdate::AddImage(key, descriptor, SerializableImageData::Raw(shm), _) => {
                            let _ = add_sender.send((key, descriptor, shm.to_vec()));
                        },
                        ImageUpdate::DeleteImage(key) => {
                            let _ = delete_sender.send(key);
                        },
                        _ => {},
                    }
                }
            },
            _ => {},
        }
    })));
    let factory = ImageCacheFactoryImpl::new(vec![]);
    let cache = factory.create(
        TEST_WEBVIEW_ID,
        TEST_PIPELINE_ID,
        &paint_api,
        Arc::new(DummyFontResolver),
    );

    // 2x2 black shape (#000000) with varying alpha (matches Wikipedia Vector SVG icons).
    let black_rgba_bytes = vec![
        0, 0, 0, 0, // transparent
        0, 0, 0, 128, // half alpha black
        0, 0, 0, 255, // opaque black
        0, 0, 0, 64, // quarter alpha black
    ];
    let source_key = ImageKey::new(webrender_api::IdNamespace(42), 999);
    let black_image = RasterImage {
        metadata: ImageMetadata { width: 2, height: 2 },
        format: PixelFormat::RGBA8,
        frames: vec![ImageFrame {
            delay: None,
            byte_range: 0..black_rgba_bytes.len(),
            width: 2,
            height: 2,
        }],
        bytes: Arc::new(black_rgba_bytes),
        id: Some(source_key),
        cors_status: CorsStatus::Unsafe,
        load_time: None,
        is_opaque: false,
        loop_count: None,
    };

    let alpha_key = cache
        .get_or_create_mask_image_key(&black_image, MaskImageMode::Alpha)
        .expect("Expected alpha mask key");
    let (uploaded_key, descriptor, uploaded_bytes) = add_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected R8 AddImage upload");
    assert_eq!(uploaded_key, alpha_key);
    assert_eq!(descriptor.format, ImageFormat::R8);
    assert_eq!(descriptor.size, webrender_api::units::DeviceIntSize::new(2, 2));
    assert_eq!(uploaded_bytes, vec![0, 128, 255, 64]);

    // Second lookup must reuse cached key without uploading again.
    let cached_key = cache
        .get_or_create_mask_image_key(&black_image, MaskImageMode::Alpha)
        .expect("Expected cached alpha mask key");
    assert_eq!(cached_key, alpha_key);
    assert!(add_receiver.try_recv().is_err());

    // Luminance mode on premultiplied RGBA8 (white, red, green, black).
    let color_rgba_bytes = vec![
        255, 255, 255, 255, // white -> 255
        255, 0, 0, 255,     // red -> 54
        0, 255, 0, 255,     // green -> 183
        0, 0, 0, 255,       // black -> 0
    ];
    let color_key = ImageKey::new(webrender_api::IdNamespace(42), 1000);
    let color_image = RasterImage {
        metadata: ImageMetadata { width: 2, height: 2 },
        format: PixelFormat::RGBA8,
        frames: vec![ImageFrame {
            delay: None,
            byte_range: 0..color_rgba_bytes.len(),
            width: 2,
            height: 2,
        }],
        bytes: Arc::new(color_rgba_bytes),
        id: Some(color_key),
        cors_status: CorsStatus::Unsafe,
        load_time: None,
        is_opaque: true,
        loop_count: None,
    };

    let lum_key = cache
        .get_or_create_mask_image_key(&color_image, MaskImageMode::Luminance)
        .expect("Expected luminance mask key");
    assert_ne!(lum_key, alpha_key);
    let (uploaded_lum_key, lum_desc, lum_bytes) = add_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected luminance R8 AddImage upload");
    assert_eq!(uploaded_lum_key, lum_key);
    assert_eq!(lum_desc.format, ImageFormat::R8);
    assert_eq!(lum_bytes, vec![255, 54, 182, 0]);

    cache.clear();
    let mut deleted = Vec::new();
    while let Ok(key) = delete_receiver.recv_timeout(Duration::from_millis(200)) {
        deleted.push(key);
    }
    assert!(deleted.contains(&alpha_key));
    assert!(deleted.contains(&lum_key));
}

#[test]
fn test_synthetic_r8_mask_cache_hit_lru_eviction_and_clear() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use paint_api::SerializableImageData;
    use webrender_api::units::DeviceIntSize;
    use webrender_api::{ImageDescriptor, ImageDescriptorFlags, ImageFormat};

    let (add_sender, add_receiver) = unbounded::<(ImageKey, ImageDescriptor, Vec<u8>)>();
    let (delete_sender, delete_receiver) = unbounded::<ImageKey>();
    let next_id = Arc::new(AtomicU32::new(1));
    let next_id_clone = next_id.clone();

    let paint_api = CrossProcessPaintApi::dummy_with_callback(Some(Box::new(move |msg| {
        match msg {
            PaintMessage::GenerateImageKey(_webview_id, sender) => {
                let id = next_id_clone.fetch_add(1, Ordering::Relaxed);
                let _ = sender.send(ImageKey::new(webrender_api::IdNamespace(42), id));
            },
            PaintMessage::UpdateImages(_, updates) => {
                for update in updates {
                    match update {
                        ImageUpdate::AddImage(key, descriptor, SerializableImageData::Raw(shm), _) => {
                            let _ = add_sender.send((key, descriptor, shm.to_vec()));
                        },
                        ImageUpdate::DeleteImage(key) => {
                            let _ = delete_sender.send(key);
                        },
                        _ => {},
                    }
                }
            },
            _ => {},
        }
    })));
    let factory = ImageCacheFactoryImpl::new(vec![]);
    let cache = factory.create(
        TEST_WEBVIEW_ID,
        TEST_PIPELINE_ID,
        &paint_api,
        Arc::new(DummyFontResolver),
    );

    // Invalid dimensions or mismatched data length must return None.
    assert!(
        cache
            .get_or_create_r8_mask_image_key(999, DeviceIntSize::new(0, 2), &[])
            .is_none()
    );
    assert!(
        cache
            .get_or_create_r8_mask_image_key(999, DeviceIntSize::new(2, 2), &[1, 2, 3])
            .is_none()
    );
    assert!(cache.get_r8_mask_image_key(100).is_none());

    // First insert uploads an R8 texture with expected descriptor and bytes.
    let key_100 = cache
        .get_or_create_r8_mask_image_key(100, DeviceIntSize::new(2, 2), &[10, 20, 30, 40])
        .expect("Expected synthetic R8 mask key");
    let (uploaded_key, descriptor, uploaded_bytes) = add_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected R8 AddImage upload");
    assert_eq!(uploaded_key, key_100);
    assert_eq!(descriptor.format, ImageFormat::R8);
    assert_eq!(descriptor.stride, None);
    assert_eq!(descriptor.flags, ImageDescriptorFlags::empty());
    assert_eq!(descriptor.size, DeviceIntSize::new(2, 2));
    assert_eq!(uploaded_bytes, vec![10, 20, 30, 40]);

    // Cache hits via both lookup helpers reuse key_100 without re-uploading.
    assert_eq!(cache.get_r8_mask_image_key(100), Some(key_100));
    assert_eq!(
        cache.get_or_create_r8_mask_image_key(100, DeviceIntSize::new(2, 2), &[10, 20, 30, 40]),
        Some(key_100)
    );
    assert!(add_receiver.try_recv().is_err());

    // Fill remaining 63 slots (total 64); no eviction should occur yet.
    let mut keys = Vec::with_capacity(64);
    keys.push((100u64, key_100));
    for i in 1u64..=63 {
        let key = cache
            .get_or_create_r8_mask_image_key(i, DeviceIntSize::new(1, 1), &[i as u8])
            .expect("Expected synthetic R8 mask key within cap");
        let _ = add_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Expected R8 AddImage upload");
        keys.push((i, key));
    }
    assert!(delete_receiver.try_recv().is_err());

    // Touch cache_key 100 so cache_key 1 becomes the least-recently-used entry.
    assert_eq!(cache.get_r8_mask_image_key(100), Some(key_100));

    // Inserting entry #65 evicts the oldest entry (cache_key 1).
    let key_64 = cache
        .get_or_create_r8_mask_image_key(64, DeviceIntSize::new(1, 1), &[64])
        .expect("Expected synthetic R8 mask key on eviction");
    let _ = add_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected R8 AddImage upload");

    let evicted_key = delete_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Expected DeleteImage for LRU-evicted R8 mask");
    assert_eq!(evicted_key, keys[1].1);
    assert!(delete_receiver.try_recv().is_err());
    assert!(cache.get_r8_mask_image_key(1).is_none());
    assert_eq!(cache.get_r8_mask_image_key(100), Some(key_100));
    assert_eq!(cache.get_r8_mask_image_key(64), Some(key_64));

    // clear() deletes all remaining 64 cached entries.
    cache.clear();
    let mut deleted = Vec::new();
    while let Ok(key) = delete_receiver.recv_timeout(Duration::from_millis(200)) {
        deleted.push(key);
    }
    assert_eq!(deleted.len(), 64);
    assert!(deleted.contains(&key_100));
    assert!(deleted.contains(&key_64));
    assert!(cache.get_r8_mask_image_key(100).is_none());
    assert!(cache.get_r8_mask_image_key(64).is_none());
}
