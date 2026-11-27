/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ptr;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::JSObject;
use js::rust::HandleObject;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::buffer_source::{
    HeapBufferSource, create_buffer_source, get_buffer_source_copy,
};
use script_bindings::domstring::TracedDOMString;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::trace::RootedTraceableBox;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::codegen::Bindings::MediaEncryptedEventBinding::{
    MediaEncryptedEventInit, MediaEncryptedEventMethods,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// <https://w3c.github.io/encrypted-media/#mediaencryptedevent>
#[dom_struct]
pub(crate) struct MediaEncryptedEvent {
    event: Event,
    init_data_type: TracedDOMString,
    #[ignore_malloc_size_of = "mozjs"]
    init_data: HeapBufferSource<ArrayBufferU8>,
}

impl MediaEncryptedEvent {
    pub(crate) fn new_inherited(
        init_data_type: DOMString,
        init_data: HeapBufferSource<ArrayBufferU8>,
    ) -> Self {
        Self {
            event: Event::new_inherited(),
            init_data_type: init_data_type.traced(),
            init_data,
        }
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        type_: Atom,
        bubbles: EventBubbles,
        cancelable: EventCancelable,
        init_data_type: DOMString,
        init_data_bytes: Option<&[u8]>,
    ) -> Fallible<DomRoot<Self>> {
        Self::new_with_proto(
            cx,
            global,
            None,
            type_,
            bubbles,
            cancelable,
            init_data_type,
            init_data_bytes,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
        type_: Atom,
        bubbles: EventBubbles,
        cancelable: EventCancelable,
        init_data_type: DOMString,
        init_data_bytes: Option<&[u8]>,
    ) -> Fallible<DomRoot<Self>> {
        let init_data = if let Some(bytes) = init_data_bytes {
            rooted!(&in(cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
            create_buffer_source::<ArrayBufferU8>(cx, bytes, array_buffer_ptr.handle_mut())
                .map_err(|()| Error::JSFailed)?;
            HeapBufferSource::<ArrayBufferU8>::new(array_buffer_ptr.handle())
        } else {
            HeapBufferSource::default()
        };

        let ev = reflect_dom_object_with_proto(
            cx,
            Box::new(Self::new_inherited(init_data_type, init_data)),
            global,
            proto,
        );
        {
            let event = ev.upcast::<Event>();
            event.init_event(type_, bool::from(bubbles), bool::from(cancelable));
        }
        Ok(ev)
    }
}

impl MediaEncryptedEventMethods<crate::DomTypeHolder> for MediaEncryptedEvent {
    /// <https://w3c.github.io/encrypted-media/#dom-mediaencryptedevent-mediaencryptedevent>
    fn Constructor(
        cx: &mut JSContext,
        global: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &MediaEncryptedEventInit,
    ) -> Fallible<DomRoot<Self>> {
        let init_data_bytes = init
            .initData
            .as_ref()
            .map(|buf| get_buffer_source_copy(buf.into()));
        Self::new_with_proto(
            cx,
            &global.as_global_scope(),
            proto,
            Atom::from(type_),
            EventBubbles::from(init.parent.bubbles),
            EventCancelable::from(init.parent.cancelable),
            init.initDataType.clone(),
            init_data_bytes.as_deref(),
        )
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediaencryptedevent-initdatatype>
    fn InitDataType(&self) -> DOMString {
        self.init_data_type.clone().root()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediaencryptedevent-initdata>
    fn GetInitData(&self) -> Option<RootedTraceableBox<HeapArrayBuffer>> {
        if self.init_data.is_initialized() {
            self.init_data.get_typed_array().ok()
        } else {
            None
        }
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.event.IsTrusted()
    }
}
