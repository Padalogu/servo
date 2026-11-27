/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGGraphicsElementBinding::{
    SVGBoundingBoxOptions, SVGGraphicsElementMethods,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::domrect::DOMRect;
use crate::dom::element::Element;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::Node;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::svg::svganimatedtransformlist::SVGAnimatedTransformList;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svggeometryelement::SVGGeometryElement;

#[dom_struct]
pub(crate) struct SVGGraphicsElement {
    svgelement: SVGElement,
    transform: MutNullableDom<SVGAnimatedTransformList>,
}

impl SVGGraphicsElement {
    pub(crate) fn new_inherited(
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGGraphicsElement {
        SVGGraphicsElement::new_inherited_with_state(
            ElementState::empty(),
            tag_name,
            prefix,
            document,
        )
    }

    pub(crate) fn new_inherited_with_state(
        state: ElementState,
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGGraphicsElement {
        SVGGraphicsElement {
            svgelement: SVGElement::new_inherited_with_state(state, tag_name, prefix, document),
            transform: Default::default(),
        }
    }

    pub(crate) fn bounding_box(&self) -> Option<(f64, f64, f64, f64)> {
        if let Some(geometry) = self.downcast::<SVGGeometryElement>() {
            return geometry.bounding_box();
        }

        let mut union: Option<(f64, f64, f64, f64)> = None;
        for node in self
            .upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
            .skip(1)
        {
            let Some(geometry) = node.downcast::<SVGGeometryElement>() else {
                continue;
            };
            let Some((x, y, w, h)) = geometry.bounding_box() else {
                continue;
            };
            let (x2, y2) = (x + w, y + h);
            union = Some(match union {
                None => (x, y, x2, y2),
                Some((min_x, min_y, max_x, max_y)) => {
                    (min_x.min(x), min_y.min(y), max_x.max(x2), max_y.max(y2))
                },
            });
        }
        union.map(|(min_x, min_y, max_x, max_y)| (min_x, min_y, max_x - min_x, max_y - min_y))
    }
}

impl SVGGraphicsElementMethods<crate::DomTypeHolder> for SVGGraphicsElement {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGraphicsElement__transform>
    fn Transform(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedTransformList> {
        self.transform.or_init(|| {
            SVGAnimatedTransformList::new(cx, self.upcast::<Element>(), local_name!("transform"))
        })
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGraphicsElement__getBBox>
    fn GetBBox(&self, cx: &mut JSContext, _options: &SVGBoundingBoxOptions) -> DomRoot<DOMRect> {
        let (x, y, width, height) = self.bounding_box().unwrap_or((0.0, 0.0, 0.0, 0.0));
        DOMRect::new(cx, &self.global(), x, y, width, height)
    }
}

impl VirtualMethods for SVGGraphicsElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGElement>() as &dyn VirtualMethods)
    }
}
