/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/#mediakeys-interface

enum MediaKeySessionType {
  "temporary",
  "persistent-license"
};

[Exposed=Window]
interface MediaKeys {
  [Throws] MediaKeySession createSession(optional MediaKeySessionType sessionType = "temporary");
  Promise<boolean> setServerCertificate(BufferSource serverCertificate);
};
