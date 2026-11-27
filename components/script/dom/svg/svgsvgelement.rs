/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::Parser;
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::SVGElementData;
use servo_arc::Arc as ServoArc;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::color::AbsoluteColor;
use style::custom_properties;
use style::parser::ParserContext;
use style::properties::ComputedValues;
use style::stylesheets::Origin;
use style::values::computed::SVGPaint;
use style::values::generics::svg::SVGPaintKind;
use style::values::specified::{Color as SpecifiedColor, LengthPercentage};
use style_traits::{ParsingMode, ToCss};
use stylo_atoms::Atom;
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::DOMMatrixBinding::DOMMatrix2DInit;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::SVGSVGElementBinding::SVGSVGElementMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::dommatrixreadonly::dommatrix2dinit_to_matrix;
use crate::dom::svg::svgtransform::SVGTransform;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
    needs_reserialization: Cell<bool>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            cached_serialized_data_url: Default::default(),
            needs_reserialization: Cell::new(true),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn mark_subtree_dirty(&self, no_gc: &NoGC) {
        if !self.needs_reserialization.replace(true) {
            self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
        }
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) -> bool {
        self.needs_reserialization.set(false);
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return false;
        }

        let doc = self.owner_document();
        let url = doc.url().into_url().into();
        let parser_context = ParserContext::new(
            Origin::Author,
            &url,
            None,
            ParsingMode::DEFAULT,
            doc.quirks_mode(),
            /* namespaces = */ Default::default(),
            None,
            None,
            /* attr_taint = */ Default::default(),
        );
        let mut use_element_styles = Vec::new();
        resolve_subtree_styles(
            cx,
            self.upcast(),
            &cloned_node,
            None,
            None,
            true,
            &parser_context,
            &mut use_element_styles,
        );

        let root_fallback_style = effective_computed_style(self.upcast(), None);
        self.process_use_elements(
            cx,
            &cloned_node,
            &parser_context,
            &use_element_styles,
            root_fallback_style.as_ref(),
        );

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return false;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(new_url) => {
                let prev_url = self.cached_serialized_data_url.borrow().clone();
                if let Some(Ok(old_url)) = prev_url {
                    if old_url == new_url {
                        return false;
                    }
                    let owner_window = self.owner_window();
                    owner_window.layout_mut().remove_cached_image(&old_url);
                    owner_window.image_cache().evict_completed_image(
                        &old_url,
                        owner_window.origin().immutable(),
                        &None,
                    );
                }
                *self.cached_serialized_data_url.borrow_mut() = Some(Ok(new_url));
                true
            },
            Err(error) => {
                error!("Unable to parse serialized SVG data url: {error}");
                *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
                false
            },
        }
    }

    fn process_use_elements(
        &self,
        cx: &mut JSContext,
        root_node: &Node,
        parser_context: &ParserContext<'_>,
        use_element_styles: &[(DomRoot<Element>, Option<ServoArc<ComputedValues>>)],
        root_fallback_style: Option<&ServoArc<ComputedValues>>,
    ) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use")
            {
                let fallback_style = use_element_styles
                    .iter()
                    .find(|(use_el, _)| **use_el == *element)
                    .and_then(|(_, style)| style.as_ref())
                    .or(root_fallback_style);
                self.process_single_use_element(
                    cx,
                    element,
                    root_node,
                    parser_context,
                    fallback_style,
                );
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
        parser_context: &ParserContext<'_>,
        fallback_style: Option<&ServoArc<ComputedValues>>,
    ) {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let Some(id_string) = href.str().strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        let mut nested_use_styles = Vec::new();
        resolve_subtree_styles(
            cx,
            referenced_node,
            &cloned_node,
            fallback_style,
            None,
            false,
            parser_context,
            &mut nested_use_styles,
        );
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    fn invalidate_cached_serialized_subtree_and_rasterization_result(&self, no_gc: &NoGC) {
        let owner_window = self.owner_window();
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.needs_reserialization.set(true);
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            needs_serialization: self.unsafe_get().needs_reserialization.get(),
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl SVGSVGElementMethods<crate::DomTypeHolder> for SVGSVGElement {
    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__createSVGTransform>
    fn CreateSVGTransform(&self, cx: &mut JSContext) -> DomRoot<SVGTransform> {
        SVGTransform::new(cx, &self.global())
    }

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__createSVGTransformFromMatrix>
    fn CreateSVGTransformFromMatrix(
        &self,
        cx: &mut JSContext,
        matrix: &DOMMatrix2DInit,
    ) -> Fallible<DomRoot<SVGTransform>> {
        let m = dommatrix2dinit_to_matrix(matrix)?;
        Ok(SVGTransform::new_with_matrix(cx, &self.global(), m))
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser = &mut Parser::new(value);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}

const MAX_VAR_DEPTH: usize = 8;

#[derive(Clone, Copy, PartialEq)]
enum ResolvedSvgPaint {
    None,
    Color(AbsoluteColor),
}

#[derive(Clone, Copy)]
struct ParentResolvedState {
    color: AbsoluteColor,
    fill: Option<ResolvedSvgPaint>,
    stroke: Option<ResolvedSvgPaint>,
}

/// Returns the computed style of `node`, or its nearest styled ancestor (or `fallback`
/// for unstyled `<use>` targets inside `display: none` sprites) without triggering reflow.
fn effective_computed_style(
    node: &Node,
    fallback: Option<&ServoArc<ComputedValues>>,
) -> Option<ServoArc<ComputedValues>> {
    for ancestor in node.inclusive_ancestors(ShadowIncluding::No) {
        if let Some(element) = ancestor.downcast::<Element>() &&
            let Some(style) = element.style_without_reflow()
        {
            return Some(style);
        }
    }
    fallback.cloned()
}

fn format_srgb_legacy(color: AbsoluteColor) -> String {
    color.into_srgb_legacy().to_css_string()
}

fn resolve_svg_paint(
    paint: &SVGPaint,
    current_color: &AbsoluteColor,
) -> Option<ResolvedSvgPaint> {
    match &paint.kind {
        SVGPaintKind::None => Some(ResolvedSvgPaint::None),
        SVGPaintKind::Color(color) => {
            Some(ResolvedSvgPaint::Color(color.resolve_to_absolute(current_color)))
        },
        _ => None,
    }
}

fn paint_to_attr_string(paint: ResolvedSvgPaint) -> String {
    match paint {
        ResolvedSvgPaint::None => String::from("none"),
        ResolvedSvgPaint::Color(color) => format_srgb_legacy(color),
    }
}

/// Resolves CSS `var(...)`, `currentColor`, and computed `color`/`fill`/`stroke` onto
/// `cloned_node` by reading computed styles from the parallel live DOM `orig_node`.
#[expect(clippy::too_many_arguments)]
fn resolve_subtree_styles(
    cx: &mut JSContext,
    orig_node: &Node,
    cloned_node: &Node,
    fallback_style: Option<&ServoArc<ComputedValues>>,
    parent_state: Option<ParentResolvedState>,
    is_root: bool,
    parser_context: &ParserContext<'_>,
    use_element_styles: &mut Vec<(DomRoot<Element>, Option<ServoArc<ComputedValues>>)>,
) {
    let next_parent_state = if let (Some(orig_elem), Some(cloned_elem)) =
        (orig_node.downcast::<Element>(), cloned_node.downcast::<Element>())
    {
        let own_style = orig_elem.style_without_reflow();
        let effective_style = own_style
            .as_ref()
            .cloned()
            .or_else(|| effective_computed_style(orig_node, fallback_style));

        if cloned_elem.local_name() == &local_name!("use") {
            use_element_styles.push((DomRoot::from_ref(cloned_elem), effective_style.clone()));
        }

        if let Some(style) = effective_style.as_deref() {
            let computed_color = style.get_inherited_text().color;
            let current_color_css = format_srgb_legacy(computed_color);
            let lookup_var = |raw_name: &str| -> Option<String> {
                let stripped = custom_properties::parse_name(raw_name.trim()).ok()?;
                let name = Atom::from(stripped);
                style
                    .custom_properties()
                    .get_for_cssom(&name)
                    .map(|val| val.to_css_string())
            };

            resolve_element_presentation_attrs(
                cx,
                cloned_elem,
                &lookup_var,
                Some(computed_color),
                Some(current_color_css.as_str()),
                parser_context,
            );

            let style_flags = if cloned_elem.has_attribute(&local_name!("style")) {
                let raw_style = cloned_elem.get_string_attribute(&local_name!("style"));
                let (resolved_style, flags) = resolve_style_attribute(
                    &raw_style.str(),
                    &lookup_var,
                    Some(computed_color),
                    Some(current_color_css.as_str()),
                    parser_context,
                );
                if resolved_style != *raw_style.str() {
                    cloned_elem.set_attribute(
                        cx,
                        &local_name!("style"),
                        AttrValue::String(resolved_style),
                    );
                }
                flags
            } else {
                InlineStyleColorFlags::default()
            };

            // Inject computed `color` on the root `<svg>` (or descendants whose CSS `color`
            // differs from their parent) so `usvg` resolves `currentColor` against HTML CSS.
            let has_resolved_color_attr = if cloned_elem.has_attribute(&local_name!("color")) {
                let color_attr = cloned_elem.get_string_attribute(&local_name!("color"));
                let color_str = color_attr.str();
                let trimmed = color_str.trim();
                !contains_unresolved_var(trimmed) && !trimmed.eq_ignore_ascii_case("inherit")
            } else {
                false
            };
            if !has_resolved_color_attr &&
                !style_flags.has_color &&
                (is_root || parent_state.is_some_and(|p| p.color != computed_color))
            {
                cloned_elem.set_attribute(
                    cx,
                    &local_name!("color"),
                    AttrValue::String(current_color_css),
                );
            }

            // If Stylo computed `fill` or `stroke` via HTML/CSS rules (and no explicit
            // attribute or inline style declaration overrides it), inject it on the clone.
            let svg_style = style.get_inherited_svg();
            let resolved_fill = resolve_svg_paint(&svg_style.fill, &computed_color);
            let resolved_stroke = resolve_svg_paint(&svg_style.stroke, &computed_color);

            if own_style.is_some() {
                let parent_fill = if is_root {
                    Some(ResolvedSvgPaint::Color(AbsoluteColor::BLACK))
                } else {
                    parent_state.and_then(|p| p.fill)
                };
                if !cloned_elem.has_attribute(&local_name!("fill")) &&
                    !style_flags.has_fill &&
                    let Some(fill) = resolved_fill &&
                    Some(fill) != parent_fill
                {
                    cloned_elem.set_attribute(
                        cx,
                        &local_name!("fill"),
                        AttrValue::String(paint_to_attr_string(fill)),
                    );
                }

                let parent_stroke = if is_root {
                    Some(ResolvedSvgPaint::None)
                } else {
                    parent_state.and_then(|p| p.stroke)
                };
                if !cloned_elem.has_attribute(&local_name!("stroke")) &&
                    !style_flags.has_stroke &&
                    let Some(stroke) = resolved_stroke &&
                    Some(stroke) != parent_stroke
                {
                    cloned_elem.set_attribute(
                        cx,
                        &local_name!("stroke"),
                        AttrValue::String(paint_to_attr_string(stroke)),
                    );
                }
            }

            Some(ParentResolvedState {
                color: computed_color,
                fill: resolved_fill,
                stroke: resolved_stroke,
            })
        } else {
            parent_state
        }
    } else {
        parent_state
    };

    for (orig_child, cloned_child) in orig_node.children().zip(cloned_node.children()) {
        resolve_subtree_styles(
            cx,
            &orig_child,
            &cloned_child,
            fallback_style,
            next_parent_state,
            false,
            parser_context,
            use_element_styles,
        );
    }
}

fn resolve_element_presentation_attrs(
    cx: &mut JSContext,
    cloned_elem: &Element,
    lookup_var: &impl Fn(&str) -> Option<String>,
    current_color: Option<AbsoluteColor>,
    current_color_css: Option<&str>,
    parser_context: &ParserContext<'_>,
) {
    let attrs = [
        local_name!("fill"),
        local_name!("stroke"),
        local_name!("color"),
        local_name!("stop-color"),
        local_name!("flood-color"),
        local_name!("lighting-color"),
    ];
    for attr_name in &attrs {
        if !cloned_elem.has_attribute(attr_name) {
            continue;
        }
        let raw_value = cloned_elem.get_string_attribute(attr_name);
        let resolved = resolve_svg_paint_value(
            &raw_value.str(),
            lookup_var,
            current_color,
            current_color_css,
            Some(parser_context),
        );
        if resolved != *raw_value.str() {
            cloned_elem.set_attribute(cx, attr_name, AttrValue::String(resolved));
        }
    }
}

#[derive(Default)]
struct InlineStyleColorFlags {
    has_color: bool,
    has_fill: bool,
    has_stroke: bool,
}

fn resolve_style_attribute(
    raw_style: &str,
    lookup_var: &impl Fn(&str) -> Option<String>,
    current_color: Option<AbsoluteColor>,
    current_color_css: Option<&str>,
    parser_context: &ParserContext<'_>,
) -> (String, InlineStyleColorFlags) {
    resolve_style_attribute_inner(
        raw_style,
        lookup_var,
        current_color,
        current_color_css,
        Some(parser_context),
    )
}

fn resolve_style_attribute_inner(
    raw_style: &str,
    lookup_var: &impl Fn(&str) -> Option<String>,
    current_color: Option<AbsoluteColor>,
    current_color_css: Option<&str>,
    parser_context: Option<&ParserContext<'_>>,
) -> (String, InlineStyleColorFlags) {
    let mut flags = InlineStyleColorFlags::default();
    let mut out_decls = Vec::new();

    for decl in split_css_declarations(raw_style) {
        let Some((name, value)) = split_css_declaration(decl) else {
            out_decls.push(decl.to_owned());
            continue;
        };
        let trimmed_name = name.trim();
        if trimmed_name.starts_with("--") {
            out_decls.push(decl.to_owned());
            continue;
        }

        let (core_value, important) = split_important_suffix(value);
        let resolved_core = if is_svg_color_css_property(trimmed_name) {
            resolve_svg_paint_value(
                core_value,
                lookup_var,
                current_color,
                current_color_css,
                parser_context,
            )
        } else {
            resolve_value_vars_and_current_color(core_value, lookup_var, current_color_css)
        };

        if trimmed_name.eq_ignore_ascii_case("color") &&
            !contains_unresolved_var(&resolved_core)
        {
            flags.has_color = true;
        } else if trimmed_name.eq_ignore_ascii_case("fill") {
            flags.has_fill = true;
        } else if trimmed_name.eq_ignore_ascii_case("stroke") {
            flags.has_stroke = true;
        }

        if important {
            out_decls.push(format!("{trimmed_name}: {resolved_core} !important"));
        } else {
            out_decls.push(format!("{trimmed_name}: {resolved_core}"));
        }
    }

    (out_decls.join("; "), flags)
}

fn is_svg_color_css_property(name: &str) -> bool {
    name.eq_ignore_ascii_case("fill") ||
        name.eq_ignore_ascii_case("stroke") ||
        name.eq_ignore_ascii_case("color") ||
        name.eq_ignore_ascii_case("stop-color") ||
        name.eq_ignore_ascii_case("flood-color") ||
        name.eq_ignore_ascii_case("lighting-color")
}

fn resolve_svg_paint_value(
    raw_value: &str,
    lookup_var: &impl Fn(&str) -> Option<String>,
    current_color: Option<AbsoluteColor>,
    current_color_css: Option<&str>,
    parser_context: Option<&ParserContext<'_>>,
) -> String {
    let resolved = resolve_value_vars_and_current_color(raw_value, lookup_var, current_color_css);
    let trimmed = resolved.trim();
    let Some(context) = parser_context else {
        return resolved;
    };
    // Convert modern CSS Color 4/5 syntax (e.g. `oklch(...)`, `color-mix(...)`, `color(...)`)
    // into legacy `rgb(...)`/`rgba(...)` for `usvg`/`svgtypes` compatibility.
    if trimmed != raw_value.trim() || looks_like_modern_css_color(trimmed) {
        let mut parser = Parser::new(trimmed);
        if let Ok(computed) = SpecifiedColor::parse_and_compute(context, &mut parser, None) {
            if let Some(abs) = computed.as_absolute() {
                return format_srgb_legacy(*abs);
            }
            if let Some(cc) = current_color {
                return format_srgb_legacy(computed.resolve_to_absolute(&cc));
            }
        }
    }
    resolved
}

fn resolve_value_vars_and_current_color(
    raw_value: &str,
    lookup_var: &impl Fn(&str) -> Option<String>,
    current_color_css: Option<&str>,
) -> String {
    let with_vars = resolve_var_references(raw_value, 0, lookup_var);
    match current_color_css {
        Some(color_css) => replace_current_color(&with_vars, color_css),
        None => with_vars,
    }
}

fn looks_like_modern_css_color(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("oklch(") ||
        lower.starts_with("oklab(") ||
        lower.starts_with("lch(") ||
        lower.starts_with("lab(") ||
        lower.starts_with("color(") ||
        lower.starts_with("color-mix(") ||
        lower.starts_with("light-dark(") ||
        (lower.starts_with("rgb(") && lower.contains('/')) ||
        (lower.starts_with("hsl(") && lower.contains('/'))
}

#[derive(Default)]
struct CssScanState {
    paren_depth: usize,
    in_quote: Option<u8>,
    escaped: bool,
}

impl CssScanState {
    /// Advances state for `b` and returns `true` if `b` is outside string literals.
    fn step(&mut self, b: u8) -> bool {
        if let Some(q) = self.in_quote {
            if self.escaped {
                self.escaped = false;
            } else if b == b'\\' {
                self.escaped = true;
            } else if b == q {
                self.in_quote = None;
            }
            return false;
        }
        match b {
            b'\'' | b'"' => {
                self.in_quote = Some(b);
                false
            },
            b'(' => {
                self.paren_depth += 1;
                true
            },
            b')' => {
                self.paren_depth = self.paren_depth.saturating_sub(1);
                true
            },
            _ => true,
        }
    }
}

fn contains_unresolved_var(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut scan = CssScanState::default();
    for (i, &b) in bytes.iter().enumerate() {
        if !scan.step(b) {
            continue;
        }
        if (i == 0 || !is_css_ident_char(bytes[i - 1])) &&
            bytes[i..].len() >= 4 &&
            bytes[i..i + 4].eq_ignore_ascii_case(b"var(")
        {
            return true;
        }
    }
    false
}

fn is_css_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Expands CSS `var(--name[, fallback])` calls up to `MAX_VAR_DEPTH` levels.
fn resolve_var_references(
    input: &str,
    depth: usize,
    lookup_var: &impl Fn(&str) -> Option<String>,
) -> String {
    if depth >= MAX_VAR_DEPTH || !input.as_bytes().windows(4).any(|w| w.eq_ignore_ascii_case(b"var(")) {
        return input.to_owned();
    }

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut last_copied = 0;
    let mut i = 0;
    let mut scan = CssScanState::default();

    while i < bytes.len() {
        let prev_unquoted = scan.in_quote.is_none();
        if prev_unquoted &&
            (i == 0 || !is_css_ident_char(bytes[i - 1])) &&
            bytes.len() - i >= 4 &&
            bytes[i..i + 4].eq_ignore_ascii_case(b"var(")
        {
            if let Some(close_idx) = find_matching_paren(bytes, i + 4) {
                let inner = &input[i + 4..close_idx];
                let (var_name, fallback) = split_var_args(inner);
                if custom_properties::parse_name(var_name).is_ok() {
                    let replacement = if let Some(val) = lookup_var(var_name) {
                        Some(resolve_var_references(&val, depth + 1, lookup_var))
                    } else {
                        fallback.map(|fb| resolve_var_references(fb.trim(), depth + 1, lookup_var))
                    };
                    if let Some(replacement) = replacement {
                        out.push_str(&input[last_copied..i]);
                        out.push_str(&replacement);
                        i = close_idx + 1;
                        last_copied = i;
                        continue;
                    }
                }
            }
            // Advance past `v` normally via `scan.step` below if unresolved/unclosed.
        }
        scan.step(bytes[i]);
        i += 1;
    }

    if last_copied == 0 {
        return input.to_owned();
    }
    out.push_str(&input[last_copied..]);
    out
}

fn find_matching_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut scan = CssScanState {
        paren_depth: 1,
        ..Default::default()
    };
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if scan.step(b) && b == b')' && scan.paren_depth == 0 {
            return Some(start + offset);
        }
    }
    None
}

fn split_var_args(inner: &str) -> (&str, Option<&str>) {
    let mut scan = CssScanState::default();
    for (i, &b) in inner.as_bytes().iter().enumerate() {
        if scan.step(b) && scan.paren_depth == 0 && b == b',' {
            return (inner[..i].trim(), Some(&inner[i + 1..]));
        }
    }
    (inner.trim(), None)
}

/// Replaces standalone `currentColor` tokens (case-insensitive) outside quotes.
fn replace_current_color(input: &str, replacement: &str) -> String {
    const NEEDLE: &[u8] = b"currentcolor";
    let bytes = input.as_bytes();
    if bytes.len() < NEEDLE.len() {
        return input.to_owned();
    }

    let mut out = String::with_capacity(input.len());
    let mut last_copied = 0;
    let mut i = 0;
    let mut scan = CssScanState::default();

    while i + NEEDLE.len() <= bytes.len() {
        if !scan.step(bytes[i]) {
            i += 1;
            continue;
        }
        let before_ok = i == 0 || !is_css_ident_char(bytes[i - 1]);
        let after_ok = i + NEEDLE.len() == bytes.len() ||
            !is_css_ident_char(bytes[i + NEEDLE.len()]);
        if before_ok && after_ok && bytes[i..i + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            out.push_str(&input[last_copied..i]);
            out.push_str(replacement);
            i += NEEDLE.len();
            last_copied = i;
            continue;
        }

        i += 1;
    }

    if last_copied == 0 {
        return input.to_owned();
    }
    out.push_str(&input[last_copied..]);
    out
}

fn split_css_declarations(style: &str) -> Vec<&str> {
    let mut decls = Vec::new();
    let mut start = 0;
    let mut scan = CssScanState::default();

    for (i, &b) in style.as_bytes().iter().enumerate() {
        if scan.step(b) && scan.paren_depth == 0 && b == b';' {
            let piece = style[start..i].trim();
            if !piece.is_empty() {
                decls.push(piece);
            }
            start = i + 1;
        }
    }

    let tail = style[start..].trim();
    if !tail.is_empty() {
        decls.push(tail);
    }
    decls
}

fn split_css_declaration(decl: &str) -> Option<(&str, &str)> {
    let mut scan = CssScanState::default();
    for (i, &b) in decl.as_bytes().iter().enumerate() {
        if scan.step(b) && scan.paren_depth == 0 && b == b':' {
            return Some((&decl[..i], decl[i + 1..].trim()));
        }
    }
    None
}

fn split_important_suffix(value: &str) -> (&str, bool) {
    let trimmed = value.trim();
    let suffix_len = "!important".len();
    if trimmed.len() >= suffix_len &&
        trimmed[trimmed.len() - suffix_len..].eq_ignore_ascii_case("!important")
    {
        return (trimmed[..trimmed.len() - suffix_len].trim_end(), true);
    }
    (trimmed, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_nested_var_and_fallback() {
        let lookup = |name: &str| match name {
            "--fg" => Some(String::from("var(--inner, #123456)")),
            "--inner" => Some(String::from("rgb(10, 20, 30)")),
            _ => None,
        };
        assert_eq!(
            resolve_var_references("var(--fg)", 0, &lookup),
            "rgb(10, 20, 30)"
        );
        assert_eq!(
            resolve_var_references("var(--missing, var(--fg, #fff))", 0, &lookup),
            "rgb(10, 20, 30)"
        );
        assert_eq!(
            resolve_var_references("var(--missing, rgba(1, 2, 3, 0.5))", 0, &lookup),
            "rgba(1, 2, 3, 0.5)"
        );
    }

    #[test]
    fn caps_var_recursion_depth() {
        let lookup = |name: &str| match name {
            "--loop" => Some(String::from("var(--loop)")),
            _ => None,
        };
        let resolved = resolve_var_references("var(--loop)", 0, &lookup);
        assert_eq!(resolved, "var(--loop)");
    }

    #[test]
    fn replaces_current_color_with_boundaries_and_quotes() {
        assert_eq!(
            replace_current_color("CurrentColor", "rgb(255, 0, 0)"),
            "rgb(255, 0, 0)"
        );
        assert_eq!(
            replace_current_color("url(#grad) currentcolor", "rgb(0, 0, 255)"),
            "url(#grad) rgb(0, 0, 255)"
        );
        assert_eq!(
            replace_current_color("my-currentColor \"currentColor\"", "rgb(0, 0, 0)"),
            "my-currentColor \"currentColor\""
        );
    }

    #[test]
    fn resolves_inline_style_declarations() {
        let lookup = |name: &str| match name {
            "--icon-color" => Some(String::from("currentColor")),
            _ => None,
        };
        let (resolved, flags) = resolve_style_attribute_inner(
            "--icon-color: currentColor; fill: var(--icon-color); stroke: var(--missing, #fff) !important",
            &lookup,
            None,
            Some("rgb(12, 34, 56)"),
            None,
        );
        assert!(flags.has_fill);
        assert!(flags.has_stroke);
        assert!(!flags.has_color);
        assert_eq!(
            resolved,
            "--icon-color: currentColor; fill: rgb(12, 34, 56); stroke: #fff !important"
        );
    }
}
