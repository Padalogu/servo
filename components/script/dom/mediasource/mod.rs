/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#[expect(clippy::module_inception, reason = "The interface name is MediaSource")]
pub(crate) mod mediasource;
pub(crate) mod sourcebuffer;
pub(crate) mod sourcebufferlist;

pub(crate) use self::mediasource::*;
