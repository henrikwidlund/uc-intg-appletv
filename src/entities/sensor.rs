// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Sensor entities.

use crate::device::state::{Dirty, Power, Snapshot};
use crate::entities::{SelectKind, SensorKind, select};
use ratv::mrp::{AudioFormat, PlaybackState};
use serde_json::{Map, Value};
use std::borrow::Cow;
use uc_api::SensorDeviceClass;
use uc_api::intg::AvailableIntgEntity;

pub fn definition(mut entity: AvailableIntgEntity, kind: SensorKind) -> AvailableIntgEntity {
    let class = if is_binary(kind) {
        SensorDeviceClass::Binary
    } else {
        SensorDeviceClass::Custom
    };
    entity.device_class = Some(class.as_ref().into());
    entity
}

const fn is_binary(kind: SensorKind) -> bool {
    matches!(kind, SensorKind::IsLive | SensorKind::VolumeControl)
}

/// Select whose current option is the value of the sensor.
const fn mirror(kind: SensorKind) -> Option<SelectKind> {
    match kind {
        SensorKind::App => Some(SelectKind::App),
        SensorKind::AudioOutput => Some(SelectKind::AudioOutput),
        SensorKind::AudioLanguage => Some(SelectKind::AudioLanguage),
        SensorKind::Subtitles => Some(SelectKind::Subtitles),
        SensorKind::PlaybackSpeed => Some(SelectKind::PlaybackSpeed),
        SensorKind::Shuffle => Some(SelectKind::Shuffle),
        SensorKind::Repeat => Some(SelectKind::Repeat),
        _ => None,
    }
}

const fn dirty_groups(kind: SensorKind) -> Dirty {
    match mirror(kind) {
        Some(s) => select::dirty_groups(s),
        None => match kind {
            SensorKind::VolumeControl => Dirty::VOLUME,
            SensorKind::IsLive => Dirty::METADATA.or(Dirty::PLAYBACK).or(Dirty::POSITION),
            _ => Dirty::METADATA,
        },
    }
}

/// Insert the attributes of a sensor if one of its groups is dirty.
pub fn attributes(
    s: &Snapshot,
    kind: SensorKind,
    dirty: Dirty,
    now: f64,
    out: &mut Map<String, Value>,
) {
    if dirty.any(Dirty::CONNECTION) {
        out.insert(
            "state".into(),
            if s.connected { "ON" } else { "UNAVAILABLE" }.into(),
        );
    }
    if dirty.any(dirty_groups(kind) | Dirty::CONNECTION | Dirty::POWER) {
        out.insert("value".into(), value(s, kind, now));
    }
}

fn value(s: &Snapshot, kind: SensorKind, now: f64) -> Value {
    if let Some(select) = mirror(kind) {
        return select::current(s, select, now)
            .unwrap_or_default()
            .into_owned()
            .into();
    }
    // Values are kept while disconnected, like the media player attributes.
    let off = s.power == Power::Off;
    match kind {
        SensorKind::VolumeControl => match s.volume_available {
            Some(true) => "on".into(),
            Some(false) => "off".into(),
            None => Value::Null,
        },
        SensorKind::IsLive => if !off && is_live(s) { "on" } else { "off" }.into(),
        _ if off => "".into(),
        SensorKind::Episode => episode(s).unwrap_or_default().into(),
        SensorKind::Genre => s.genre.clone().unwrap_or_default().into(),
        SensorKind::ContentRating => s.content_rating.clone().unwrap_or_default().into(),
        SensorKind::AudioFormat => s.audio_format.as_ref().map_or_default(audio_format).into(),
        _ => Value::Null,
    }
}

/// Live content. Apps often do not set `is_live`: then content that plays without a duration
/// counts as live.
fn is_live(s: &Snapshot) -> bool {
    s.is_live.unwrap_or_else(|| {
        matches!(
            s.state,
            Some(PlaybackState::Playing | PlaybackState::Paused)
        ) && s.duration.is_none()
    })
}

/// Episode text, for example `Show - S2E5`.
fn episode(s: &Snapshot) -> Option<String> {
    let series = s.series_name.as_deref();
    let number = match (s.season, s.episode) {
        (Some(season), Some(episode)) => Some(Cow::<str>::Owned(format!("S{season}E{episode}"))),
        (None, Some(episode)) => Some(Cow::Owned(format!("E{episode}"))),
        _ => None,
    };
    match (series, number) {
        (Some(series), Some(number)) => Some(format!("{series} - {number}")),
        (Some(series), None) => Some(series.to_string()),
        (None, Some(number)) => Some(number.into_owned()),
        (None, None) => None,
    }
}

fn audio_format(f: &AudioFormat) -> String {
    // ratv does not export the tier enum, so the proto names are used.
    let tier = f
        .tier
        .is_some()
        .then(|| f.tier().as_str_name())
        .and_then(|t| match t {
            "LowBandwidthStereo" => Some("Stereo"),
            "HighQualityStereo" => Some("High quality stereo"),
            "Lossless" => Some("Lossless"),
            "HighResolutionLossless" => Some("Hi-Res Lossless"),
            "Spatial" => Some("Spatial"),
            _ => None,
        });
    match (tier, f.spatialized == Some(true)) {
        (Some(t), true) if t != "Spatial" => format!("{t}, spatial"),
        (Some(t), _) => t.to_string(),
        (None, true) => "Spatial".into(),
        (None, false) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_without_duration() {
        let mut s = Snapshot {
            state: Some(PlaybackState::Playing),
            ..Snapshot::default()
        };
        assert!(is_live(&s));
        s.duration = Some(60.0);
        assert!(!is_live(&s));
        s.is_live = Some(true);
        assert!(is_live(&s));
        s.is_live = None;
        s.duration = None;
        s.state = Some(PlaybackState::Stopped);
        assert!(!is_live(&s));
    }

    #[test]
    fn episode_text() {
        let mut s = Snapshot {
            series_name: Some("Show".into()),
            season: Some(2),
            episode: Some(5),
            ..Snapshot::default()
        };
        assert_eq!(Some("Show - S2E5".into()), episode(&s));
        s.season = None;
        assert_eq!(Some("Show - E5".into()), episode(&s));
        s.series_name = None;
        s.episode = None;
        assert_eq!(None, episode(&s));
    }

    #[test]
    fn binary_sensor_values() {
        let mut s = Snapshot {
            connected: true,
            ..Snapshot::default()
        };
        assert_eq!(Value::Null, value(&s, SensorKind::VolumeControl, 0.0));
        s.volume_available = Some(false);
        assert_eq!(
            Value::from("off"),
            value(&s, SensorKind::VolumeControl, 0.0)
        );
        s.is_live = Some(true);
        assert_eq!(Value::from("on"), value(&s, SensorKind::IsLive, 0.0));
    }
}
