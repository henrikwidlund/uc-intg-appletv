// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Entities of one Apple TV: ids, definitions, attributes and command mapping.
//!
//! Entity ids have the form `<type>.<key>[.<suffix>]`. The key is 12 characters, so the longest
//! id is 34 characters, below the limit of 36.

pub mod media_player;
pub mod remote;
pub mod select;
pub mod sensor;
pub mod voice;

use crate::config_store::DeviceConfig;
use crate::device::state::{Dirty, Snapshot};
use serde_json::{Map, Value};
use std::collections::HashMap;
use uc_api::EntityType;
use uc_api::intg::AvailableIntgEntity;

/// Maximum length of an entity id.
#[cfg(test)]
pub const MAX_ENTITY_ID_LEN: usize = 36;

/// Select entities of an Apple TV.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum SelectKind {
    App,
    AudioOutput,
    AudioLanguage,
    Subtitles,
    Account,
    Chapter,
    UpNext,
    PlaybackSpeed,
    Shuffle,
    Repeat,
}

/// Sensor entities of an Apple TV.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum SensorKind {
    App,
    AudioOutput,
    AudioLanguage,
    Subtitles,
    PlaybackSpeed,
    Shuffle,
    Repeat,
    Episode,
    Genre,
    ContentRating,
    AudioFormat,
    IsLive,
    VolumeControl,
}

/// Entity of an Apple TV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    MediaPlayer,
    Remote,
    VoiceAssistant,
    Select(SelectKind),
    Sensor(SensorKind),
}

impl EntityKind {
    /// Every entity of one Apple TV.
    pub fn all() -> impl Iterator<Item = Self> {
        use strum::VariantArray;
        [Self::MediaPlayer, Self::Remote, Self::VoiceAssistant]
            .into_iter()
            .chain(SelectKind::VARIANTS.iter().copied().map(Self::Select))
            .chain(SensorKind::VARIANTS.iter().copied().map(Self::Sensor))
    }

    pub const fn entity_type(self) -> EntityType {
        match self {
            Self::MediaPlayer => EntityType::MediaPlayer,
            Self::Remote => EntityType::Remote,
            Self::VoiceAssistant => EntityType::VoiceAssistant,
            Self::Select(_) => EntityType::Select,
            Self::Sensor(_) => EntityType::Sensor,
        }
    }

    /// Build the entity id for a device key.
    pub fn entity_id(self, key: &str) -> String {
        let mut id = String::new();
        self.write_entity_id(key, &mut id);
        id
    }

    /// Append the entity id for a device key to `out`.
    pub fn write_entity_id(self, key: &str, out: &mut String) {
        use std::fmt::Write;
        let ty = self.entity_type();
        let _ = match self {
            Self::MediaPlayer | Self::Remote | Self::VoiceAssistant => write!(out, "{ty}.{key}"),
            Self::Select(s) => write!(out, "{ty}.{key}.{}", s.as_ref()),
            Self::Sensor(s) => write!(out, "{ty}.{key}.{}", s.as_ref()),
        };
    }

    /// Parse an entity id into the device key and the entity kind.
    pub fn parse(entity_id: &str) -> Option<(&str, Self)> {
        let mut parts = entity_id.splitn(3, '.');
        let ty = parts.next()?;
        let key = parts.next()?;
        let suffix = parts.next();
        let kind = match (ty, suffix) {
            ("media_player", None) => Self::MediaPlayer,
            ("remote", None) => Self::Remote,
            ("voice_assistant", None) => Self::VoiceAssistant,
            ("select", Some(s)) => Self::Select(s.parse().ok()?),
            ("sensor", Some(s)) => Self::Sensor(s.parse().ok()?),
            _ => return None,
        };
        Some((key, kind))
    }
}

/// Build all entity definitions of an Apple TV.
pub fn definitions(cfg: &DeviceConfig) -> Vec<AvailableIntgEntity> {
    EntityKind::all()
        .map(|kind| definition(cfg, kind))
        .collect()
}

