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
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::trace::RootedTraceableBox;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::codegen::Bindings::MediaKeyMessageEventBinding::{
    MediaKeyMessageEventInit, MediaKeyMessageEventMethods, MediaKeyMessageType,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// <https://w3c.github.io/encrypted-media/#mediakeymessageevent>
#[dom_struct]
pub(crate) struct MediaKeyMessageEvent {
    event: Event,
    message_type: MediaKeyMessageType,
    #[ignore_malloc_size_of = "mozjs"]
    message: HeapBufferSource<ArrayBufferU8>,
}

impl MediaKeyMessageEvent {
    pub(crate) fn new_inherited(
        message_type: MediaKeyMessageType,
        message: HeapBufferSource<ArrayBufferU8>,
    ) -> Self {
        Self {
            event: Event::new_inherited(),
            message_type,
            message,
        }
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        type_: Atom,
        bubbles: EventBubbles,
        cancelable: EventCancelable,
        message_type: MediaKeyMessageType,
        message_bytes: &[u8],
    ) -> Fallible<DomRoot<Self>> {
        Self::new_with_proto(
            cx,
            global,
            None,
            type_,
            bubbles,
            cancelable,
            message_type,
            message_bytes,
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
        message_type: MediaKeyMessageType,
        message_bytes: &[u8],
    ) -> Fallible<DomRoot<Self>> {
        rooted!(&in(cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
        create_buffer_source::<ArrayBufferU8>(cx, message_bytes, array_buffer_ptr.handle_mut())
            .map_err(|()| Error::JSFailed)?;
        let message = HeapBufferSource::<ArrayBufferU8>::new(array_buffer_ptr.handle());

        let ev = reflect_dom_object_with_proto(
            cx,
            Box::new(Self::new_inherited(message_type, message)),
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

impl MediaKeyMessageEventMethods<crate::DomTypeHolder> for MediaKeyMessageEvent {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeymessageevent-mediakeymessageevent>
    fn Constructor(
        cx: &mut JSContext,
        global: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &MediaKeyMessageEventInit,
    ) -> Fallible<DomRoot<Self>> {
        let message_bytes = get_buffer_source_copy((&init.message).into());
        Self::new_with_proto(
            cx,
            &global.as_global_scope(),
            proto,
            Atom::from(type_),
            EventBubbles::from(init.parent.bubbles),
            EventCancelable::from(init.parent.cancelable),
            init.messageType,
            &message_bytes,
        )
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeymessageevent-messagetype>
    fn MessageType(&self) -> MediaKeyMessageType {
        self.message_type
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeymessageevent-message>
    fn Message(&self) -> RootedTraceableBox<HeapArrayBuffer> {
        self.message
            .get_typed_array()
            .expect("MediaKeyMessageEvent message ArrayBuffer must be initialized")
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.event.IsTrusted()
    }
}
