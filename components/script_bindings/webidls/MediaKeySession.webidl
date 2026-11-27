/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/#mediakeysession-interface

enum MediaKeySessionClosedReason {
  "internal-error",
  "closed-by-application",
  "release-acknowledged",
  "hardware-context-reset",
  "resource-evicted"
};

[Exposed=Window]
interface MediaKeySession : EventTarget {
  readonly attribute DOMString sessionId;
  readonly attribute unrestricted double expiration;
  readonly attribute Promise<MediaKeySessionClosedReason> closed;
  readonly attribute MediaKeyStatusMap keyStatuses;
  attribute EventHandler onkeystatuseschange;
  attribute EventHandler onmessage;
  Promise<undefined> generateRequest(DOMString initDataType, BufferSource initData);
  Promise<boolean> load(DOMString sessionId);
  Promise<undefined> update(BufferSource response);
  Promise<undefined> close();
  Promise<undefined> remove();
};
