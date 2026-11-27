/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericUnionTypes::ArrayBufferViewOrArrayBuffer;
use script_bindings::reflector::reflect_dom_object;
use script_bindings::str::DOMString;
use serde::Deserialize;
use serde_json::json;
use servo_media::player::cenc::extract_kids_from_cenc_pssh;
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaKeyMessageEventBinding::MediaKeyMessageType;
use crate::dom::bindings::codegen::Bindings::MediaKeySessionBinding::{
    MediaKeySessionClosedReason, MediaKeySessionMethods,
};
use crate::dom::bindings::codegen::Bindings::MediaKeyStatusMapBinding::MediaKeyStatus;
use crate::dom::bindings::codegen::Bindings::MediaKeysBinding::MediaKeySessionType;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eme::mediakeymessageevent::MediaKeyMessageEvent;
use crate::dom::eme::mediakeys::MediaKeys;
use crate::dom::eme::mediakeystatusmap::MediaKeyStatusMap;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};

#[derive(Deserialize)]
struct ClearKeyInitDataJson {
    kids: Vec<String>,
}

#[derive(Deserialize)]
struct ClearKeyJwkEntry {
    kty: String,
    k: String,
    kid: String,
}

#[derive(Deserialize)]
struct ClearKeyJwkSet {
    keys: Vec<ClearKeyJwkEntry>,
    #[serde(default, rename = "type")]
    session_type: Option<String>,
}

fn parse_kids_init_data(init_data_type: &str, init_data: &[u8]) -> Result<Vec<[u8; 16]>, Error> {
    if init_data.is_empty() {
        return Err(Error::Type(c"initData must not be empty".to_owned()));
    }
    match init_data_type {
        "cenc" => {
            let kids = extract_kids_from_cenc_pssh(init_data)
                .map_err(|_| Error::Type(c"Invalid cenc initData".to_owned()))?;
            if kids.is_empty() {
                return Err(Error::NotSupported(Some(
                    "No ClearKey KIDs found in cenc initData".into(),
                )));
            }
            Ok(kids)
        },
        "keyids" => {
            let parsed: ClearKeyInitDataJson = serde_json::from_slice(init_data)
                .map_err(|_| Error::Type(c"Invalid keyids JSON initData".to_owned()))?;
            if parsed.kids.is_empty() {
                return Err(Error::Type(
                    c"keyids initData must contain at least one KID".to_owned(),
                ));
            }
            let mut kids = Vec::with_capacity(parsed.kids.len());
            for encoded in &parsed.kids {
                let decoded = URL_SAFE_NO_PAD
                    .decode(encoded)
                    .map_err(|_| Error::Type(c"Invalid base64url KID in keyids initData".to_owned()))?;
                let kid: [u8; 16] = decoded
                    .try_into()
                    .map_err(|_| Error::Type(c"KID must be 16 bytes".to_owned()))?;
                kids.push(kid);
            }
            Ok(kids)
        },
        "webm" => {
            let kid: [u8; 16] = init_data
                .try_into()
                .map_err(|_| Error::Type(c"webm initData KID must be 16 bytes".to_owned()))?;
            Ok(vec![kid])
        },
        _ => Err(Error::NotSupported(Some(format!(
            "Unsupported initDataType: {init_data_type}"
        )))),
    }
}

fn parse_clearkey_jwk_set(bytes: &[u8]) -> Result<Vec<([u8; 16], [u8; 16])>, Error> {
    if bytes.is_empty() {
        return Err(Error::Type(c"License response must not be empty".to_owned()));
    }
    let parsed: ClearKeyJwkSet = serde_json::from_slice(bytes)
        .map_err(|_| Error::Type(c"Invalid ClearKey JWK Set JSON".to_owned()))?;
    if let Some(ref ty) = parsed.session_type {
        if ty != "temporary" {
            return Err(Error::NotSupported(Some(format!(
                "Unsupported ClearKey session type in license: {ty}"
            ))));
        }
    }
    let mut keys = Vec::with_capacity(parsed.keys.len());
    for entry in &parsed.keys {
        if entry.kty != "oct" {
            return Err(Error::Type(
                c"ClearKey JWK entry kty must be 'oct'".to_owned(),
            ));
        }
        let raw_kid = URL_SAFE_NO_PAD
            .decode(&entry.kid)
            .map_err(|_| Error::Type(c"Invalid base64url kid in ClearKey JWK".to_owned()))?;
        let raw_k = URL_SAFE_NO_PAD
            .decode(&entry.k)
            .map_err(|_| Error::Type(c"Invalid base64url k in ClearKey JWK".to_owned()))?;
        let kid: [u8; 16] = raw_kid
            .try_into()
            .map_err(|_| Error::Type(c"ClearKey JWK kid must be 16 bytes".to_owned()))?;
        let key: [u8; 16] = raw_k
            .try_into()
            .map_err(|_| Error::Type(c"ClearKey JWK k must be 16 bytes".to_owned()))?;
        keys.push((kid, key));
    }
    Ok(keys)
}

