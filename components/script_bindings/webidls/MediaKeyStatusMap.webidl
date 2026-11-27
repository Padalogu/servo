/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/#mediakeystatusmap-interface

enum MediaKeyStatus {
  "usable",
  "expired",
  "released",
  "output-restricted",
  "output-downscaled",
  "usable-in-future",
  "status-pending",
  "internal-error"
};

[Exposed=Window]
interface MediaKeyStatusMap {
  iterable<BufferSource, MediaKeyStatus>;
  readonly attribute unsigned long size;
  boolean has(BufferSource keyId);
  (MediaKeyStatus or undefined) get(BufferSource keyId);
};