/// Build the definition of one entity.
pub fn definition(cfg: &DeviceConfig, kind: EntityKind) -> AvailableIntgEntity {
    let entity = AvailableIntgEntity {
        entity_id: kind.entity_id(&cfg.key),
        device_id: None,
        entity_type: kind.entity_type(),
        device_class: None,
        name: name(&cfg.name, entity_name_suffix(kind)),
        icon: None,
        description: None,
        features: None,
        area: None,
        options: None,
        attributes: None,
    };
    match kind {
        EntityKind::MediaPlayer => media_player::definition(entity),
        EntityKind::Remote => remote::definition(entity),
        EntityKind::VoiceAssistant => voice::definition(entity),
        EntityKind::Select(_) => entity,
        EntityKind::Sensor(s) => sensor::definition(entity, s),
    }
}

/// Insert the attributes of an entity that depend on the dirty groups.
pub fn attributes(
    s: &Snapshot,
    kind: EntityKind,
    dirty: Dirty,
    now: f64,
    out: &mut Map<String, Value>,
) {
    match kind {
        EntityKind::MediaPlayer => media_player::attributes(s, dirty, now, out),
        EntityKind::Remote => {
            if dirty.any(Dirty::CONNECTION | Dirty::POWER) {
                out.insert("state".into(), remote::state(s).into());
            }
        }
        EntityKind::VoiceAssistant => {
            if dirty.any(Dirty::CONNECTION) {
                voice::attributes(s, out);
            }
        }
        EntityKind::Select(k) => select::attributes(s, k, dirty, now, out),
        EntityKind::Sensor(k) => sensor::attributes(s, k, dirty, now, out),
    }
}

const fn entity_name_suffix(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::MediaPlayer => "",
        EntityKind::Remote => "Remote",
        EntityKind::VoiceAssistant => "Siri",
        EntityKind::Select(s) => match s {
            SelectKind::App => "App",
            SelectKind::AudioOutput => "Audio output",
            SelectKind::AudioLanguage => "Audio language",
            SelectKind::Subtitles => "Subtitles",
            SelectKind::Account => "Account",
            SelectKind::Chapter => "Chapter",
            SelectKind::UpNext => "Up Next",
            SelectKind::PlaybackSpeed => "Playback speed",
            SelectKind::Shuffle => "Shuffle",
            SelectKind::Repeat => "Repeat",
        },
        EntityKind::Sensor(s) => match s {
            SensorKind::App => "App",
            SensorKind::AudioOutput => "Audio output",
            SensorKind::AudioLanguage => "Audio language",
            SensorKind::Subtitles => "Subtitles",
            SensorKind::PlaybackSpeed => "Playback speed",
            SensorKind::Shuffle => "Shuffle",
            SensorKind::Repeat => "Repeat",
            SensorKind::Episode => "Episode",
            SensorKind::Genre => "Genre",
            SensorKind::ContentRating => "Content rating",
            SensorKind::AudioFormat => "Audio format",
            SensorKind::IsLive => "Live",
            SensorKind::VolumeControl => "Volume control",
        },
    }
}

fn name(device: &str, suffix: &str) -> HashMap<String, String> {
    let name = if suffix.is_empty() {
        device.to_string()
    } else {
        format!("{device} {suffix}")
    };
    HashMap::from([("en".to_string(), name)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_store::KEY_LEN;

    #[test]
    fn entity_ids_fit_the_limit() {
        let key = "A".repeat(KEY_LEN);
        for kind in EntityKind::all() {
            let id = kind.entity_id(&key);
            assert!(id.len() <= MAX_ENTITY_ID_LEN, "{id} is too long");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
                "{id}"
            );
        }
    }

    #[test]
    fn entity_id_round_trip() {
        for kind in EntityKind::all() {
            let id = kind.entity_id("AABBCCDDEEFF");
            assert_eq!(Some(("AABBCCDDEEFF", kind)), EntityKind::parse(&id), "{id}");
        }
    }

    #[test]
    fn invalid_entity_ids() {
        assert_eq!(None, EntityKind::parse("media_player"));
        assert_eq!(None, EntityKind::parse("select.KEY"));
        assert_eq!(None, EntityKind::parse("select.KEY.unknown"));
        assert_eq!(None, EntityKind::parse("remote.KEY.app"));
        assert_eq!(None, EntityKind::parse("light.KEY"));
    }
}
