/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediasource::sourcebuffer::SourceBuffer;

#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    pub(crate) fn new_inherited() -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            buffers: DomRefCell::new(Vec::new()),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited()), global)
    }

    pub(crate) fn len(&self) -> usize {
        self.buffers.borrow().len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buffers.borrow().is_empty()
    }

    pub(crate) fn item(&self, idx: usize) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(idx)
            .map(|buffer| DomRoot::from_ref(&**buffer))
    }

    pub(crate) fn contains(&self, buffer: &SourceBuffer) -> bool {
        self.buffers.borrow().iter().any(|b| &**b == buffer)
    }

    pub(crate) fn snapshot(&self) -> Vec<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .iter()
            .map(|buffer| DomRoot::from_ref(&**buffer))
            .collect()
    }

    pub(crate) fn add(&self, buffer: &SourceBuffer) {
        self.buffers.borrow_mut().push(Dom::from_ref(buffer));
        self.queue_event(Atom::from("addsourcebuffer"));
    }

    pub(crate) fn remove(&self, buffer: &SourceBuffer) -> bool {
        let mut buffers = self.buffers.borrow_mut();
        if let Some(pos) = buffers.iter().position(|b| &**b == buffer) {
            buffers.remove(pos);
            drop(buffers);
            self.queue_event(Atom::from("removesourcebuffer"));
            true
        } else {
            false
        }
    }

    pub(crate) fn clear_with_events(&self) {
        let count = self.buffers.borrow().len();
        self.buffers.borrow_mut().clear();
        for _ in 0..count {
            self.queue_event(Atom::from("removesourcebuffer"));
        }
    }

    fn queue_event(&self, name: Atom) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(sourcebufferlist_event: move |cx| {
                let this = this.root();
                this.upcast::<EventTarget>().fire_event(cx, name);
            }));
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-length>
    fn Length(&self) -> u32 {
        self.len() as u32
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-getter>
    fn IndexedGetter(&self, idx: u32) -> Option<DomRoot<SourceBuffer>> {
        self.item(idx as usize)
    }

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onaddsourcebuffer
    event_handler!(addsourcebuffer, GetOnaddsourcebuffer, SetOnaddsourcebuffer);

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onremovesourcebuffer
    event_handler!(
        removesourcebuffer,
        GetOnremovesourcebuffer,
        SetOnremovesourcebuffer
    );
}
