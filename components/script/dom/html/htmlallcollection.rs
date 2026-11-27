/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{local_name, ns};
use js::context::{JSContext, NoGC};
use rustc_hash::FxHashSet;
use script_bindings::domstring::TracedDOMString;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::HTMLAllCollectionBinding::HTMLAllCollectionMethods;
use crate::dom::bindings::codegen::Bindings::HTMLCollectionBinding::HTMLCollectionMethods;
use crate::dom::bindings::codegen::UnionTypes::HTMLCollectionOrElement;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::html::htmlcollection::{CollectionFilter, HTMLCollection};
use crate::dom::node::Node;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#all-exposed>
fn is_all_exposed(elem: &Element) -> bool {
    *elem.namespace() == ns!(html) &&
        matches!(
            elem.local_name(),
            &local_name!("a") |
                &local_name!("button") |
                &local_name!("embed") |
                &local_name!("form") |
                &local_name!("frame") |
                &local_name!("frameset") |
                &local_name!("iframe") |
                &local_name!("img") |
                &local_name!("input") |
                &local_name!("map") |
                &local_name!("meta") |
                &local_name!("object") |
                &local_name!("select") |
                &local_name!("textarea")
        )
}

fn parse_array_index(s: &str) -> Option<u32> {
    if s == "0" {
        return Some(0);
    }
    if s.is_empty() || s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let idx = s.parse::<u32>().ok()?;
    if idx < u32::MAX { Some(idx) } else { None }
}

#[derive(JSTraceable, MallocSizeOf)]
struct AllElementsFilter;

impl CollectionFilter for AllElementsFilter {
    fn filter(&self, _elem: &Element, _root: &Node) -> bool {
        true
    }
}

#[derive(JSTraceable, MallocSizeOf)]
struct AllNamedFilter {
    #[no_trace]
    name: Atom,
}

impl CollectionFilter for AllNamedFilter {
    fn filter(&self, elem: &Element, _root: &Node) -> bool {
        elem.get_id().is_some_and(|id| id == self.name) ||
            (is_all_exposed(elem) && elem.get_name().is_some_and(|n| n == self.name))
    }
}

#[dom_struct]
pub(crate) struct HTMLAllCollection {
    reflector_: Reflector,
    collection: Dom<HTMLCollection>,
}

impl HTMLAllCollection {
    fn new_inherited(collection: &HTMLCollection) -> HTMLAllCollection {
        HTMLAllCollection {
            reflector_: Reflector::new(),
            collection: Dom::from_ref(collection),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        root: &Node,
    ) -> DomRoot<HTMLAllCollection> {
        let collection = HTMLCollection::create(cx, window, root, Box::new(AllElementsFilter));
        reflect_dom_object(cx, Box::new(Self::new_inherited(&collection)), window)
    }

    /// <https://html.spec.whatwg.org/multipage/#concept-htmlallcollection-named>
    fn get_item(&self, cx: &mut JSContext, name_or_index: Option<DOMString>) -> Option<HTMLCollectionOrElement> {
        let name_or_index = name_or_index?;
        if let Some(index) = parse_array_index(&name_or_index.str()) {
            return self
                .collection
                .IndexedGetter(cx, index)
                .map(HTMLCollectionOrElement::Element);
        }
        self.NamedItem(cx, name_or_index)
    }
}

impl HTMLAllCollectionMethods<crate::DomTypeHolder> for HTMLAllCollection {
    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-length>
    fn Length(&self, cx: &JSContext) -> u32 {
        self.collection.Length(cx)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn IndexedGetter(&self, cx: &JSContext, index: u32) -> Option<DomRoot<Element>> {
        self.collection.IndexedGetter(cx, index)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedItem(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        if name.is_empty() {
            return None;
        }

        let name = Atom::from(name);
        {
            let mut matches = self
                .collection
                .elements_iter(cx.no_gc())
                .filter(|elem| {
                    elem.get_id().is_some_and(|id| id == name) ||
                        (is_all_exposed(elem) && elem.get_name().is_some_and(|n| n == name))
                })
                .peekable();

            let first = matches.next()?;
            if matches.peek().is_none() {
                return Some(HTMLCollectionOrElement::Element(first.as_rooted()));
            }
        }

        let global = self.global();
        let window = global.as_window();
        let root = self.collection.root_node();
        Some(HTMLCollectionOrElement::HTMLCollection(
            HTMLCollection::create(cx, window, &root, Box::new(AllNamedFilter { name })),
        ))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedGetter(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.NamedItem(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn Item(&self, cx: &mut JSContext, name_or_index: Option<DOMString>) -> Option<HTMLCollectionOrElement> {
        self.get_item(cx, name_or_index)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-call>
    fn LegacyCaller(&self, cx: &mut JSContext, name_or_index: Option<DOMString>) -> Option<HTMLCollectionOrElement> {
        self.get_item(cx, name_or_index)
    }

    /// <https://html.spec.whatwg.org/multipage/#the-htmlallcollection-interface:supported-property-names>
    fn SupportedPropertyNames(&self, no_gc: &NoGC) -> Vec<DOMString> {
        let mut seen = FxHashSet::default();
        let mut result = vec![];

        for elem in self.collection.elements_iter(no_gc) {
            if let Some(id_atom) = elem.get_id() {
                if !id_atom.is_empty() &&
                    parse_array_index(&id_atom).is_none() &&
                    seen.insert(id_atom.clone())
                {
                    result.push(TracedDOMString::from(id_atom).root());
                }
            }
            if is_all_exposed(&elem) {
                if let Some(name_atom) = elem.get_name() {
                    if !name_atom.is_empty() &&
                        parse_array_index(&name_atom).is_none() &&
                        seen.insert(name_atom.clone())
                    {
                        result.push(TracedDOMString::from(name_atom).root());
                    }
                }
            }
        }

        result
    }
}
