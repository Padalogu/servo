/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::time::Instant;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::num::Finite;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::root::DomRoot;

use crate::dom::bindings::codegen::Bindings::IdleDeadlineBinding::IdleDeadlineMethods;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// <https://w3c.github.io/requestidlecallback/#the-idledeadline-interface>
#[dom_struct]
pub(crate) struct IdleDeadline {
    reflector_: Reflector,
    did_timeout: bool,
    deadline: Instant,
}

impl IdleDeadline {
    fn new_inherited(did_timeout: bool, deadline: Instant) -> Self {
        Self {
            reflector_: Reflector::new(),
            did_timeout,
            deadline,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        did_timeout: bool,
        deadline: Instant,
    ) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited(did_timeout, deadline)), global)
    }
}

impl IdleDeadlineMethods<crate::DomTypeHolder> for IdleDeadline {
    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-timeremaining>
    fn TimeRemaining(&self) -> Finite<f64> {
        let remaining_ms = self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            * 1000.0;
        Finite::wrap(remaining_ms.clamp(0.0, 50.0))
    }

    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-didtimeout>
    fn DidTimeout(&self) -> bool {
        self.did_timeout
    }
}

#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct IdleCallbackTimeoutCallback {
    #[ignore_malloc_size_of = "Because it is non-owning"]
    pub(crate) window: Trusted<Window>,
    pub(crate) handle: u32,
}

impl IdleCallbackTimeoutCallback {
    pub(crate) fn invoke(self, cx: &mut JSContext) {
        self.window.root().run_idle_callback(cx, self.handle);
    }
}
