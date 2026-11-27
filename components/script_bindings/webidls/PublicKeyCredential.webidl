/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://w3c.github.io/webauthn/#publickeycredential
 */

// https://w3c.github.io/webauthn/#publickeycredential
[SecureContext, Exposed=Window, Pref="dom_credential_management_enabled"]
interface PublicKeyCredential : Credential {
  readonly attribute ArrayBuffer rawId;
  readonly attribute AuthenticatorResponse response;
  readonly attribute DOMString? authenticatorAttachment;
  static Promise<boolean> isUserVerifyingPlatformAuthenticatorAvailable();
  static Promise<boolean> isConditionalMediationAvailable();
  static Promise<record<DOMString, boolean>> getClientCapabilities();
};

// https://w3c.github.io/webauthn/#authenticatorresponse
[SecureContext, Exposed=Window, Pref="dom_credential_management_enabled"]
interface AuthenticatorResponse {
  readonly attribute ArrayBuffer clientDataJSON;
};
