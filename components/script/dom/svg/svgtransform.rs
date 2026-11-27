/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use euclid::default::{Transform2D, Transform3D};
use js::context::JSContext;
use script_bindings::num::Finite;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::DOMMatrixBinding::DOMMatrix2DInit;
use crate::dom::bindings::codegen::Bindings::SVGTransformBinding::{
    SVGTransformConstants, SVGTransformMethods,
};
use crate::dom::bindings::error::ErrorResult;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::dommatrix::DOMMatrix;
use crate::dom::dommatrixreadonly::{DOMMatrixReadOnly, dommatrix2dinit_to_matrix};
use crate::dom::globalscope::GlobalScope;
use crate::dom::svg::svgtransformlist::SVGTransformList;

#[dom_struct]
pub(crate) struct SVGTransform {
    reflector_: Reflector,
    transform_type: Cell<u16>,
    angle: Cell<f32>,
    cx_val: Cell<f32>,
    cy_val: Cell<f32>,
    mat_a: Cell<f64>,
    mat_b: Cell<f64>,
    mat_c: Cell<f64>,
    mat_d: Cell<f64>,
    mat_e: Cell<f64>,
    mat_f: Cell<f64>,
    matrix_obj: MutNullableDom<DOMMatrix>,
    owner_list: MutNullableDom<SVGTransformList>,
}

impl SVGTransform {
    fn new_inherited() -> Self {
        Self {
            reflector_: Reflector::new(),
            transform_type: Cell::new(SVGTransformConstants::SVG_TRANSFORM_MATRIX),
            angle: Cell::new(0.0),
            cx_val: Cell::new(0.0),
            cy_val: Cell::new(0.0),
            mat_a: Cell::new(1.0),
            mat_b: Cell::new(0.0),
            mat_c: Cell::new(0.0),
            mat_d: Cell::new(1.0),
            mat_e: Cell::new(0.0),
            mat_f: Cell::new(0.0),
            matrix_obj: Default::default(),
            owner_list: Default::default(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited()), global)
    }

    pub(crate) fn new_with_matrix(
        cx: &mut JSContext,
        global: &GlobalScope,
        m: Transform2D<f64>,
    ) -> DomRoot<Self> {
        let item = Self::new_inherited();
        item.set_matrix_2d([m.m11, m.m12, m.m21, m.m22, m.m31, m.m32]);
        reflect_dom_object(cx, Box::new(item), global)
    }

    pub(crate) fn set_owner(&self, owner: Option<&SVGTransformList>) {
        self.owner_list.set(owner);
    }

    pub(crate) fn owner(&self) -> Option<DomRoot<SVGTransformList>> {
        self.owner_list.get()
    }

    pub(crate) fn matrix_2d_array(&self) -> [f64; 6] {
        [
            self.mat_a.get(),
            self.mat_b.get(),
            self.mat_c.get(),
            self.mat_d.get(),
            self.mat_e.get(),
            self.mat_f.get(),
        ]
    }

    fn set_matrix_2d(&self, m: [f64; 6]) {
        self.mat_a.set(m[0]);
        self.mat_b.set(m[1]);
        self.mat_c.set(m[2]);
        self.mat_d.set(m[3]);
        self.mat_e.set(m[4]);
        self.mat_f.set(m[5]);
    }

    fn transform_3d(&self) -> Transform3D<f64> {
        let [a, b, c, d, e, f] = self.matrix_2d_array();
        Transform3D::new(
            a, b, 0.0, 0.0, //
            c, d, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            e, f, 0.0, 1.0,
        )
    }

    fn apply_update(&self, cx: Option<&mut JSContext>) {
        if let Some(matrix_obj) = self.matrix_obj.get() {
            matrix_obj
                .upcast::<DOMMatrixReadOnly>()
                .set_matrix(self.transform_3d());
        }
        if let Some(cx) = cx
            && let Some(owner) = self.owner_list.get()
        {
            owner.sync_attribute(cx);
        }
    }

    pub(crate) fn to_svg_string(&self) -> String {
        let [a, b, c, d, e, f] = self.matrix_2d_array();
        match self.transform_type.get() {
            SVGTransformConstants::SVG_TRANSFORM_TRANSLATE => {
                if f == 0.0 {
                    format!("translate({e})")
                } else {
                    format!("translate({e} {f})")
                }
            },
            SVGTransformConstants::SVG_TRANSFORM_SCALE => {
                if a == d {
                    format!("scale({a})")
                } else {
                    format!("scale({a} {d})")
                }
            },
            SVGTransformConstants::SVG_TRANSFORM_ROTATE => {
                let angle = self.angle.get();
                let cx = self.cx_val.get();
                let cy = self.cy_val.get();
                if cx == 0.0 && cy == 0.0 {
                    format!("rotate({angle})")
                } else {
                    format!("rotate({angle} {cx} {cy})")
                }
            },
            SVGTransformConstants::SVG_TRANSFORM_SKEWX => {
                format!("skewX({})", self.angle.get())
            },
            SVGTransformConstants::SVG_TRANSFORM_SKEWY => {
                format!("skewY({})", self.angle.get())
            },
            _ => format!("matrix({a} {b} {c} {d} {e} {f})"),
        }
    }
}

