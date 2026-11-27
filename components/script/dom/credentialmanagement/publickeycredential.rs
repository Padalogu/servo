/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::buffer_source::HeapBufferSource;
use script_bindings::domstring::TracedDOMString;
use script_bindings::record::Record;
use script_bindings::reflector::reflect_dom_object;
use script_bindings::str::{DOMString, USVString};
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::codegen::Bindings::PublicKeyCredentialBinding::PublicKeyCredentialMethods;
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::credentialmanagement::authenticatorresponse::AuthenticatorResponse;
use crate::dom::credentialmanagement::credential::Credential;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::window::Window;

/// <https://w3c.github.io/webauthn/#publickeycredential>
#[dom_struct]
pub(crate) struct PublicKeyCredential {
    credential: Credential,
    #[ignore_malloc_size_of = "mozjs"]
    raw_id: HeapBufferSource<ArrayBufferU8>,
    response: Dom<AuthenticatorResponse>,
    authenticator_attachment: Option<TracedDOMString>,
}

impl PublicKeyCredential {
    #[expect(dead_code)]
    pub(crate) fn new_inherited(
        id: USVString,
        raw_id: HeapBufferSource<ArrayBufferU8>,
        response: &AuthenticatorResponse,
        authenticator_attachment: Option<DOMString>,
    ) -> PublicKeyCredential {
        PublicKeyCredential {
            credential: Credential::new_inherited(id, DOMString::from_static("public-key")),
            raw_id,
            response: Dom::from_ref(response),
            authenticator_attachment: authenticator_attachment.map(DOMString::traced),
        }
    }

    #[expect(dead_code)]
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        id: USVString,
        raw_id: HeapBufferSource<ArrayBufferU8>,
        response: &AuthenticatorResponse,
        authenticator_attachment: Option<DOMString>,
    ) -> DomRoot<PublicKeyCredential> {
        reflect_dom_object(
            cx,
            Box::new(PublicKeyCredential::new_inherited(
                id,
                raw_id,
                response,
                authenticator_attachment,
            )),
            global,
        )
    }
}

impl PublicKeyCredentialMethods<DomTypeHolder> for PublicKeyCredential {
    /// <https://w3c.github.io/webauthn/#dom-publickeycredential-rawid>
    fn RawId(&self) -> RootedTraceableBox<HeapArrayBuffer> {
        self.raw_id
            .get_typed_array()
            .expect("rawId ArrayBuffer must be initialized")
    }

    /// <https://w3c.github.io/webauthn/#dom-publickeycredential-response>
    fn Response(&self) -> DomRoot<AuthenticatorResponse> {
        DomRoot::from_ref(&self.response)
    }

    /// <https://w3c.github.io/webauthn/#dom-publickeycredential-authenticatorattachment>
    fn GetAuthenticatorAttachment(&self) -> Option<DOMString> {
        self.authenticator_attachment
            .as_ref()
            .map(|attachment| attachment.clone().root())
    }

    /// <https://w3c.github.io/webauthn/#dom-publickeycredential-isuserverifyingplatformauthenticatoravailable>
    fn IsUserVerifyingPlatformAuthenticatorAvailable(
        cx: &mut CurrentRealm,
        _global: &Window,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        promise.resolve_native(cx, &false);
        promise
    }

    /// <https://w3c.github.io/webauthn/#dom-publickeycredential-isconditionalmediationavailable>
    fn IsConditionalMediationAvailable(cx: &mut CurrentRealm, _global: &Window) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        promise.resolve_native(cx, &false);
        promise
    }

    /// <https://w3c.github.io/webauthn/#dom-publickeycredential-getclientcapabilities>
    fn GetClientCapabilities(cx: &mut CurrentRealm, _global: &Window) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let mut capabilities = Record::<DOMString, bool>::new();
        capabilities.insert(DOMString::from_static("conditionalCreate"), false);
        capabilities.insert(DOMString::from_static("conditionalGet"), false);
        capabilities.insert(DOMString::from_static("hybridTransport"), false);
        capabilities.insert(DOMString::from_static("passkeyPlatformAuthenticator"), false);
        capabilities.insert(
            DOMString::from_static("userVerifyingPlatformAuthenticator"),
            false,
        );
        promise.resolve_native(cx, &capabilities);
        promise
    }
}
