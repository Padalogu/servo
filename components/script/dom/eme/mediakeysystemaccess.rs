/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use mime::Mime;
use script_bindings::domstring::TracedDOMString;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::str::DOMString;

use crate::dom::bindings::codegen::Bindings::MediaKeySystemAccessBinding::{
    MediaKeySystemAccessMethods, MediaKeySystemConfiguration, MediaKeySystemMediaCapability,
    MediaKeysRequirement,
};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::eme::mediakeys::MediaKeys;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediasource::MediaSource;
use crate::dom::promise::{Promise, RootedPromise};

pub(crate) const CLEARKEY_KEY_SYSTEM: &str = "org.w3.clearkey";

#[derive(Clone, Debug, MallocSizeOf)]
pub(crate) struct ResolvedMediaCapability {
    pub(crate) content_type: String,
    pub(crate) encryption_scheme: Option<String>,
    pub(crate) robustness: String,
}

#[derive(Clone, Debug, MallocSizeOf)]
pub(crate) struct ResolvedMediaKeySystemConfig {
    pub(crate) label: String,
    pub(crate) init_data_types: Vec<String>,
    pub(crate) audio_capabilities: Vec<ResolvedMediaCapability>,
    pub(crate) video_capabilities: Vec<ResolvedMediaCapability>,
    pub(crate) distinctive_identifier: MediaKeysRequirement,
    pub(crate) persistent_state: MediaKeysRequirement,
    pub(crate) session_types: Vec<String>,
}

fn is_supported_init_data_type(init_data_type: &str) -> bool {
    matches!(init_data_type, "cenc" | "keyids" | "webm")
}

fn is_supported_encryption_scheme(scheme: Option<&str>) -> bool {
    match scheme {
        None => true,
        Some(s) => matches!(s, "cenc" | "cbcs" | "cbcs-1-9"),
    }
}

fn is_supported_capability(cap: &MediaKeySystemMediaCapability, expect_audio: bool) -> bool {
    let content_type = cap.contentType.str();
    let trimmed = content_type.trim();
    if trimmed.is_empty() {
        return false;
    }
    if !cap.robustness.is_empty() {
        return false;
    }
    let scheme_str = cap.encryptionScheme.as_ref().map(|s| s.to_string());
    if !is_supported_encryption_scheme(scheme_str.as_deref()) {
        return false;
    }

    let Ok(mime) = trimmed.parse::<Mime>() else {
        return false;
    };
    let expected_type = if expect_audio { "audio" } else { "video" };
    if !mime.type_().as_str().eq_ignore_ascii_case(expected_type) {
        return false;
    }

    if mime.get_param("codecs").is_some() {
        return MediaSource::is_type_supported_str(trimmed);
    }

    let container = format!(
        "{}/{}",
        mime.type_().as_str().to_ascii_lowercase(),
        mime.subtype().as_str().to_ascii_lowercase()
    );
    if expect_audio {
        matches!(
            container.as_str(),
            "audio/mp4" | "audio/webm" | "audio/mpeg" | "audio/aac"
        )
    } else {
        matches!(container.as_str(), "video/mp4" | "video/webm")
    }
}

