/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use euclid::default::Transform2D;
use html5ever::{LocalName, ns};
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::num::Finite;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::str::DOMString;

use crate::dom::bindings::codegen::Bindings::DOMMatrixBinding::DOMMatrix2DInit;
use crate::dom::bindings::codegen::Bindings::SVGTransformBinding::SVGTransformMethods;
use crate::dom::bindings::codegen::Bindings::SVGTransformListBinding::SVGTransformListMethods;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::dommatrixreadonly::dommatrix2dinit_to_matrix;
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgtransform::SVGTransform;

#[dom_struct]
pub(crate) struct SVGTransformList {
    reflector_: Reflector,
    owner: Dom<Element>,
    #[no_trace]
    attr_name: LocalName,
    items: DomRefCell<Vec<Dom<SVGTransform>>>,
}

fn parse_numbers(args: &str) -> Vec<f64> {
    args.split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|tok| !tok.is_empty())
        .filter_map(|tok| tok.parse::<f64>().ok().filter(|v| v.is_finite()))
        .collect()
}

fn parse_transform_attribute(
    cx: &mut JSContext,
    owner: &Element,
    attr_name: &LocalName,
) -> Vec<DomRoot<SVGTransform>> {
    let raw = owner.get_string_attribute(attr_name);
    let text = raw.str();
    let global = owner.owner_window();
    let global = global.as_global_scope();
    let mut out = Vec::new();
    let mut rest = text.trim();

    while !rest.is_empty() {
        let Some(open) = rest.find('(') else {
            break;
        };
        let Some(close_rel) = rest[open + 1..].find(')') else {
            break;
        };
        let name = rest[..open].trim().trim_start_matches(',');
        let args = &rest[open + 1..open + 1 + close_rel];
        let nums = parse_numbers(args);

        let item = SVGTransform::new(cx, global);
        let mut parsed = true;
        match name {
            "translate" if !nums.is_empty() => {
                if let (Some(tx), Some(ty)) = (
                    Finite::new(nums[0] as f32),
                    Finite::new(*nums.get(1).unwrap_or(&0.0) as f32),
                ) {
                    SVGTransformMethods::<crate::DomTypeHolder>::SetTranslate(&*item, cx, tx, ty);
                } else {
                    parsed = false;
                }
            },
            "scale" if !nums.is_empty() => {
                let sy_raw = *nums.get(1).unwrap_or(&nums[0]);
                if let (Some(sx), Some(sy)) =
                    (Finite::new(nums[0] as f32), Finite::new(sy_raw as f32))
                {
                    SVGTransformMethods::<crate::DomTypeHolder>::SetScale(&*item, cx, sx, sy);
                } else {
                    parsed = false;
                }
            },
            "rotate" if !nums.is_empty() => {
                let cx_raw = *nums.get(1).unwrap_or(&0.0);
                let cy_raw = *nums.get(2).unwrap_or(&0.0);
                if let (Some(a), Some(cx_f), Some(cy_f)) = (
                    Finite::new(nums[0] as f32),
                    Finite::new(cx_raw as f32),
                    Finite::new(cy_raw as f32),
                ) {
                    SVGTransformMethods::<crate::DomTypeHolder>::SetRotate(
                        &*item, cx, a, cx_f, cy_f,
                    );
                } else {
                    parsed = false;
                }
            },
            "skewX" if !nums.is_empty() => {
                if let Some(a) = Finite::new(nums[0] as f32) {
                    SVGTransformMethods::<crate::DomTypeHolder>::SetSkewX(&*item, cx, a);
                } else {
                    parsed = false;
                }
            },
            "skewY" if !nums.is_empty() => {
                if let Some(a) = Finite::new(nums[0] as f32) {
                    SVGTransformMethods::<crate::DomTypeHolder>::SetSkewY(&*item, cx, a);
                } else {
                    parsed = false;
                }
            },
            "matrix" if nums.len() >= 6 => {
                let init = DOMMatrix2DInit {
                    a: Some(nums[0]),
                    b: Some(nums[1]),
                    c: Some(nums[2]),
                    d: Some(nums[3]),
                    e: Some(nums[4]),
                    f: Some(nums[5]),
                    m11: None,
                    m12: None,
                    m21: None,
                    m22: None,
                    m41: None,
                    m42: None,
                };
                if SVGTransformMethods::<crate::DomTypeHolder>::SetMatrix(&*item, cx, &init).is_err()
                {
                    parsed = false;
                }
            },
            _ => {
                parsed = false;
            },
        }
        if parsed {
            out.push(item);
        }
        rest = rest[open + 1 + close_rel + 1..].trim();
    }

    out
}

