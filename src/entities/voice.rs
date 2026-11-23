// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Voice assistant entity: Siri on the Apple TV.
//!
//! Siri shows its answer on the TV. No text or speech is sent back to the Remote.

use crate::controller::VOICE_SAMPLE_RATE;
use crate::device::state::Snapshot;
use serde_json::{Map, Value};
use uc_api::intg::AvailableIntgEntity;
use uc_api::{AudioConfiguration, SampleFormat, VoiceAssistantEntityOptions};

pub fn definition(mut entity: AvailableIntgEntity) -> AvailableIntgEntity {
    entity.features = Some(Vec::new());
    let options = VoiceAssistantEntityOptions {
        audio_cfg: Some(AudioConfiguration {
            channels: 1,
            sample_rate: VOICE_SAMPLE_RATE,
            sample_format: SampleFormat::I16,
        }),
        profiles: None,
        preferred_profile: None,
    };
    if let Ok(Value::Object(map)) = serde_json::to_value(options) {
        entity.options = Some(map);
    }
    entity
}

/// Insert the state attribute: ON with Siri, OFF when connected without Siri.
pub fn attributes(s: &Snapshot, out: &mut Map<String, Value>) {
    let state = match (s.connected, s.siri) {
        (true, true) => "ON",
        (true, false) => "OFF",
        (false, _) => "UNAVAILABLE",
    };
    out.insert("state".into(), state.into());
}

#[cfg(test)]
mod tests {
    use super::attributes;
    use crate::device::state::Snapshot;
    use serde_json::{Map, json};

    #[test]
    fn off_without_siri_unavailable_without_connection() {
        let mut s = Snapshot {
            connected: true,
            siri: true,
            ..Snapshot::default()
        };
        let mut out = Map::new();
        attributes(&s, &mut out);
        assert_eq!(Some(&json!("ON")), out.get("state"));

        s.siri = false;
        attributes(&s, &mut out);
        assert_eq!(Some(&json!("OFF")), out.get("state"));

        s.connected = false;
        attributes(&s, &mut out);
        assert_eq!(Some(&json!("UNAVAILABLE")), out.get("state"));
    }
}
