// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Media player entity.

use crate::device::state::{Dirty, Power, Snapshot};
use crate::entities::SelectKind;
use crate::errors::ServiceError;
use ratv::Key;
use ratv::mrp::{MediaSubType, MediaType, PlaybackState, RepeatMode, ShuffleMode};
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::time::Duration;
use uc_api::intg::AvailableIntgEntity;
use uc_api::{
    MediaContentType, MediaPlayerAttribute as Attr, MediaPlayerCommand as Cmd,
    MediaPlayerDeviceClass, MediaPlayerFeature as F, MediaPlayerOptionField, MediaPlayerRepeatMode,
};

/// Additional commands that are not covered by the media player features.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum SimpleCommand {
    AppSwitcher,
    Screensaver,
    ControlCenter,
    SkipForward,
    SkipBackward,
    FastForwardBegin,
    FastForwardEnd,
    RewindBegin,
    RewindEnd,
    SwipeLeft,
    SwipeRight,
    SwipeUp,
    SwipeDown,
    Play,
    Pause,
    PlayPauseKey,
    Like,
    Dislike,
    NextChapter,
    PreviousChapter,
}

const FEATURES: &[F] = &[
    F::OnOff,
    F::Toggle,
    F::PlayPause,
    F::Stop,
    F::Next,
    F::Previous,
    F::FastForward,
    F::Rewind,
    F::Seek,
    F::Repeat,
    F::Shuffle,
    F::MediaDuration,
    F::MediaPosition,
    F::MediaTitle,
    F::MediaArtist,
    F::MediaAlbum,
    F::MediaImageUrl,
    F::MediaType,
    F::Home,
    F::Menu,
    F::ContextMenu,
    F::Guide,
    F::DPad,
    F::ChannelSwitcher,
    F::SelectSource,
    F::SelectSoundMode,
    F::AudioTrack,
    F::Subtitle,
    F::PlayMedia,
    F::BrowseMedia,
];

const VOLUME_FEATURES: &[F] = &[
    F::Volume,
    F::VolumeUpDown,
    F::MuteToggle,
    F::Mute,
    F::Unmute,
];

/// Browse and play media ids are always stable: bit 0.
const STABLE_ID_SUPPORT: u8 = 1;

pub fn definition(mut entity: AvailableIntgEntity) -> AvailableIntgEntity {
    use strum::VariantArray;

    entity.device_class = Some(MediaPlayerDeviceClass::TV.as_ref().into());
    entity.features = Some(
        FEATURES
            .iter()
            .chain(VOLUME_FEATURES)
            .map(|f| f.as_ref().to_string())
            .collect(),
    );
    let simple: Vec<&str> = SimpleCommand::VARIANTS.iter().map(AsRef::as_ref).collect();
    let mut options = Map::new();
    options.insert(
        MediaPlayerOptionField::SimpleCommands.as_ref().into(),
        json!(simple),
    );
    options.insert(
        MediaPlayerOptionField::StableIdSupport.as_ref().into(),
        json!(STABLE_ID_SUPPORT),
    );
    entity.options = Some(options);
    entity
}

/// Entity state of the media player.
pub fn state(s: &Snapshot) -> &'static str {
    if !s.connected {
        return "UNAVAILABLE";
    }
    if s.power == Power::Off {
        return "OFF";
    }
    match s.state {
        Some(PlaybackState::Playing) => "PLAYING",
        Some(PlaybackState::Paused | PlaybackState::Interrupted) => "PAUSED",
        Some(PlaybackState::Seeking) => "BUFFERING",
        _ => "STANDBY",
    }
}

