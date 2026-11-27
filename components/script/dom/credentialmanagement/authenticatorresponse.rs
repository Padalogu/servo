/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::buffer_source::HeapBufferSource;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::codegen::Bindings::PublicKeyCredentialBinding::AuthenticatorResponseMethods;
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;

/// <https://w3c.github.io/webauthn/#authenticatorresponse>
#[dom_struct]
pub(crate) struct AuthenticatorResponse {
    reflector_: Reflector,
    #[ignore_malloc_size_of = "mozjs"]
    client_data_json: HeapBufferSource<ArrayBufferU8>,
}

impl AuthenticatorResponse {
    #[expect(dead_code)]
    pub(crate) fn new_inherited(
        client_data_json: HeapBufferSource<ArrayBufferU8>,
    ) -> AuthenticatorResponse {
        AuthenticatorResponse {
            reflector_: Reflector::new(),
            client_data_json,
        }
    }

    #[expect(dead_code)]
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        client_data_json: HeapBufferSource<ArrayBufferU8>,
    ) -> DomRoot<AuthenticatorResponse> {
        reflect_dom_object(
            cx,
            Box::new(AuthenticatorResponse::new_inherited(client_data_json)),
            global,
        )
    }
}

impl AuthenticatorResponseMethods<DomTypeHolder> for AuthenticatorResponse {
    /// <https://w3c.github.io/webauthn/#dom-authenticatorresponse-clientdatajson>
    fn ClientDataJSON(&self) -> RootedTraceableBox<HeapArrayBuffer> {
        self.client_data_json
            .get_typed_array()
            .expect("clientDataJSON ArrayBuffer must be initialized")
    }
}