impl SVGTransformList {
    fn new_inherited(
        owner: &Element,
        attr_name: LocalName,
        initial: Vec<DomRoot<SVGTransform>>,
    ) -> Self {
        Self {
            reflector_: Reflector::new(),
            owner: Dom::from_ref(owner),
            attr_name,
            items: DomRefCell::new(initial.iter().map(|i| Dom::from_ref(&**i)).collect()),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        owner: &Element,
        attr_name: LocalName,
    ) -> DomRoot<Self> {
        let initial = parse_transform_attribute(cx, owner, &attr_name);
        let list = reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(owner, attr_name, initial)),
            &*owner.owner_window(),
        );
        for item in list.items.borrow().iter() {
            item.set_owner(Some(&list));
        }
        list
    }

    pub(crate) fn sync_attribute(&self, cx: &mut JSContext) {
        let serialized = {
            let items = self.items.borrow();
            if items.is_empty() {
                None
            } else {
                let parts: Vec<String> = items.iter().map(|i| i.to_svg_string()).collect();
                Some(parts.join(" "))
            }
        };
        match serialized {
            Some(value) => {
                self.owner
                    .set_string_attribute(cx, &self.attr_name, DOMString::from(value));
            },
            None => {
                self.owner.remove_attribute(cx, &ns!(), &self.attr_name);
            },
        }
    }

    fn detach_from_previous_owner(&self, item: &SVGTransform) {
        if let Some(prev_owner) = item.owner() {
            if std::ptr::eq(&*prev_owner as *const Self, self as *const Self) {
                let mut items = self.items.borrow_mut();
                if let Some(pos) = items
                    .iter()
                    .position(|existing| std::ptr::eq(&**existing, item))
                {
                    items.remove(pos);
                }
            } else {
                let mut prev_items = prev_owner.items.borrow_mut();
                if let Some(pos) = prev_items
                    .iter()
                    .position(|existing| std::ptr::eq(&**existing, item))
                {
                    prev_items.remove(pos);
                }
            }
            item.set_owner(None);
        }
    }

    fn clear_items(&self) {
        let mut items = self.items.borrow_mut();
        for item in items.drain(..) {
            item.set_owner(None);
        }
    }
}

