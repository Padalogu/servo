/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use super::extcolorbufferfloat::EXTColorBufferFloat;
use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

#[dom_struct]
pub(crate) struct EXTFloatBlend {
    reflector_: Reflector,
}

impl EXTFloatBlend {
    fn new_inherited() -> EXTFloatBlend {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for EXTFloatBlend {
    type Extension = EXTFloatBlend;

    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<EXTFloatBlend> {
        reflect_dom_object(cx, Box::new(Self::new_inherited()), &*ctx.global())
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        ext.supports_gl_extension("GL_EXT_float_blend") || EXTColorBufferFloat::is_supported(ext)
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "EXT_float_blend"
    }
}
