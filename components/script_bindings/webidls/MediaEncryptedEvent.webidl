/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/#mediaencryptedevent

[Exposed=Window]
interface MediaEncryptedEvent : Event {
  [Throws] constructor(DOMString type, optional MediaEncryptedEventInit eventInitDict = {});
  readonly attribute DOMString initDataType;
  readonly attribute ArrayBuffer? initData;
};

dictionary MediaEncryptedEventInit : EventInit {
  DOMString initDataType = "";
  ArrayBuffer? initData = null;
};
