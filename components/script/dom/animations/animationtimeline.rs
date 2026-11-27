/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::Reflector;
use script_bindings::root::{Dom, DomRoot};
use time::Duration;

use crate::dom::animation::Animation;
use crate::dom::animations::keyframeeffect::KeyframeEffect;
use crate::dom::bindings::codegen::Bindings::AnimationBinding::AnimationPlayState;
use crate::dom::bindings::codegen::Bindings::AnimationTimelineBinding::AnimationTimelineMethods;
use crate::dom::element::Element;
use crate::dom::node::Node;
use crate::dom::shadowroot::ShadowRoot;

/// <https://drafts.csswg.org/web-animations-1/#animationtimeline>
#[dom_struct]
pub(crate) struct AnimationTimeline {
    reflector_: Reflector,
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationtimeline-currenttime>
    ///
    /// The current time of this [`AnimationTimeline`] expressed as a [`time::Duration`] since the
    /// Document's "time origin." Note that this Duration may be negative.
    #[no_trace]
    current_time: Cell<Duration>,
    animations: DomRefCell<Vec<Dom<Animation>>>,
}

impl AnimationTimeline {
    pub(crate) fn new_inherited(current_time: Duration) -> Self {
        Self {
            reflector_: Reflector::new(),
            current_time: Cell::new(current_time),
            animations: Default::default(),
        }
    }

    pub(crate) fn current_time_in_seconds(&self) -> f64 {
        self.current_time.get().as_seconds_f64()
    }

    pub(crate) fn set_current_time(&self, duration: Duration) {
        self.current_time.set(duration);
    }

    #[allow(dead_code)]
    pub(crate) fn advance_specific(&self, by: Duration) {
        self.current_time.set(self.current_time.get() + by);
    }

    pub(crate) fn add_animation(&self, animation: &Animation) {
        let mut animations = self.animations.borrow_mut();
        if !animations.iter().any(|existing| &**existing == animation) {
            animations.push(Dom::from_ref(animation));
        }
    }

    pub(crate) fn remove_animation(&self, animation: &Animation) {
        self.animations
            .borrow_mut()
            .retain(|existing| &**existing != animation);
    }

    pub(crate) fn active_animations(&self) -> Vec<DomRoot<Animation>> {
        self.animations
            .borrow()
            .iter()
            .filter(|a| a.play_state() != AnimationPlayState::Idle)
            .map(|a| DomRoot::from_ref(&**a))
            .collect()
    }

    pub(crate) fn animations_for_element(
        &self,
        element: &Element,
        subtree: bool,
    ) -> Vec<DomRoot<Animation>> {
        let elem_node = element.upcast::<Node>();
        self.animations
            .borrow()
            .iter()
            .filter(|a| a.play_state() != AnimationPlayState::Idle)
            .filter(|a| {
                let Some(effect) = a.effect_ref() else {
                    return false;
                };
                let Some(kf) = effect.downcast::<KeyframeEffect>() else {
                    return false;
                };
                let Some(target) = kf.target_element() else {
                    return false;
                };
                if subtree {
                    elem_node.is_shadow_including_inclusive_ancestor_of(target.upcast::<Node>())
                } else {
                    &*target == element
                }
            })
            .map(|a| DomRoot::from_ref(&**a))
            .collect()
    }

    pub(crate) fn animations_for_shadow_root(
        &self,
        shadow_root: &ShadowRoot,
    ) -> Vec<DomRoot<Animation>> {
        self.animations
            .borrow()
            .iter()
            .filter(|a| a.play_state() != AnimationPlayState::Idle)
            .filter(|a| {
                let Some(effect) = a.effect_ref() else {
                    return false;
                };
                let Some(kf) = effect.downcast::<KeyframeEffect>() else {
                    return false;
                };
                let Some(target) = kf.target_element() else {
                    return false;
                };
                target
                    .upcast::<Node>()
                    .containing_shadow_root()
                    .is_some_and(|sr| &*sr == shadow_root)
            })
            .map(|a| DomRoot::from_ref(&**a))
            .collect()
    }
}

impl AnimationTimelineMethods<crate::DomTypeHolder> for AnimationTimeline {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationtimeline-currenttime>
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        Finite::new(self.current_time.get().as_seconds_f64() * 1000.0)
    }
}
