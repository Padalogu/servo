/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use script_bindings::num::Finite;
use servo_canvas_traits::canvas::Path;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGGeometryElementBinding::SVGGeometryElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::Document;
use crate::dom::dompoint::DOMPoint;
use crate::dom::element::Element;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::svg::svgcircleelement::SVGCircleElement;
use crate::dom::svg::svgellipseelement::SVGEllipseElement;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::dom::svg::svglineelement::SVGLineElement;
use crate::dom::svg::svgpathelement::SVGPathElement;
use crate::dom::svg::svgpolygonelement::SVGPolygonElement;
use crate::dom::svg::svgpolylineelement::SVGPolylineElement;
use crate::dom::svg::svgrectelement::SVGRectElement;

#[dom_struct]
pub(crate) struct SVGGeometryElement {
    svggraphicselement: SVGGraphicsElement,
}

fn parse_svg_number(element: &Element, name: &LocalName) -> Option<f64> {
    let raw = element.get_string_attribute(name);
    let s = raw.str();
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let numeric = trimmed.strip_suffix("px").unwrap_or(trimmed).trim();
    numeric.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn build_points_path(raw: &str, close: bool) -> Path {
    let mut path = Path::new();
    let mut nums = raw
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|tok| !tok.is_empty())
        .filter_map(|tok| tok.parse::<f64>().ok().filter(|v| v.is_finite()));
    let mut first = true;
    while let (Some(x), Some(y)) = (nums.next(), nums.next()) {
        if first {
            path.move_to(x, y);
            first = false;
        } else {
            path.line_to(x, y);
        }
    }
    if close && !first {
        path.close_path();
    }
    path
}

impl SVGGeometryElement {
    pub(crate) fn new_inherited(
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGGeometryElement {
        SVGGeometryElement::new_inherited_with_state(
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
    ) -> SVGGeometryElement {
        SVGGeometryElement {
            svggraphicselement: SVGGraphicsElement::new_inherited_with_state(
                state, tag_name, prefix, document,
            ),
        }
    }

    fn to_canvas_path(&self) -> Path {
        let element = self.upcast::<Element>();
        if self.is::<SVGPathElement>() {
            let d = element.get_string_attribute(&local_name!("d"));
            return Path::from_svg(&d.str());
        }
        if self.is::<SVGLineElement>() {
            let x1 = parse_svg_number(element, &local_name!("x1")).unwrap_or(0.0);
            let y1 = parse_svg_number(element, &local_name!("y1")).unwrap_or(0.0);
            let x2 = parse_svg_number(element, &local_name!("x2")).unwrap_or(0.0);
            let y2 = parse_svg_number(element, &local_name!("y2")).unwrap_or(0.0);
            let mut path = Path::new();
            path.move_to(x1, y1);
            path.line_to(x2, y2);
            return path;
        }
        if self.is::<SVGCircleElement>() {
            let cx = parse_svg_number(element, &local_name!("cx")).unwrap_or(0.0);
            let cy = parse_svg_number(element, &local_name!("cy")).unwrap_or(0.0);
            let r = parse_svg_number(element, &local_name!("r")).unwrap_or(0.0);
            return Path::from_svg_circle(cx, cy, r);
        }
        if self.is::<SVGEllipseElement>() {
            let cx = parse_svg_number(element, &local_name!("cx")).unwrap_or(0.0);
            let cy = parse_svg_number(element, &local_name!("cy")).unwrap_or(0.0);
            let rx = parse_svg_number(element, &local_name!("rx")).unwrap_or(0.0);
            let ry = parse_svg_number(element, &local_name!("ry")).unwrap_or(0.0);
            return Path::from_svg_ellipse(cx, cy, rx, ry);
        }
        if self.is::<SVGRectElement>() {
            let x = parse_svg_number(element, &local_name!("x")).unwrap_or(0.0);
            let y = parse_svg_number(element, &local_name!("y")).unwrap_or(0.0);
            let w = parse_svg_number(element, &local_name!("width")).unwrap_or(0.0);
            let h = parse_svg_number(element, &local_name!("height")).unwrap_or(0.0);
            let rx_attr = parse_svg_number(element, &local_name!("rx")).filter(|&v| v >= 0.0);
            let ry_attr = parse_svg_number(element, &local_name!("ry")).filter(|&v| v >= 0.0);
            let (rx, ry) = match (rx_attr, ry_attr) {
                (Some(rx), Some(ry)) => (rx, ry),
                (Some(rx), None) => (rx, rx),
                (None, Some(ry)) => (ry, ry),
                (None, None) => (0.0, 0.0),
            };
            return Path::from_svg_rect(x, y, w, h, rx, ry);
        }
        if self.is::<SVGPolylineElement>() {
            let pts = element.get_string_attribute(&local_name!("points"));
            return build_points_path(&pts.str(), false);
        }
        if self.is::<SVGPolygonElement>() {
            let pts = element.get_string_attribute(&local_name!("points"));
            return build_points_path(&pts.str(), true);
        }
        Path::new()
    }

    pub(crate) fn bounding_box(&self) -> Option<(f64, f64, f64, f64)> {
        let path = self.to_canvas_path();
        if path.0.elements().is_empty() {
            return None;
        }
        let rect = path.bounding_box();
        let (x, y, w, h) = (
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
        );
        if x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() {
            Some((x, y, w, h))
        } else {
            None
        }
    }
}

impl SVGGeometryElementMethods<crate::DomTypeHolder> for SVGGeometryElement {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGeometryElement__getTotalLength>
    fn GetTotalLength(&self) -> Finite<f32> {
        let len = self.to_canvas_path().total_length() as f32;
        if len.is_finite() && len >= 0.0 {
            Finite::wrap(len)
        } else {
            Finite::wrap(0.0)
        }
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGeometryElement__getPointAtLength>
    fn GetPointAtLength(&self, cx: &mut JSContext, distance: Finite<f32>) -> DomRoot<DOMPoint> {
        let (x, y) = self.to_canvas_path().point_at_length(*distance as f64);
        DOMPoint::new(cx, &self.global(), x, y, 0.0, 1.0)
    }
}

impl VirtualMethods for SVGGeometryElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }
}
