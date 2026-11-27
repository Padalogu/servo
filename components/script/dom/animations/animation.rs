/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use stylo_atoms::{Atom, atom};

use crate::dom::animationeffect::AnimationEffect;
use crate::dom::bindings::codegen::Bindings::AnimationBinding::{
    AnimationMethods, AnimationPlayState, AnimationReplaceState,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::types::AnimationTimeline;
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;

/// <https://drafts.csswg.org/web-animations-1/#animation>
#[dom_struct]
pub(crate) struct Animation {
    event_target: EventTarget,

    id: DomRefCell<String>,

    /// <https://drafts.csswg.org/web-animations-1/#timeline>
    timeline: MutNullableDom<AnimationTimeline>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>
    associated_effect: MutNullableDom<AnimationEffect>,

    start_time: Cell<Option<f64>>,
    current_time: Cell<Option<f64>>,
    playback_rate: Cell<f64>,
    play_state: Cell<AnimationPlayState>,
    replace_state: Cell<AnimationReplaceState>,
    ready_promise: DomRefCell<TracedPromise>,
    finished_promise: DomRefCell<TracedPromise>,
    finished_observed: Cell<bool>,
    generation: Cell<u64>,
}

impl Animation {
    pub(crate) fn new_inherited(
        ready_promise: &RootedPromise,
        finished_promise: &RootedPromise,
    ) -> Self {
        Self {
            event_target: EventTarget::new_inherited(),
            id: Default::default(),
            timeline: Default::default(),
            associated_effect: Default::default(),
            start_time: Cell::new(None),
            current_time: Cell::new(None),
            playback_rate: Cell::new(1.0),
            play_state: Cell::new(AnimationPlayState::Idle),
            replace_state: Cell::new(AnimationReplaceState::Active),
            ready_promise: DomRefCell::new(ready_promise.to_traced()),
            finished_promise: DomRefCell::new(finished_promise.to_traced()),
            finished_observed: Cell::new(false),
            generation: Cell::new(0),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        let ready_promise = Promise::new(cx, global);
        let finished_promise = Promise::new(cx, global);
        let animation = reflect_dom_object_with_proto(
            cx,
            Box::new(Self::new_inherited(&ready_promise, &finished_promise)),
            global,
            proto,
        );
        ready_promise.resolve_native(cx, &*animation);
        animation
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        Self::new_with_proto(cx, global, None)
    }

    pub(crate) fn play_state(&self) -> AnimationPlayState {
        self.play_state.get()
    }

    pub(crate) fn effect_ref(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-timeline-of-an-animation>
    fn set_the_timeline(&self, timeline: Option<&AnimationTimeline>) {
        if let Some(old_tl) = self.timeline.get()
            && timeline.is_none_or(|new_tl| &*old_tl != new_tl)
        {
            old_tl.remove_animation(self);
        }
        self.timeline.set(timeline);
        if self.play_state.get() != AnimationPlayState::Idle
            && let Some(new_tl) = timeline
        {
            new_tl.add_animation(self);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-associated-effect-of-an-animation>
    fn set_the_associated_effect(&self, effect: Option<&AnimationEffect>) {
        // FIXME: Implement this fully
        self.associated_effect.set(effect);
    }

    fn complete_animation(&self, cx: &mut JSContext) {
        if self.play_state.get() == AnimationPlayState::Finished {
            return;
        }
        self.play_state.set(AnimationPlayState::Finished);
        if let Some(effect) = self.associated_effect.get() {
            let end_ms = effect.total_end_time_ms();
            if end_ms.is_finite() {
                self.current_time.set(Some(end_ms));
            }
        } else {
            self.current_time.set(Some(0.0));
        }
        {
            let finished = self.finished_promise.borrow();
            if !finished.is_fulfilled() {
                finished.resolve_native(cx, self);
            }
        }
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("finish"));
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        _object: Option<HandleObject>,
        effect: Option<&AnimationEffect>,
        timeline: Option<Option<&AnimationTimeline>>,
    ) -> DomRoot<Self> {
        // Step 1. Let animation be a new Animation object.
        let animation = Animation::new(cx, window.upcast());

        // Step 2. Run the procedure to set the timeline of an animation on animation passing timeline
        // as the new timeline; or, if the timeline argument is missing, passing the default document
        // timeline of the Document associated with the Window that is the current global object.
        if let Some(timeline) = timeline.flatten() {
            animation.set_the_timeline(Some(timeline));
        } else {
            let document = window.document_unrooted(cx.no_gc());
            let timeline = document.Timeline();
            animation.set_the_timeline(Some(timeline.upcast()));
        }

        // Step 3. Run the procedure to set the associated effect of an animation on animation passing
        // source as the new effect.
        animation.set_the_associated_effect(effect);

        animation
    }

    fn Id(&self) -> DOMString {
        DOMString::from(self.id.borrow().as_str())
    }

    fn SetId(&self, id: DOMString) {
        *self.id.borrow_mut() = id.to_string();
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn GetEffect(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn SetEffect(&self, effect: Option<&AnimationEffect>) {
        // > Setting this attribute updates the object’s associated effect using
        // > the procedure to set the associated effect of an animation.
        self.set_the_associated_effect(effect);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation-effect-timeline-timeline>
    fn GetTimeline(&self) -> Option<DomRoot<AnimationTimeline>> {
        self.timeline.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation-effect-timeline-timeline>
    fn SetTimeline(&self, timeline: Option<&AnimationTimeline>) {
        // > Setting this attribute updates the object’s timeline using the procedure
        // > to set the timeline of an animation.
        self.set_the_timeline(timeline);
    }

    fn GetStartTime(&self) -> Option<Finite<f64>> {
        self.start_time.get().and_then(Finite::new)
    }

    fn SetStartTime(&self, value: Option<Finite<f64>>) {
        self.start_time.set(value.map(|v| *v));
    }

    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.current_time.get().and_then(Finite::new)
    }

    fn SetCurrentTime(&self, value: Option<Finite<f64>>) {
        self.current_time.set(value.map(|v| *v));
    }

    fn PlaybackRate(&self) -> Finite<f64> {
        Finite::new(self.playback_rate.get()).unwrap_or_else(|| Finite::wrap(1.0))
    }

    fn SetPlaybackRate(&self, value: Finite<f64>) {
        self.playback_rate.set(*value);
    }

    fn PlayState(&self) -> AnimationPlayState {
        self.play_state.get()
    }

    fn ReplaceState(&self) -> AnimationReplaceState {
        self.replace_state.get()
    }

    fn Pending(&self) -> bool {
        false
    }

    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        self.ready_promise.borrow().root(cx)
    }

    fn Finished(&self, cx: &mut JSContext) -> RootedPromise {
        self.finished_observed.set(true);
        self.finished_promise.borrow().root(cx)
    }

    event_handler!(finish, GetOnfinish, SetOnfinish);
    event_handler!(cancel, GetOncancel, SetOncancel);
    event_handler!(remove, GetOnremove, SetOnremove);

    fn Cancel(&self, cx: &mut JSContext) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.play_state.set(AnimationPlayState::Idle);
        self.start_time.set(None);
        self.current_time.set(None);
        if let Some(tl) = self.timeline.get() {
            tl.remove_animation(self);
        }
        {
            let finished = self.finished_promise.borrow();
            if !finished.is_fulfilled() {
                finished.reject_error(cx, Error::Abort(None));
                if !self.finished_observed.get() {
                    finished.set_promise_is_handled(cx);
                }
            }
        }
        let next_finished = Promise::new(cx, &self.global());
        *self.finished_promise.borrow_mut() = next_finished.to_traced();
        self.finished_observed.set(false);
        self.upcast::<EventTarget>()
            .fire_event(cx, atom!("cancel"));
    }

    fn Finish(&self, cx: &mut JSContext) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.complete_animation(cx);
    }

    fn Play(&self, cx: &mut JSContext) {
        let next_gen = self.generation.get().wrapping_add(1);
        self.generation.set(next_gen);

        if let Some(tl) = self.timeline.get() {
            tl.add_animation(self);
            if self.start_time.get().is_none() {
                self.start_time.set(Some(tl.current_time_in_seconds() * 1000.0));
            }
        } else if self.start_time.get().is_none() {
            self.start_time.set(Some(0.0));
        }
        if self.current_time.get().is_none() {
            self.current_time.set(Some(0.0));
        }

        if self.finished_promise.borrow().is_fulfilled() {
            let next_finished = Promise::new(cx, &self.global());
            *self.finished_promise.borrow_mut() = next_finished.to_traced();
            self.finished_observed.set(false);
        }

        self.play_state.set(AnimationPlayState::Running);

        let end_ms = self
            .associated_effect
            .get()
            .map_or(0.0, |e| e.total_end_time_ms());
        let rate = self.playback_rate.get();
        if rate == 0.0 || end_ms.is_infinite() {
            return;
        }
        if end_ms <= 0.0 {
            self.complete_animation(cx);
            return;
        }

        let delay_raw = (end_ms / rate.abs()).ceil();
        if !delay_raw.is_finite() {
            return;
        }
        let delay_ms = (delay_raw as i64).clamp(1, 60_000);
        let anim_keepalive = Trusted::new(self);
        self.global().run_steps_after_a_timeout(
            DOMString::from_static("web-animation-finish"),
            delay_ms,
            move |_cx, global| {
                let task_source = global.task_manager().timer_task_source().to_sendable();
                task_source.queue(task!(web_animation_finish: move |cx| {
                    let anim = anim_keepalive.root();
                    if anim.generation.get() == next_gen
                        && anim.play_state.get() == AnimationPlayState::Running
                    {
                        let mut realm = enter_auto_realm(cx, &*anim.global());
                        let mut realm = realm.current_realm();
                        anim.complete_animation(&mut realm);
                    }
                }));
            },
        );
    }

    fn Pause(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.play_state.set(AnimationPlayState::Paused);
    }

    fn UpdatePlaybackRate(&self, playback_rate: Finite<f64>) {
        self.playback_rate.set(*playback_rate);
    }

    fn Reverse(&self, cx: &mut JSContext) {
        self.playback_rate.set(-self.playback_rate.get());
        self.Play(cx);
    }

    fn Persist(&self) {
        self.replace_state.set(AnimationReplaceState::Persisted);
    }

    fn CommitStyles(&self) {}
}