/// Insert the attributes of the dirty groups.
pub fn attributes(s: &Snapshot, dirty: Dirty, now: f64, out: &mut Map<String, Value>) {
    if dirty.any(Dirty::CONNECTION | Dirty::POWER | Dirty::PLAYBACK) {
        set(out, Attr::State, state(s).into());
    }
    // Media attributes are empty while off. They are kept while disconnected.
    let off = s.power == Power::Off;

    if dirty.any(Dirty::VOLUME | Dirty::CONNECTION) {
        if let Some(v) = s.volume_percent() {
            set(out, Attr::Volume, v.into());
        }
        if let Some(m) = s.muted {
            set(out, Attr::Muted, m.into());
        }
    }
    if dirty.any(Dirty::METADATA | Dirty::POWER | Dirty::CONNECTION) {
        metadata_attributes(s, off, out);
    }
    // With PLAYBACK, so a resume sends a new timestamp.
    if dirty.any(Dirty::POSITION | Dirty::PLAYBACK | Dirty::POWER | Dirty::CONNECTION) {
        position_attributes(s, off, now, out);
    }
    if dirty.any(Dirty::ARTWORK | Dirty::POWER | Dirty::CONNECTION) {
        let url = if off || s.state.is_none() {
            None
        } else {
            s.artwork_url.as_deref()
        };
        set(out, Attr::MediaImageUrl, url.unwrap_or("").into());
    }
    if dirty.any(Dirty::APPS | Dirty::CONNECTION) {
        let list: Vec<&str> = s.apps.iter().map(|a| a.name.as_str()).collect();
        set(out, Attr::SourceList, json!(list));
    }
    if dirty.any(Dirty::APPS | Dirty::APP | Dirty::CONNECTION) {
        set(out, Attr::Source, current_app(s).unwrap_or("").into());
    }
    if dirty.any(Dirty::OUTPUTS | Dirty::CONNECTION) {
        let list: Vec<&str> = s.output_combos.iter().map(|c| c.name.as_str()).collect();
        set(out, Attr::SoundModeList, json!(list));
        let current = s
            .current_output_combo()
            .map_or("", |i| s.output_combos[i].name.as_str());
        set(out, Attr::SoundMode, current.into());
    }
    if dirty.any(Dirty::SHUFFLE_REPEAT | Dirty::CONNECTION) {
        shuffle_repeat_attributes(s, out);
    }
}

fn set(out: &mut Map<String, Value>, a: Attr, v: Value) {
    out.insert(a.as_ref().into(), v);
}

fn metadata_attributes(s: &Snapshot, off: bool, out: &mut Map<String, Value>) {
    let text =
        |v: &Option<String>| -> Value { if off { "" } else { v.as_deref().unwrap_or("") }.into() };
    let title = if off { "" } else { title(s).unwrap_or("") };
    set(out, Attr::MediaTitle, title.into());
    // TV shows have the series name as artist.
    let artist = if s.artist.is_some() {
        &s.artist
    } else {
        &s.series_name
    };
    set(out, Attr::MediaArtist, text(artist));
    set(out, Attr::MediaAlbum, text(&s.album));
    let media_type = if off { None } else { media_type(s) };
    set(
        out,
        Attr::MediaType,
        media_type
            .as_ref()
            .map_or("", MediaContentType::as_str)
            .into(),
    );
}

fn position_attributes(s: &Snapshot, off: bool, now: f64, out: &mut Map<String, Value>) {
    let pos = if off { None } else { s.position(now) };
    let duration = if off { None } else { s.duration };
    set(
        out,
        Attr::MediaPosition,
        pos.map_or(0, |p| p.round() as u64).into(),
    );
    set(
        out,
        Attr::MediaDuration,
        duration.map_or(0, |d| d.round() as u64).into(),
    );
    // The controller sets the time only if it sends the position, see [`position_timestamp`].
    set(out, Attr::MediaPositionUpdatedAt, Value::Null);
}

/// Time for `media_position_updated_at`, like `2026-11-04T15:37:27.106292+00:00`.
pub fn position_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