impl SVGTransformMethods<crate::DomTypeHolder> for SVGTransform {
    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__type>
    fn Type(&self) -> u16 {
        self.transform_type.get()
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__matrix>
    fn Matrix(&self, cx: &mut JSContext) -> DomRoot<DOMMatrix> {
        self.matrix_obj
            .or_init(|| DOMMatrix::new(cx, &self.global(), true, self.transform_3d()))
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__angle>
    fn Angle(&self) -> f32 {
        self.angle.get()
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__setMatrix>
    fn SetMatrix(&self, cx: &mut JSContext, matrix: &DOMMatrix2DInit) -> ErrorResult {
        let m = dommatrix2dinit_to_matrix(matrix)?;
        self.transform_type
            .set(SVGTransformConstants::SVG_TRANSFORM_MATRIX);
        self.angle.set(0.0);
        self.cx_val.set(0.0);
        self.cy_val.set(0.0);
        self.set_matrix_2d([m.m11, m.m12, m.m21, m.m22, m.m31, m.m32]);
        self.apply_update(Some(cx));
        Ok(())
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__setTranslate>
    fn SetTranslate(&self, cx: &mut JSContext, tx: Finite<f32>, ty: Finite<f32>) {
        self.transform_type
            .set(SVGTransformConstants::SVG_TRANSFORM_TRANSLATE);
        self.angle.set(0.0);
        self.cx_val.set(0.0);
        self.cy_val.set(0.0);
        self.set_matrix_2d([1.0, 0.0, 0.0, 1.0, *tx as f64, *ty as f64]);
        self.apply_update(Some(cx));
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__setScale>
    fn SetScale(&self, cx: &mut JSContext, sx: Finite<f32>, sy: Finite<f32>) {
        self.transform_type
            .set(SVGTransformConstants::SVG_TRANSFORM_SCALE);
        self.angle.set(0.0);
        self.cx_val.set(0.0);
        self.cy_val.set(0.0);
        self.set_matrix_2d([*sx as f64, 0.0, 0.0, *sy as f64, 0.0, 0.0]);
        self.apply_update(Some(cx));
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__setRotate>
    fn SetRotate(
        &self,
        cx: &mut JSContext,
        angle: Finite<f32>,
        cx_arg: Finite<f32>,
        cy_arg: Finite<f32>,
    ) {
        let a = *angle as f64;
        let cx_f = *cx_arg as f64;
        let cy_f = *cy_arg as f64;
        let rad = a.to_radians();
        let cos = rad.cos();
        let sin = rad.sin();
        let e = cx_f - cx_f * cos + cy_f * sin;
        let f = cy_f - cx_f * sin - cy_f * cos;

        self.transform_type
            .set(SVGTransformConstants::SVG_TRANSFORM_ROTATE);
        self.angle.set(*angle);
        self.cx_val.set(*cx_arg);
        self.cy_val.set(*cy_arg);
        self.set_matrix_2d([cos, sin, -sin, cos, e, f]);
        self.apply_update(Some(cx));
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__setSkewX>
    fn SetSkewX(&self, cx: &mut JSContext, angle: Finite<f32>) {
        let tan = (*angle as f64).to_radians().tan();
        self.transform_type
            .set(SVGTransformConstants::SVG_TRANSFORM_SKEWX);
        self.angle.set(*angle);
        self.cx_val.set(0.0);
        self.cy_val.set(0.0);
        self.set_matrix_2d([1.0, 0.0, tan, 1.0, 0.0, 0.0]);
        self.apply_update(Some(cx));
    }

    /// <https://svgwg.org/svg2-draft/coords.html#__svg__SVGTransform__setSkewY>
    fn SetSkewY(&self, cx: &mut JSContext, angle: Finite<f32>) {
        let tan = (*angle as f64).to_radians().tan();
        self.transform_type
            .set(SVGTransformConstants::SVG_TRANSFORM_SKEWY);
        self.angle.set(*angle);
        self.cx_val.set(0.0);
        self.cy_val.set(0.0);
        self.set_matrix_2d([1.0, tan, 0.0, 1.0, 0.0, 0.0]);
        self.apply_update(Some(cx));
    }
}
