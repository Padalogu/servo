/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::local_name;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedStringBinding::SVGAnimatedStringMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;

#[dom_struct]
pub(crate) struct SVGAnimatedString {
    reflector_: Reflector,
    element: Dom<Element>,
}

impl SVGAnimatedString {
    fn new_inherited(element: &Element) -> SVGAnimatedString {
        SVGAnimatedString {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, element: &Element) -> DomRoot<SVGAnimatedString> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedString::new_inherited(element)),
            &*element.owner_window(),
        )
    }
}

impl SVGAnimatedStringMethods<crate::DomTypeHolder> for SVGAnimatedString {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn BaseVal(&self) -> DOMString {
        self.element.get_string_attribute(&local_name!("class"))
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: DOMString) {
        self.element
            .set_tokenlist_attribute(cx, &local_name!("class"), value);
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__animVal>
    fn AnimVal(&self) -> DOMString {
        self.BaseVal()
    }
}