/// <https://w3c.github.io/encrypted-media/#mediakeysession-interface>
#[dom_struct]
pub(crate) struct MediaKeySession {
    eventtarget: EventTarget,
    media_keys: Dom<MediaKeys>,
    session_id: DomRefCell<String>,
    session_type: MediaKeySessionType,
    expiration: Cell<f64>,
    #[ignore_malloc_size_of = "TracedPromise"]
    closed_promise: RefCell<TracedPromise>,
    key_statuses: Dom<MediaKeyStatusMap>,
    callable_init: Cell<bool>,
    closing_or_closed: Cell<bool>,
    owned_kids: DomRefCell<Vec<Vec<u8>>>,
}

impl MediaKeySession {
    pub(crate) fn new_inherited(
        media_keys: &MediaKeys,
        session_type: MediaKeySessionType,
        closed_promise: &RootedPromise,
        key_statuses: &MediaKeyStatusMap,
    ) -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            media_keys: Dom::from_ref(media_keys),
            session_id: DomRefCell::new(String::new()),
            session_type,
            expiration: Cell::new(f64::NAN),
            closed_promise: RefCell::new(closed_promise.to_traced()),
            key_statuses: Dom::from_ref(key_statuses),
            callable_init: Cell::new(false),
            closing_or_closed: Cell::new(false),
            owned_kids: DomRefCell::new(Vec::new()),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        media_keys: &MediaKeys,
        session_type: MediaKeySessionType,
    ) -> DomRoot<Self> {
        let closed_promise = Promise::new(cx, global);
        let key_statuses = MediaKeyStatusMap::new(cx, global);
        reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(
                media_keys,
                session_type,
                &closed_promise,
                &key_statuses,
            )),
            global,
        )
    }

    fn queue_message_event(&self, message_type: MediaKeyMessageType, message_bytes: Vec<u8>) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(eme_license_message: move |cx| {
                let this = this.root();
                if this.closing_or_closed.get() {
                    return;
                }
                let global = this.global();
                if let Ok(event) = MediaKeyMessageEvent::new(
                    cx,
                    &global,
                    Atom::from("message"),
                    EventBubbles::DoesNotBubble,
                    EventCancelable::NotCancelable,
                    message_type,
                    &message_bytes,
                ) {
                    event.upcast::<Event>().fire(cx, this.upcast::<EventTarget>());
                }
            }));
    }

    fn queue_key_statuses_change_event(&self) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(eme_keystatuseschange: move |cx| {
                let this = this.root();
                if this.closing_or_closed.get() {
                    return;
                }
                this.upcast::<EventTarget>()
                    .fire_event(cx, Atom::from("keystatuseschange"));
            }));
    }

    fn release_session_keys(&self) {
        let _ = std::mem::take(&mut *self.owned_kids.borrow_mut());
        let session_id = self.session_id.borrow().clone();
        if !session_id.is_empty() {
            self.media_keys.clearkey_store().remove_session(&session_id);
        }
        self.key_statuses.mark_all_released();
        self.media_keys.notify_keys_updated();
    }
}