impl SVGTransformListMethods<crate::DomTypeHolder> for SVGTransformList {
    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__length>
    fn Length(&self) -> u32 {
        self.items.borrow().len() as u32
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__numberOfItems>
    fn NumberOfItems(&self) -> u32 {
        self.items.borrow().len() as u32
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__clear>
    fn Clear(&self, cx: &mut JSContext) -> ErrorResult {
        self.clear_items();
        self.sync_attribute(cx);
        Ok(())
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__initialize>
    fn Initialize(
        &self,
        cx: &mut JSContext,
        new_item: &SVGTransform,
    ) -> Fallible<DomRoot<SVGTransform>> {
        self.detach_from_previous_owner(new_item);
        self.clear_items();
        new_item.set_owner(Some(self));
        self.items.borrow_mut().push(Dom::from_ref(new_item));
        self.sync_attribute(cx);
        Ok(DomRoot::from_ref(new_item))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__getItem>
    fn GetItem(&self, index: u32) -> Fallible<DomRoot<SVGTransform>> {
        self.items
            .borrow()
            .get(index as usize)
            .map(Dom::as_rooted)
            .ok_or(Error::IndexSize(None))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__insertItemBefore>
    fn InsertItemBefore(
        &self,
        cx: &mut JSContext,
        new_item: &SVGTransform,
        index: u32,
    ) -> Fallible<DomRoot<SVGTransform>> {
        self.detach_from_previous_owner(new_item);
        {
            let mut items = self.items.borrow_mut();
            let pos = (index as usize).min(items.len());
            new_item.set_owner(Some(self));
            items.insert(pos, Dom::from_ref(new_item));
        }
        self.sync_attribute(cx);
        Ok(DomRoot::from_ref(new_item))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__replaceItem>
    fn ReplaceItem(
        &self,
        cx: &mut JSContext,
        new_item: &SVGTransform,
        index: u32,
    ) -> Fallible<DomRoot<SVGTransform>> {
        let len = self.items.borrow().len();
        if (index as usize) >= len {
            return Err(Error::IndexSize(None));
        }
        self.detach_from_previous_owner(new_item);
        {
            let mut items = self.items.borrow_mut();
            let pos = (index as usize).min(items.len().saturating_sub(1));
            if pos < items.len() {
                items[pos].set_owner(None);
                new_item.set_owner(Some(self));
                items[pos] = Dom::from_ref(new_item);
            } else {
                new_item.set_owner(Some(self));
                items.push(Dom::from_ref(new_item));
            }
        }
        self.sync_attribute(cx);
        Ok(DomRoot::from_ref(new_item))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__removeItem>
    fn RemoveItem(&self, cx: &mut JSContext, index: u32) -> Fallible<DomRoot<SVGTransform>> {
        let removed = {
            let mut items = self.items.borrow_mut();
            if (index as usize) >= items.len() {
                return Err(Error::IndexSize(None));
            }
            let item = items.remove(index as usize);
            item.set_owner(None);
            DomRoot::from_ref(&*item)
        };
        self.sync_attribute(cx);
        Ok(removed)
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__appendItem>
    fn AppendItem(
        &self,
        cx: &mut JSContext,
        new_item: &SVGTransform,
    ) -> Fallible<DomRoot<SVGTransform>> {
        self.detach_from_previous_owner(new_item);
        new_item.set_owner(Some(self));
        self.items.borrow_mut().push(Dom::from_ref(new_item));
        self.sync_attribute(cx);
        Ok(DomRoot::from_ref(new_item))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__IndexedSetter>
    fn IndexedSetter(&self, cx: &mut JSContext, index: u32, new_item: &SVGTransform) -> ErrorResult {
        self.ReplaceItem(cx, new_item, index).map(|_| ())
    }

    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SVGTransform>> {
        self.items.borrow().get(index as usize).map(Dom::as_rooted)
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__createSVGTransformFromMatrix>
    fn CreateSVGTransformFromMatrix(
        &self,
        cx: &mut JSContext,
        matrix: &DOMMatrix2DInit,
    ) -> Fallible<DomRoot<SVGTransform>> {
        let m = dommatrix2dinit_to_matrix(matrix)?;
        Ok(SVGTransform::new_with_matrix(cx, &self.global(), m))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransformList__consolidate>
    fn Consolidate(&self, cx: &mut JSContext) -> Fallible<Option<DomRoot<SVGTransform>>> {
        let matrices: Vec<[f64; 6]> = self
            .items
            .borrow()
            .iter()
            .map(|i| i.matrix_2d_array())
            .collect();
        if matrices.is_empty() {
            return Ok(None);
        }

        let mut acc = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        for [a2, b2, c2, d2, e2, f2] in matrices {
            let [a1, b1, c1, d1, e1, f1] = acc;
            acc = [
                a1 * a2 + c1 * b2,
                b1 * a2 + d1 * b2,
                a1 * c2 + c1 * d2,
                b1 * c2 + d1 * d2,
                a1 * e2 + c1 * f2 + e1,
                b1 * e2 + d1 * f2 + f1,
            ];
        }

        let consolidated = SVGTransform::new_with_matrix(
            cx,
            &self.global(),
            Transform2D::new(acc[0], acc[1], acc[2], acc[3], acc[4], acc[5]),
        );
        self.clear_items();
        consolidated.set_owner(Some(self));
        self.items.borrow_mut().push(Dom::from_ref(&*consolidated));
        self.sync_attribute(cx);
        Ok(Some(consolidated))
    }
}