fn shuffle_repeat_attributes(s: &Snapshot, out: &mut Map<String, Value>) {
    let repeat = match s.repeat {
        Some(RepeatMode::All) => MediaPlayerRepeatMode::All,
        Some(RepeatMode::One) => MediaPlayerRepeatMode::One,
        _ => MediaPlayerRepeatMode::Off,
    };
    set(out, Attr::Repeat, repeat.as_ref().into());
    let shuffle = matches!(s.shuffle, Some(ShuffleMode::Songs | ShuffleMode::Albums));
    set(out, Attr::Shuffle, shuffle.into());
}

/// Media title. For a TV episode it is the episode title, and the show is the artist.
fn title(s: &Snapshot) -> Option<&str> {
    s.subtitle
        .as_deref()
        .filter(|_| s.episode.is_some())
        .or(s.title.as_deref())
}

/// Name of the app in the foreground.
pub fn current_app(s: &Snapshot) -> Option<&str> {
    let name = |bundle_id: &str| {
        s.apps
            .iter()
            .find(|a| a.bundle_id == bundle_id)
            .map(|a| a.name.as_str())
    };
    if let Some(launched) = s.launched_app.as_deref() {
        return name(launched);
    }
    s.app_name.as_deref().or_else(|| name(s.app.as_deref()?))
}

fn media_type(s: &Snapshot) -> Option<MediaContentType> {
    match s.media_type? {
        MediaType::Audio => Some(match s.media_sub_type {
            Some(MediaSubType::Podcast) => MediaContentType::Podcast,
            _ => MediaContentType::Music,
        }),
        MediaType::Video if s.series_name.is_some() || s.episode.is_some() => {
            Some(MediaContentType::TvShow)
        }
        MediaType::Video => Some(MediaContentType::Video),
        MediaType::UnknownMediaType => None,
    }
}

/// Swipe direction on the touch surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swipe {
    Left,
    Right,
    Up,
    Down,
}

/// Operation on a select entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectOp {
    Option(String),
    First,
    Last,
    Next { cycle: bool },
    Previous { cycle: bool },
}

/// Action to run on the Apple TV.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    On,
    Off,
    Toggle,
    Press(Key),
    Hold(Key, Duration),
    PlayPause,
    PlayPauseKey,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    FastForwardBegin,
    FastForwardEnd,
    RewindBegin,
    RewindEnd,
    Seek(f64),
    /// Skip forward, or backward if `false`, by the interval of the app.
    Skip(bool),
    /// Volume in percent.
    Volume(u8),
    VolumeUp,
    VolumeDown,
    MuteToggle,
    Mute(bool),
    Repeat(RepeatMode),
    Shuffle(ShuffleMode),
    Swipe(Swipe),
    AppSwitcher,
    ContextMenu,
    ControlCenter,
    SelectSource(String),
    SelectSoundMode(String),
    PlayMedia(String),
    CycleAudioTrack,
    CycleSubtitle,
    NextChapter,
    PreviousChapter,
    Like,
    Dislike,
    Select(SelectKind, SelectOp),
}

impl Action {
    /// Returns `true` for actions that need the Apple TV to be on.
    pub const fn needs_power(&self) -> bool {
        !matches!(self, Self::On | Self::Off | Self::Toggle)
    }

    /// Returns `true` for playback actions that end an active fast forward or rewind.
    pub const fn ends_seek(&self) -> bool {
        matches!(
            self,
            Self::PlayPause
                | Self::PlayPauseKey
                | Self::Play
                | Self::Pause
                | Self::Stop
                | Self::Next
                | Self::Previous
                | Self::Skip(_)
                | Self::Seek(_)
                | Self::FastForwardEnd
                | Self::RewindEnd
        )
    }

    /// Key of a key action. Used for the `hold` parameter of the remote entity.
    pub const fn key(&self) -> Option<Key> {
        match self {
            Self::Press(k) | Self::Hold(k, _) => Some(*k),
            _ => None,
        }
    }
}

