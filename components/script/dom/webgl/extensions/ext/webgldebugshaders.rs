/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::WEBGLDebugShadersBinding::WEBGLDebugShadersMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;
use crate::dom::webgl::webglshader::WebGLShader;

#[dom_struct]
pub(crate) struct WEBGLDebugShaders {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl WEBGLDebugShaders {
    fn new_inherited(ctx: &WebGLRenderingContext) -> WEBGLDebugShaders {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }
}

impl WEBGLDebugShadersMethods<crate::DomTypeHolder> for WEBGLDebugShaders {
    /// <https://registry.khronos.org/webgl/extensions/WEBGL_debug_shaders/>
    fn GetTranslatedShaderSource(&self, shader: &WebGLShader) -> DOMString {
        if self.ctx.is_context_lost() {
            return DOMString::new();
        }
        if let Err(error) = self.ctx.validate_ownership(shader) {
            self.ctx.webgl_error(error);
            return DOMString::new();
        }
        if shader.is_deleted() || !shader.successfully_compiled() {
            return DOMString::new();
        }
        shader.translated_source()
    }
}

impl WebGLExtension for WEBGLDebugShaders {
    type Extension = WEBGLDebugShaders;

    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<WEBGLDebugShaders> {
        reflect_dom_object(cx, Box::new(Self::new_inherited(ctx)), &*ctx.global())
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(_ext: &WebGLExtensions) -> bool {
        true
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "WEBGL_debug_shaders"
    }
}
