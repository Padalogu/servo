/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/#mediakeymessageevent

enum MediaKeyMessageType {
  "license-request",
  "license-renewal",
  "license-release",
  "individualization-request"
};

[Exposed=Window]
interface MediaKeyMessageEvent : Event {
  [Throws] constructor(DOMString type, MediaKeyMessageEventInit eventInitDict);
  readonly attribute MediaKeyMessageType messageType;
  readonly attribute ArrayBuffer message;
};

dictionary MediaKeyMessageEventInit : EventInit {
  required MediaKeyMessageType messageType;
  required ArrayBuffer message;
};