/// Map a media player command to an action.
pub fn command(cmd_id: &str, params: Option<&Map<String, Value>>) -> Result<Action, ServiceError> {
    if let Ok(simple) = SimpleCommand::from_str(cmd_id) {
        return Ok(simple_command(simple));
    }
    let cmd = Cmd::from_str(cmd_id)
        .map_err(|_| ServiceError::BadRequest(format!("Unknown command: {cmd_id}")))?;
    let action = match cmd {
        Cmd::On => Action::On,
        Cmd::Off => Action::Off,
        Cmd::Toggle => Action::Toggle,
        Cmd::PlayPause => Action::PlayPause,
        Cmd::Stop => Action::Stop,
        Cmd::Previous => Action::Previous,
        Cmd::Next => Action::Next,
        Cmd::FastForward => Action::FastForwardBegin,
        Cmd::Rewind => Action::RewindBegin,
        Cmd::Seek => {
            let position = param_f64(params, "media_position")?;
            if position < 0.0 {
                return Err(ServiceError::BadRequest(
                    "Invalid parameter: media_position".into(),
                ));
            }
            Action::Seek(position)
        }
        Cmd::Volume => {
            let v = param_f64(params, "volume")?;
            if !(0.0..=100.0).contains(&v) {
                return Err(ServiceError::BadRequest("Invalid parameter: volume".into()));
            }
            Action::Volume(v.round() as u8)
        }
        Cmd::VolumeUp => Action::VolumeUp,
        Cmd::VolumeDown => Action::VolumeDown,
        Cmd::MuteToggle => Action::MuteToggle,
        Cmd::Mute => Action::Mute(true),
        Cmd::Unmute => Action::Mute(false),
        Cmd::Repeat => {
            let mode = param_str(params, "repeat")?;
            Action::Repeat(match MediaPlayerRepeatMode::from_str(mode)? {
                MediaPlayerRepeatMode::Off => RepeatMode::Off,
                MediaPlayerRepeatMode::All => RepeatMode::All,
                MediaPlayerRepeatMode::One => RepeatMode::One,
            })
        }
        Cmd::Shuffle => {
            let on = params
                .and_then(|p| p.get("shuffle"))
                .and_then(Value::as_bool)
                .ok_or_else(|| ServiceError::BadRequest("Missing parameter: shuffle".into()))?;
            Action::Shuffle(if on {
                ShuffleMode::Songs
            } else {
                ShuffleMode::Off
            })
        }
        Cmd::ChannelUp => Action::Press(Key::ChannelUp),
        Cmd::ChannelDown => Action::Press(Key::ChannelDown),
        Cmd::CursorUp => Action::Press(Key::Up),
        Cmd::CursorDown => Action::Press(Key::Down),
        Cmd::CursorLeft => Action::Press(Key::Left),
        Cmd::CursorRight => Action::Press(Key::Right),
        Cmd::CursorEnter => Action::Press(Key::Select),
        Cmd::Home => Action::Press(Key::Home),
        // Control Center, as in the Python driver.
        Cmd::Menu => Action::ControlCenter,
        Cmd::ContextMenu => Action::ContextMenu,
        Cmd::Guide => Action::Press(Key::Guide),
        Cmd::Back => Action::Press(Key::Menu),
        Cmd::SelectSource => Action::SelectSource(param_str(params, "source")?.into()),
        Cmd::SelectSoundMode => Action::SelectSoundMode(param_str(params, "mode")?.into()),
        Cmd::AudioTrack => Action::CycleAudioTrack,
        Cmd::Subtitle => Action::CycleSubtitle,
        Cmd::PlayMedia => Action::PlayMedia(param_str(params, "media_id")?.into()),
        _ => return Err(ServiceError::NotSupported(cmd_id.into())),
    };
    Ok(action)
}

