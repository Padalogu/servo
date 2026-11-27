/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ptr;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::JSObject;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::buffer_source::{create_buffer_source, get_buffer_source_copy};
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericUnionTypes::{
    ArrayBufferViewOrArrayBuffer, MediaKeyStatusOrUndefined,
};
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::codegen::Bindings::MediaKeyStatusMapBinding::{
    MediaKeyStatus, MediaKeyStatusMapMethods,
};
use crate::dom::bindings::iterable::Iterable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;

/// <https://w3c.github.io/encrypted-media/#mediakeystatusmap-interface>
#[dom_struct]
pub(crate) struct MediaKeyStatusMap {
    reflector_: Reflector,
    entries: DomRefCell<Vec<(Vec<u8>, MediaKeyStatus)>>,
}

impl MediaKeyStatusMap {
    pub(crate) fn new_inherited() -> Self {
        Self {
            reflector_: Reflector::new(),
            entries: DomRefCell::new(Vec::new()),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited()), global)
    }

    pub(crate) fn merge_entries(&self, updates: Vec<(Vec<u8>, MediaKeyStatus)>) {
        let mut entries = self.entries.borrow_mut();
        for (kid, status) in updates {
            if let Some((_, existing_status)) = entries.iter_mut().find(|(k, _)| *k == kid) {
                *existing_status = status;
            } else {
                entries.push((kid, status));
            }
        }
    }

    pub(crate) fn mark_all_released(&self) {
        let mut entries = self.entries.borrow_mut();
        for (_, status) in entries.iter_mut() {
            *status = MediaKeyStatus::Released;
        }
    }
}

impl MediaKeyStatusMapMethods<crate::DomTypeHolder> for MediaKeyStatusMap {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeystatusmap-size>
    fn Size(&self) -> u32 {
        self.entries.borrow().len() as u32
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeystatusmap-has>
    fn Has(&self, key_id: ArrayBufferViewOrArrayBuffer) -> bool {
        let bytes = get_buffer_source_copy((&key_id).into());
        self.entries.borrow().iter().any(|(k, _)| *k == bytes)
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeystatusmap-get>
    fn Get(&self, key_id: ArrayBufferViewOrArrayBuffer) -> MediaKeyStatusOrUndefined {
        let bytes = get_buffer_source_copy((&key_id).into());
        match self.entries.borrow().iter().find(|(k, _)| *k == bytes) {
            Some((_, status)) => MediaKeyStatusOrUndefined::MediaKeyStatus(*status),
            None => MediaKeyStatusOrUndefined::Undefined(()),
        }
    }
}

impl Iterable for MediaKeyStatusMap {
    type Key = RootedTraceableBox<HeapArrayBuffer>;
    type Value = MediaKeyStatus;

    fn get_iterable_length(&self, _cx: &mut JSContext) -> u32 {
        self.entries.borrow().len() as u32
    }

    fn get_value_at_index(&self, _cx: &mut JSContext, index: u32) -> Self::Value {
        self.entries.borrow()[index as usize].1
    }

    fn get_key_at_index(&self, cx: &mut JSContext, index: u32) -> Self::Key {
        let bytes = self.entries.borrow()[index as usize].0.clone();
        rooted!(&in(cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
        create_buffer_source::<ArrayBufferU8>(cx, &bytes, array_buffer_ptr.handle_mut())
            .expect("Failed to allocate ArrayBuffer for MediaKeyStatusMap key")
    }
}
