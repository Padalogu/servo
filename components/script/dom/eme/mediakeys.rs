/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::atomic::{AtomicU64, Ordering};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericUnionTypes::ArrayBufferViewOrArrayBuffer;
use script_bindings::domstring::TracedDOMString;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::str::DOMString;
use servo_media::player::cenc::ClearKeyStore;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaKeysBinding::{
    MediaKeySessionType, MediaKeysMethods,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::eme::mediakeysession::MediaKeySession;
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmlmediaelement::{HTMLMediaElement, MediaElementWeakRef};
use crate::dom::promise::{Promise, RootedPromise};

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// <https://w3c.github.io/encrypted-media/#mediakeys-interface>
#[dom_struct]
pub(crate) struct MediaKeys {
    reflector_: Reflector,
    key_system: TracedDOMString,
    #[no_trace]
    #[ignore_malloc_size_of = "Arc<RwLock<_>> ClearKeyStore"]
    clearkey_store: ClearKeyStore,
    media_element: DomRefCell<Option<MediaElementWeakRef>>,
}

impl MediaKeys {
    pub(crate) fn new_inherited(key_system: DOMString) -> Self {
        Self {
            reflector_: Reflector::new(),
            key_system: key_system.traced(),
            clearkey_store: ClearKeyStore::new(),
            media_element: DomRefCell::new(None),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        key_system: &str,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(DOMString::from(key_system))),
            global,
        )
    }

    pub(crate) fn clearkey_store(&self) -> &ClearKeyStore {
        &self.clearkey_store
    }

    pub(crate) fn next_session_id() -> String {
        let id = NEXT_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("clearkey-{id:08x}")
    }

    pub(crate) fn attached_media_element(&self) -> Option<DomRoot<HTMLMediaElement>> {
        self.media_element
            .borrow()
            .as_ref()
            .and_then(MediaElementWeakRef::root)
    }

    pub(crate) fn attach_to_media_element(&self, element: &HTMLMediaElement) -> Result<(), ()> {
        if let Some(existing) = self.attached_media_element() {
            if std::ptr::eq(&*existing, element) {
                return Ok(());
            }
            return Err(());
        }
        *self.media_element.borrow_mut() = Some(MediaElementWeakRef::new(element));
        Ok(())
    }

    pub(crate) fn detach_from_media_element(&self, element: &HTMLMediaElement) {
        let matches = self
            .attached_media_element()
            .is_some_and(|existing| std::ptr::eq(&*existing, element));
        if matches {
            *self.media_element.borrow_mut() = None;
        }
    }

    pub(crate) fn notify_keys_updated(&self) {
        if let Some(element) = self.attached_media_element() {
            element.notify_eme_keys_updated();
        }
    }
}

impl MediaKeysMethods<crate::DomTypeHolder> for MediaKeys {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeys-createsession>
    fn CreateSession(
        &self,
        cx: &mut JSContext,
        session_type: MediaKeySessionType,
    ) -> Fallible<DomRoot<MediaKeySession>> {
        if session_type != MediaKeySessionType::Temporary {
            return Err(Error::NotSupported(Some(
                "Only temporary sessions are supported for ClearKey".into(),
            )));
        }
        Ok(MediaKeySession::new(
            cx,
            &self.global(),
            self,
            session_type,
        ))
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeys-setservercertificate>
    fn SetServerCertificate(
        &self,
        cx: &mut CurrentRealm,
        server_certificate: ArrayBufferViewOrArrayBuffer,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let bytes = get_buffer_source_copy((&server_certificate).into());
        if bytes.is_empty() {
            promise.reject_error(cx, Error::Type(c"serverCertificate must not be empty".to_owned()));
            return promise;
        }
        // ClearKey does not support server certificates; per W3C EME resolve with false.
        promise.resolve_native(cx, &false);
        promise
    }
}
