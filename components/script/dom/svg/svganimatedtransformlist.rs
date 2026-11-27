/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedTransformListBinding::SVGAnimatedTransformListMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgtransformlist::SVGTransformList;

#[dom_struct]
pub(crate) struct SVGAnimatedTransformList {
    reflector_: Reflector,
    base_val: Dom<SVGTransformList>,
}

impl SVGAnimatedTransformList {
    fn new_inherited(base_val: &SVGTransformList) -> Self {
        Self {
            reflector_: Reflector::new(),
            base_val: Dom::from_ref(base_val),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        owner: &Element,
        attr_name: LocalName,
    ) -> DomRoot<Self> {
        let base_val = SVGTransformList::new(cx, owner, attr_name);
        reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(&base_val)),
            &*owner.owner_window(),
        )
    }
}

impl SVGAnimatedTransformListMethods<crate::DomTypeHolder> for SVGAnimatedTransformList {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedTransformList__baseVal>
    fn BaseVal(&self) -> DomRoot<SVGTransformList> {
        DomRoot::from_ref(&*self.base_val)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedTransformList__animVal>
    fn AnimVal(&self) -> DomRoot<SVGTransformList> {
        DomRoot::from_ref(&*self.base_val)
    }
}