const fn simple_command(cmd: SimpleCommand) -> Action {
    match cmd {
        SimpleCommand::AppSwitcher => Action::AppSwitcher,
        SimpleCommand::Screensaver => Action::Press(Key::Screensaver),
        SimpleCommand::ControlCenter => Action::ControlCenter,
        SimpleCommand::SkipForward => Action::Skip(true),
        SimpleCommand::SkipBackward => Action::Skip(false),
        SimpleCommand::FastForwardBegin => Action::FastForwardBegin,
        SimpleCommand::FastForwardEnd => Action::FastForwardEnd,
        SimpleCommand::RewindBegin => Action::RewindBegin,
        SimpleCommand::RewindEnd => Action::RewindEnd,
        SimpleCommand::SwipeLeft => Action::Swipe(Swipe::Left),
        SimpleCommand::SwipeRight => Action::Swipe(Swipe::Right),
        SimpleCommand::SwipeUp => Action::Swipe(Swipe::Up),
        SimpleCommand::SwipeDown => Action::Swipe(Swipe::Down),
        SimpleCommand::Play => Action::Play,
        SimpleCommand::Pause => Action::Pause,
        SimpleCommand::PlayPauseKey => Action::PlayPauseKey,
        SimpleCommand::Like => Action::Like,
        SimpleCommand::Dislike => Action::Dislike,
        SimpleCommand::NextChapter => Action::NextChapter,
        SimpleCommand::PreviousChapter => Action::PreviousChapter,
    }
}

pub fn param_str<'a>(
    params: Option<&'a Map<String, Value>>,
    name: &str,
) -> Result<&'a str, ServiceError> {
    params
        .and_then(|p| p.get(name))
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::BadRequest(format!("Missing parameter: {name}")))
}