pub(crate) fn evaluate_clearkey_configuration(
    candidate: &MediaKeySystemConfiguration,
) -> Option<ResolvedMediaKeySystemConfig> {
    if candidate.distinctiveIdentifier == MediaKeysRequirement::Required ||
        candidate.persistentState == MediaKeysRequirement::Required
    {
        return None;
    }

    let session_types = match &candidate.sessionTypes {
        None => vec!["temporary".to_string()],
        Some(requested) => {
            if requested.is_empty() ||
                requested.iter().any(|t| &*t.str() != "temporary")
            {
                return None;
            }
            vec!["temporary".to_string()]
        },
    };

    let init_data_types: Vec<String> = if candidate.initDataTypes.is_empty() {
        Vec::new()
    } else {
        let supported: Vec<String> = candidate
            .initDataTypes
            .iter()
            .filter_map(|t| {
                let s = t.str();
                if is_supported_init_data_type(&s) {
                    Some(s.to_string())
                } else {
                    None
                }
            })
            .collect();
        if supported.is_empty() {
            return None;
        }
        supported
    };

    let mut audio_capabilities = Vec::new();
    if !candidate.audioCapabilities.is_empty() {
        for cap in &candidate.audioCapabilities {
            if is_supported_capability(cap, true) {
                audio_capabilities.push(ResolvedMediaCapability {
                    content_type: cap.contentType.to_string(),
                    encryption_scheme: cap.encryptionScheme.as_ref().map(|s| s.to_string()),
                    robustness: cap.robustness.to_string(),
                });
            }
        }
        if audio_capabilities.is_empty() {
            return None;
        }
    }

    let mut video_capabilities = Vec::new();
    if !candidate.videoCapabilities.is_empty() {
        for cap in &candidate.videoCapabilities {
            if is_supported_capability(cap, false) {
                video_capabilities.push(ResolvedMediaCapability {
                    content_type: cap.contentType.to_string(),
                    encryption_scheme: cap.encryptionScheme.as_ref().map(|s| s.to_string()),
                    robustness: cap.robustness.to_string(),
                });
            }
        }
        if video_capabilities.is_empty() {
            return None;
        }
    }

    if audio_capabilities.is_empty() && video_capabilities.is_empty() {
        return None;
    }

    Some(ResolvedMediaKeySystemConfig {
        label: candidate.label.to_string(),
        init_data_types,
        audio_capabilities,
        video_capabilities,
        distinctive_identifier: MediaKeysRequirement::Not_allowed,
        persistent_state: MediaKeysRequirement::Not_allowed,
        session_types,
    })
}

/// <https://w3c.github.io/encrypted-media/#mediakeysystemaccess-interface>
#[dom_struct]
pub(crate) struct MediaKeySystemAccess {
    reflector_: Reflector,
    key_system: TracedDOMString,
    #[no_trace]
    configuration: ResolvedMediaKeySystemConfig,
}

impl MediaKeySystemAccess {
    pub(crate) fn new_inherited(
        key_system: DOMString,
        configuration: ResolvedMediaKeySystemConfig,
    ) -> Self {
        Self {
            reflector_: Reflector::new(),
            key_system: key_system.traced(),
            configuration,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        key_system: &str,
        configuration: ResolvedMediaKeySystemConfig,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(
                DOMString::from(key_system),
                configuration,
            )),
            global,
        )
    }
}

impl MediaKeySystemAccessMethods<crate::DomTypeHolder> for MediaKeySystemAccess {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysystemaccess-keysystem>
    fn KeySystem(&self) -> DOMString {
        self.key_system.clone().root()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysystemaccess-getconfiguration>
    fn GetConfiguration(&self) -> MediaKeySystemConfiguration {
        MediaKeySystemConfiguration {
            label: DOMString::from(self.configuration.label.clone()),
            initDataTypes: self
                .configuration
                .init_data_types
                .iter()
                .map(|s| DOMString::from(s.clone()))
                .collect(),
            audioCapabilities: self
                .configuration
                .audio_capabilities
                .iter()
                .map(|c| MediaKeySystemMediaCapability {
                    contentType: DOMString::from(c.content_type.clone()),
                    encryptionScheme: c
                        .encryption_scheme
                        .as_ref()
                        .map(|s| DOMString::from(s.clone())),
                    robustness: DOMString::from(c.robustness.clone()),
                })
                .collect(),
            videoCapabilities: self
                .configuration
                .video_capabilities
                .iter()
                .map(|c| MediaKeySystemMediaCapability {
                    contentType: DOMString::from(c.content_type.clone()),
                    encryptionScheme: c
                        .encryption_scheme
                        .as_ref()
                        .map(|s| DOMString::from(s.clone())),
                    robustness: DOMString::from(c.robustness.clone()),
                })
                .collect(),
            distinctiveIdentifier: self.configuration.distinctive_identifier,
            persistentState: self.configuration.persistent_state,
            sessionTypes: Some(
                self.configuration
                    .session_types
                    .iter()
                    .map(|s| DOMString::from(s.clone()))
                    .collect(),
            ),
        }
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysystemaccess-createmediakeys>
    fn CreateMediaKeys(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let key_system = self.key_system.str();
        let media_keys = MediaKeys::new(cx, &self.global(), &key_system);
        promise.resolve_native(cx, &media_keys);
        promise
    }
}