impl MediaKeySessionMethods<crate::DomTypeHolder> for MediaKeySession {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-sessionid>
    fn SessionId(&self) -> DOMString {
        DOMString::from(self.session_id.borrow().clone())
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-expiration>
    fn Expiration(&self) -> f64 {
        self.expiration.get()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-closed>
    fn Closed(&self, cx: &JSContext) -> RootedPromise {
        self.closed_promise.borrow().root(cx)
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-keystatuses>
    fn KeyStatuses(&self) -> DomRoot<MediaKeyStatusMap> {
        DomRoot::from_ref(&self.key_statuses)
    }

    event_handler!(keystatuseschange, GetOnkeystatuseschange, SetOnkeystatuseschange);
    event_handler!(message, GetOnmessage, SetOnmessage);

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-generaterequest>
    fn GenerateRequest(
        &self,
        cx: &mut CurrentRealm,
        init_data_type: DOMString,
        init_data: ArrayBufferViewOrArrayBuffer,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if self.closing_or_closed.get() || self.callable_init.get() {
            promise.reject_error(
                cx,
                Error::InvalidState(Some("MediaKeySession is already initialized or closed".into())),
            );
            return promise;
        }
        if init_data_type.is_empty() {
            promise.reject_error(cx, Error::Type(c"initDataType must not be empty".to_owned()));
            return promise;
        }
        let bytes = get_buffer_source_copy((&init_data).into());
        let kids = match parse_kids_init_data(&init_data_type.str(), &bytes) {
            Ok(kids) => kids,
            Err(err) => {
                promise.reject_error(cx, err);
                return promise;
            },
        };

        self.callable_init.set(true);
        if self.session_id.borrow().is_empty() {
            *self.session_id.borrow_mut() = MediaKeys::next_session_id();
        }

        let encoded_kids: Vec<String> = kids.iter().map(|kid| URL_SAFE_NO_PAD.encode(kid)).collect();
        let license_request = json!({
            "kids": encoded_kids,
            "type": match self.session_type {
                MediaKeySessionType::Temporary => "temporary",
                MediaKeySessionType::Persistent_license => "persistent-license",
            },
        })
        .to_string()
        .into_bytes();

        promise.resolve_native(cx, &());
        self.queue_message_event(MediaKeyMessageType::License_request, license_request);
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-load>
    fn Load(&self, cx: &mut CurrentRealm, session_id: DOMString) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if self.closing_or_closed.get() || self.callable_init.get() {
            promise.reject_error(
                cx,
                Error::InvalidState(Some("MediaKeySession is already initialized or closed".into())),
            );
            return promise;
        }
        if session_id.is_empty() {
            promise.reject_error(cx, Error::Type(c"sessionId must not be empty".to_owned()));
            return promise;
        }
        // Temporary sessions do not support persistent storage lookup.
        promise.resolve_native(cx, &false);
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-update>
    fn Update(
        &self,
        cx: &mut CurrentRealm,
        response: ArrayBufferViewOrArrayBuffer,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if self.closing_or_closed.get() || !self.callable_init.get() {
            promise.reject_error(
                cx,
                Error::InvalidState(Some(
                    "MediaKeySession is not initialized or already closed".into(),
                )),
            );
            return promise;
        }

        let bytes = get_buffer_source_copy((&response).into());
        let keys = match parse_clearkey_jwk_set(&bytes) {
            Ok(keys) => keys,
            Err(err) => {
                promise.reject_error(cx, err);
                return promise;
            },
        };

        if self.session_id.borrow().is_empty() {
            *self.session_id.borrow_mut() = MediaKeys::next_session_id();
        }
        let session_id = self.session_id.borrow().clone();
        let store = self.media_keys.clearkey_store();
        let mut owned = self.owned_kids.borrow_mut();
        let mut status_entries = Vec::with_capacity(keys.len());
        for (kid, key) in keys {
            store.set_session_key(&session_id, kid, key);
            let kid_vec = kid.to_vec();
            if !owned.contains(&kid_vec) {
                owned.push(kid_vec.clone());
            }
            status_entries.push((kid_vec, MediaKeyStatus::Usable));
        }
        drop(owned);
        self.key_statuses.merge_entries(status_entries);
        self.media_keys.notify_keys_updated();

        promise.resolve_native(cx, &());
        self.queue_key_statuses_change_event();
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-close>
    fn Close(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if self.closing_or_closed.get() {
            promise.resolve_native(cx, &());
            return promise;
        }
        self.closing_or_closed.set(true);
        self.release_session_keys();
        self.closed_promise
            .borrow()
            .resolve_native(cx, &MediaKeySessionClosedReason::Closed_by_application);
        promise.resolve_native(cx, &());
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-remove>
    fn Remove(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if self.closing_or_closed.get() || !self.callable_init.get() {
            promise.reject_error(
                cx,
                Error::InvalidState(Some(
                    "MediaKeySession is not initialized or already closed".into(),
                )),
            );
            return promise;
        }
        self.release_session_keys();
        promise.resolve_native(cx, &());
        self.queue_key_statuses_change_event();
        promise
    }
}