/// Number parameter. `NaN` and infinite values are rejected.
fn param_f64(params: Option<&Map<String, Value>>, name: &str) -> Result<f64, ServiceError> {
    let value = params
        .and_then(|p| p.get(name))
        .ok_or_else(|| ServiceError::BadRequest(format!("Missing parameter: {name}")))?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|v: &f64| v.is_finite())
        .ok_or_else(|| ServiceError::BadRequest(format!("Invalid parameter: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launched_app_is_shown_until_another_app_plays() {
        let app = |id: &str, name: &str| crate::device::state::AppInfo {
            bundle_id: id.into(),
            name: name.into(),
        };
        let mut s = Snapshot {
            apps: vec![app("music", "Music"), app("nrk", "NRK TV")],
            app: Some("music".into()),
            launched_app: Some("nrk".into()),
            ..Snapshot::default()
        };
        assert_eq!(Some("NRK TV"), current_app(&s));

        // Pause in the same app: the launched app stays.
        let mut np = ratv::mrp::NowPlaying::default();
        np.app = Some("music".into());
        s.update_now_playing(&np);
        assert_eq!(Some("NRK TV"), current_app(&s));

        np.app = Some("nrk".into());
        s.update_now_playing(&np);
        assert_eq!(None, s.launched_app);
        assert_eq!(Some("NRK TV"), current_app(&s));
    }

    #[test]
    fn episode_title_is_media_title() {
        let mut s = Snapshot {
            title: Some("Slow Horses".into()),
            subtitle: Some("Failure's Contagious".into()),
            ..Snapshot::default()
        };
        assert_eq!(Some("Slow Horses"), title(&s));
        s.episode = Some(1);
        assert_eq!(Some("Failure's Contagious"), title(&s));
    }

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn map_commands() {
        assert_eq!(
            Action::Press(Key::Up),
            command("cursor_up", None).expect("ok")
        );
        assert_eq!(Action::Press(Key::Menu), command("back", None).expect("ok"));
        assert_eq!(Action::ControlCenter, command("menu", None).expect("ok"));
        assert_eq!(
            Action::ContextMenu,
            command("context_menu", None).expect("ok")
        );
        assert!(command("TOP_MENU", None).is_err());
        assert_eq!(
            Action::FastForwardBegin,
            command("fast_forward", None).expect("ok")
        );
        assert_eq!(
            Action::Skip(false),
            command("SKIP_BACKWARD", None).expect("ok")
        );
    }

    #[test]
    fn map_commands_with_params() {
        let p = params(json!({ "volume": 42.4 }));
        assert_eq!(Action::Volume(42), command("volume", Some(&p)).expect("ok"));
        let p = params(json!({ "volume": "100" }));
        assert_eq!(
            Action::Volume(100),
            command("volume", Some(&p)).expect("ok")
        );
        let p = params(json!({ "media_position": 61 }));
        assert_eq!(Action::Seek(61.0), command("seek", Some(&p)).expect("ok"));
        let p = params(json!({ "repeat": "ONE" }));
        assert_eq!(
            Action::Repeat(RepeatMode::One),
            command("repeat", Some(&p)).expect("ok")
        );
        let p = params(json!({ "shuffle": true }));
        assert_eq!(
            Action::Shuffle(ShuffleMode::Songs),
            command("shuffle", Some(&p)).expect("ok")
        );
        let p = params(json!({ "source": "Music" }));
        assert_eq!(
            Action::SelectSource("Music".into()),
            command("select_source", Some(&p)).expect("ok")
        );
    }

    #[test]
    fn invalid_commands() {
        assert!(matches!(
            command("foo", None),
            Err(ServiceError::BadRequest(_))
        ));
        assert!(matches!(
            command("volume", None),
            Err(ServiceError::BadRequest(_))
        ));
        assert!(matches!(
            command("eject", None),
            Err(ServiceError::NotSupported(_))
        ));
    }

    #[test]
    fn invalid_numbers() {
        for (cmd, p) in [
            ("volume", json!({ "volume": 150 })),
            ("volume", json!({ "volume": -1 })),
            ("volume", json!({ "volume": "NaN" })),
            ("volume", json!({ "volume": "inf" })),
            ("seek", json!({ "media_position": -5 })),
            ("seek", json!({ "media_position": "NaN" })),
            ("seek", json!({ "media_position": "-inf" })),
        ] {
            assert!(
                matches!(
                    command(cmd, Some(&params(p.clone()))),
                    Err(ServiceError::BadRequest(_))
                ),
                "{cmd} {p}"
            );
        }
        let p = params(json!({ "media_position": 0 }));
        assert_eq!(Action::Seek(0.0), command("seek", Some(&p)).expect("ok"));
    }

    #[test]
    fn simple_command_names_are_valid() {
        use strum::VariantArray;
        for c in SimpleCommand::VARIANTS {
            let name = c.as_ref();
            assert!(name.len() <= 20, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "{name}"
            );
        }
    }

    #[test]
    fn state_mapping() {
        let mut s = Snapshot::default();
        assert_eq!("UNAVAILABLE", state(&s));
        s.connected = true;
        assert_eq!("STANDBY", state(&s));
        s.state = Some(PlaybackState::Playing);
        assert_eq!("PLAYING", state(&s));
        s.state = Some(PlaybackState::Interrupted);
        assert_eq!("PAUSED", state(&s));
        s.state = Some(PlaybackState::Seeking);
        assert_eq!("BUFFERING", state(&s));
        s.power = Power::Off;
        assert_eq!("OFF", state(&s));
    }

    #[test]
    fn position_timestamp_has_microseconds_and_offset() {
        let ts = position_timestamp();
        let ts = ts.as_str();
        // Same shape as `2026-11-04T15:37:27.106292+00:00`.
        assert_eq!(32, ts.len(), "{ts}");
        assert!(ts.ends_with("+00:00"), "{ts}");
        assert_eq!(Some('.'), ts.chars().nth(19), "{ts}");
        assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "{ts}");
    }

    #[test]
    fn media_cleared_when_off() {
        let s = Snapshot {
            connected: true,
            power: Power::Off,
            title: Some("Song".into()),
            ..Snapshot::default()
        };
        let mut attrs = Map::new();
        attributes(&s, Dirty::ALL, 0.0, &mut attrs);
        assert_eq!(Some(&json!("")), attrs.get("media_title"));
        assert_eq!(Some(&json!("OFF")), attrs.get("state"));
    }
}
