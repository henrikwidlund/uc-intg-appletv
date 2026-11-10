// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Last known state of one Apple TV.
//!
//! The snapshot copies only the fields that changed, and reports them as [`Dirty`] groups. Only
//! the attributes of dirty groups are built again.

use bytes::Bytes;
use ratv::companion::AttentionState;
use ratv::mrp::{
    AudioFormat, Chapter, Command, LanguageOption, MediaSubType, MediaType, NowPlaying,
    OutputDevice, PlaybackState, QueueItem, RepeatMode, ShuffleMode, Volume,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::{BitOr, BitOrAssign};
use std::time::{Duration, Instant};

/// Size of the requested artwork.
pub const ARTWORK_SIZE: u32 = 250;

/// Groups of changed state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dirty(u32);

impl Dirty {
    pub const NONE: Self = Self(0);
    pub const CONNECTION: Self = Self(1);
    pub const POWER: Self = Self(1 << 1);
    pub const PLAYBACK: Self = Self(1 << 2);
    pub const METADATA: Self = Self(1 << 3);
    pub const POSITION: Self = Self(1 << 4);
    pub const ARTWORK: Self = Self(1 << 5);
    pub const VOLUME: Self = Self(1 << 6);
    /// The app list changed.
    pub const APPS: Self = Self(1 << 7);
    pub const OUTPUTS: Self = Self(1 << 8);
    /// The language lists changed.
    pub const LANGUAGES: Self = Self(1 << 9);
    pub const CHAPTERS: Self = Self(1 << 10);
    pub const QUEUE: Self = Self(1 << 11);
    /// The playback rate changed.
    pub const RATE: Self = Self(1 << 12);
    pub const SHUFFLE_REPEAT: Self = Self(1 << 13);
    pub const ACCOUNTS: Self = Self(1 << 14);
    /// The current chapter changed, but not the list.
    pub const CHAPTER: Self = Self(1 << 15);
    /// The current app changed, but not the list.
    pub const APP: Self = Self(1 << 16);
    /// The current languages changed, but not the lists.
    pub const LANGUAGE: Self = Self(1 << 17);
    /// The supported playback rates changed.
    pub const RATES: Self = Self(1 << 18);
    pub const ALL: Self = Self(u32::MAX);

    /// Returns `true` if any group of `other` is dirty.
    pub const fn any(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Both groups, for use in constants.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Dirty {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Dirty {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Power state from the Companion attention state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Power {
    /// Companion is not connected or did not answer yet.
    #[default]
    Unknown,
    On,
    Off,
}

impl From<AttentionState> for Power {
    fn from(state: AttentionState) -> Self {
        match state {
            AttentionState::Asleep => Self::Off,
            AttentionState::Awake | AttentionState::Idle | AttentionState::Screensaver => Self::On,
            _ => Self::Unknown,
        }
    }
}

/// Installed app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
}

/// User account on the Apple TV.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub id: String,
    pub name: String,
}

/// One entry of the audio output select: a group of AirPlay outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCombo {
    pub name: String,
    /// Output uids besides the Apple TV. Empty means only the Apple TV.
    pub uids: Vec<String>,
}

/// Last known state of one Apple TV.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Snapshot {
    pub connected: bool,
    /// MRP is connected: needed for volume and the full now playing.
    pub mrp: bool,
    /// Companion or MRP is connected: needed for Siri.
    pub siri: bool,
    pub power: Power,
    pub name: String,

    pub app: Option<String>,
    pub app_name: Option<String>,
    pub state: Option<PlaybackState>,
    pub title: Option<String>,
    /// Episode title for TV shows.
    pub subtitle: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub series_name: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub genre: Option<String>,
    pub content_rating: Option<String>,
    pub media_type: Option<MediaType>,
    pub media_sub_type: Option<MediaSubType>,
    pub is_live: Option<bool>,
    pub audio_format: Option<AudioFormat>,
    pub duration: Option<f64>,
    pub elapsed: Option<f64>,
    pub elapsed_timestamp: Option<f64>,
    pub rate: Option<f32>,
    pub shuffle: Option<ShuffleMode>,
    pub repeat: Option<RepeatMode>,
    pub audio_languages: Vec<LanguageOption>,
    pub subtitle_languages: Vec<LanguageOption>,
    pub audio_language: Option<LanguageOption>,
    pub subtitle_language: Option<LanguageOption>,
    /// Subtitles are set to Auto. `subtitle_language` is then the Auto option.
    pub subtitles_auto: bool,
    /// True if subtitles are shown, from Companion.
    pub captions_enabled: Option<bool>,
    pub chapters: Vec<Chapter>,
    pub up_next: Vec<QueueItem>,
    /// Rates as the app lists them, to find changes.
    pub rates: Vec<f32>,
    /// Playback speeds for the select, from [`NowPlaying::playback_speeds`].
    pub supported_rates: Vec<f32>,
    /// Commands the app supports. Empty if nothing plays.
    pub commands: Vec<Command>,
    /// Skip intervals of the app in seconds.
    pub skip_forward_interval: Option<f64>,
    pub skip_backward_interval: Option<f64>,
    pub artwork_id: Option<String>,
    /// CDN artwork URL, or the fallback route URL.
    pub artwork_url: Option<String>,
    /// The app gives no artwork URL: the driver fetches the image.
    pub artwork_fetch: bool,
    /// Image of the item from Companion. Saves the fetch over MRP.
    pub artwork_data: Option<Bytes>,
    /// Changes when title, artist or album change.
    pub item_seq: u32,
    /// `item_seq` of the item that `artwork_url` belongs to.
    pub artwork_item: u32,
    /// `artwork_url` was kept from the item before.
    pub artwork_kept: bool,
    /// Start of the current item, or the last image change of it.
    pub item_started: Option<Instant>,
    /// How long the item before lasted, from its last image change.
    pub prev_item_lasted: Option<Duration>,

    pub volume_level: Option<f32>,
    pub muted: Option<bool>,
    pub volume_available: Option<bool>,
    pub outputs: Vec<OutputDevice>,
    /// Output uid of the Apple TV itself.
    pub own_output_uid: Option<String>,
    pub global_volume: bool,

    /// Installed apps, sorted by name.
    pub apps: Vec<AppInfo>,
    pub accounts: Vec<AccountInfo>,
    /// Last account chosen through the driver. The Apple TV does not report it.
    pub current_account: Option<String>,
    /// Bundle id of the app opened through the driver. Shown until another app plays media.
    pub launched_app: Option<String>,
    /// AirPlay devices on the network that can join the group: (uid, name).
    pub airplay_devices: Vec<(String, String)>,
    /// Derived from `airplay_devices` and `outputs`.
    pub output_combos: Vec<OutputCombo>,
}

/// Copy a field if it changed and mark the group dirty.
macro_rules! update {
    ($dirty:ident, $group:expr, $dst:expr, $src:expr) => {
        if $dst != $src {
            $dst.clone_from(&$src);
            $dirty |= $group;
        }
    };
}

impl Snapshot {
    /// Copy the changed fields of a now playing update.
    pub fn update_now_playing(&mut self, np: &NowPlaying) -> Dirty {
        let mut d = Dirty::NONE;
        let item_changed =
            self.title != np.title || self.artist != np.artist || self.album != np.album;
        if item_changed {
            self.item_seq = self.item_seq.wrapping_add(1);
            let now = Instant::now();
            self.prev_item_lasted = self.item_started.map(|t| now - t);
            self.item_started = Some(now);
        }
        let had_state = self.state.is_some();
        update!(d, Dirty::PLAYBACK, self.state, np.state);
        // The image is only shown and fetched with a state.
        if had_state != self.state.is_some() {
            d |= Dirty::ARTWORK;
        }
        if np.app.is_some() && self.app != np.app && self.launched_app.take().is_some() {
            d |= Dirty::APP;
        }
        update!(d, Dirty::APP, self.app, np.app);
        update!(d, Dirty::APP, self.app_name, np.app_name);
        update!(d, Dirty::METADATA, self.title, np.title);
        update!(d, Dirty::METADATA, self.subtitle, np.subtitle);
        update!(d, Dirty::METADATA, self.artist, np.artist);
        update!(d, Dirty::METADATA, self.album, np.album);
        update!(d, Dirty::METADATA, self.series_name, np.series_name);
        update!(d, Dirty::METADATA, self.season, np.season);
        update!(d, Dirty::METADATA, self.episode, np.episode);
        update!(d, Dirty::METADATA, self.genre, np.genre);
        update!(d, Dirty::METADATA, self.content_rating, np.content_rating);
        update!(d, Dirty::METADATA, self.media_type, np.media_type);
        update!(d, Dirty::METADATA, self.media_sub_type, np.media_sub_type);
        update!(d, Dirty::METADATA, self.is_live, np.is_live);
        update!(d, Dirty::METADATA, self.audio_format, np.audio_format);
        // Live content can report a duration of 0.
        let duration = np.duration.filter(|d| *d > 0.0);
        update!(d, Dirty::POSITION, self.duration, duration);
        update!(d, Dirty::POSITION, self.elapsed, np.elapsed);
        update!(
            d,
            Dirty::POSITION,
            self.elapsed_timestamp,
            np.elapsed_timestamp
        );
        update!(d, Dirty::POSITION | Dirty::RATE, self.rate, np.rate);
        update!(d, Dirty::SHUFFLE_REPEAT, self.shuffle, np.shuffle);
        update!(d, Dirty::SHUFFLE_REPEAT, self.repeat, np.repeat);
        update!(
            d,
            Dirty::LANGUAGES,
            self.audio_languages,
            np.audio_languages
        );
        update!(
            d,
            Dirty::LANGUAGES,
            self.subtitle_languages,
            np.subtitle_languages
        );
        update!(d, Dirty::LANGUAGE, self.audio_language, np.audio_language);
        update!(
            d,
            Dirty::LANGUAGE,
            self.subtitle_language,
            np.subtitle_language
        );
        update!(d, Dirty::LANGUAGE, self.subtitles_auto, np.subtitles_auto());
        update!(
            d,
            Dirty::LANGUAGE,
            self.captions_enabled,
            np.captions_enabled
        );
        update!(d, Dirty::CHAPTERS, self.chapters, np.chapters);
        update!(d, Dirty::QUEUE, self.up_next, np.up_next);
        // `playback_speeds` allocates, so call it only when the rates change.
        if self.rates != np.supported_rates {
            self.rates.clone_from(&np.supported_rates);
            let speeds = np.playback_speeds();
            if self.supported_rates != speeds {
                self.supported_rates = speeds;
                d |= Dirty::RATES;
            }
        }
        // Used only for commands.
        if self.commands != np.commands {
            self.commands.clone_from(&np.commands);
        }
        self.skip_forward_interval = np.skip_forward_interval;
        self.skip_backward_interval = np.skip_backward_interval;

        d | self.update_artwork(np, item_changed)
    }

    /// Copy the artwork fields of a now playing update.
    fn update_artwork(&mut self, np: &NowPlaying, item_changed: bool) -> Dirty {
        // Compare the buffers, not the bytes: ratv keeps the buffer while the image is the same.
        let data_changed = match (&self.artwork_data, &np.artwork_data) {
            (Some(a), Some(b)) => a.as_ptr() != b.as_ptr() || a.len() != b.len(),
            (a, b) => a.is_some() != b.is_some(),
        };
        if data_changed {
            self.artwork_data.clone_from(&np.artwork_data);
        }
        if self.artwork_id != np.artwork_id {
            self.artwork_id.clone_from(&np.artwork_id);
            if !item_changed && np.artwork_id.is_some() {
                self.item_started = Some(Instant::now());
            }
            self.set_artwork(np);
            Dirty::ARTWORK
        } else if self.artwork_fetch && np.artwork_url.is_some() {
            // The URL can arrive after the id.
            self.set_artwork(np);
            Dirty::ARTWORK
        } else if self.artwork_id.is_none() && item_changed {
            // Some apps send no artwork id, but the image is there. Ask again for each item.
            self.set_artwork(np);
            Dirty::ARTWORK
        } else if self.artwork_fetch && data_changed && self.artwork_data.is_some() {
            // Companion can send the image after the item.
            Dirty::ARTWORK
        } else {
            Dirty::NONE
        }
    }

    /// Use the CDN URL, or mark the image for fetching. The last image stays until the fetch ends.
    fn set_artwork(&mut self, np: &NowPlaying) {
        match np.artwork_url_sized(ARTWORK_SIZE, ARTWORK_SIZE) {
            Some(url) => {
                self.artwork_url = Some(url);
                self.artwork_item = self.item_seq;
                self.artwork_kept = false;
                self.artwork_fetch = false;
            }
            None => self.artwork_fetch = true,
        }
    }

    /// Copy the changed fields of a volume update.
    pub fn update_volume(&mut self, v: &Volume) -> Dirty {
        let mut d = Dirty::NONE;
        update!(d, Dirty::VOLUME, self.volume_level, v.level);
        update!(d, Dirty::VOLUME, self.muted, v.muted);
        update!(d, Dirty::VOLUME, self.volume_available, v.available);
        if self.outputs != v.outputs {
            // The volume of an output changes often. The combinations only depend on the group.
            let group_changed = self.outputs.len() != v.outputs.len()
                || self
                    .outputs
                    .iter()
                    .zip(&v.outputs)
                    .any(|(a, b)| a.uid != b.uid || a.name != b.name);
            self.outputs.clone_from(&v.outputs);
            d |= Dirty::VOLUME;
            if group_changed {
                d |= Dirty::OUTPUTS;
                self.rebuild_output_combos();
            }
        }
        d
    }

    /// Set the AirPlay devices that can join the group.
    pub fn set_airplay_devices(&mut self, mut devices: Vec<(String, String)>) -> Dirty {
        devices.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        if self.airplay_devices == devices {
            return Dirty::NONE;
        }
        self.airplay_devices = devices;
        self.rebuild_output_combos();
        Dirty::OUTPUTS
    }

    /// Set the installed apps. They are sorted by name, case-insensitive.
    pub fn set_apps(&mut self, mut apps: Vec<AppInfo>) -> Dirty {
        apps.sort_unstable_by(|a, b| {
            let a = a.name.chars().flat_map(char::to_lowercase);
            a.cmp(b.name.chars().flat_map(char::to_lowercase))
        });
        if self.apps == apps {
            return Dirty::NONE;
        }
        self.apps = apps;
        Dirty::APPS
    }

    /// Set the device name. The output names include it.
    pub fn set_name(&mut self, name: &str) -> Dirty {
        if self.name == name {
            return Dirty::NONE;
        }
        name.clone_into(&mut self.name);
        self.rebuild_output_combos();
        Dirty::OUTPUTS
    }

    pub fn set_accounts(&mut self, accounts: Vec<AccountInfo>) -> Dirty {
        if self.accounts == accounts {
            return Dirty::NONE;
        }
        self.accounts = accounts;
        Dirty::ACCOUNTS
    }

    pub fn set_power(&mut self, power: Power) -> Dirty {
        if self.power == power {
            return Dirty::NONE;
        }
        self.power = power;
        Dirty::POWER
    }

    /// Mark the state as disconnected. The rest is kept, so the Remote keeps showing the media
    /// info until the device sends new values after the reconnect.
    pub const fn disconnected(&mut self) -> Dirty {
        self.connected = false;
        self.mrp = false;
        self.siri = false;
        Dirty::ALL
    }

    /// Current playback position in seconds.
    pub fn position(&self, now: f64) -> Option<f64> {
        let elapsed = self.elapsed?;
        let Some(ts) = self.elapsed_timestamp else {
            return Some(elapsed);
        };
        let rate = f64::from(self.rate.unwrap_or(0.0));
        let pos = elapsed + (now - ts).max(0.0) * rate;
        Some(self.duration.map_or(pos, |d| pos.min(d)).max(0.0))
    }

    /// Index of the chapter that contains the position.
    pub fn current_chapter(&self, now: f64) -> Option<usize> {
        let pos = self.position(now)?;
        self.chapters
            .iter()
            .rposition(|c| c.start.is_some_and(|s| s <= pos + 0.5))
    }

    /// Output uids of the group besides the Apple TV.
    pub fn grouped_uids(&self) -> impl Iterator<Item = &str> {
        let own = self.own_output_uid.as_deref();
        self.outputs
            .iter()
            .map(|o| o.uid.as_str())
            .filter(move |uid| Some(*uid) != own)
    }

    /// Index of the current output combination.
    pub fn current_output_combo(&self) -> Option<usize> {
        let mut grouped: Vec<&str> = self.grouped_uids().collect();
        grouped.sort_unstable();
        self.output_combos.iter().position(|c| {
            c.uids.len() == grouped.len()
                && c.uids
                    .iter()
                    .all(|u| grouped.binary_search(&u.as_str()).is_ok())
        })
    }

    /// Volume in percent. With `global_volume` it is the average of all outputs.
    pub fn volume_percent(&self) -> Option<u8> {
        let level = if self.global_volume {
            let (sum, count) = self
                .outputs
                .iter()
                .filter_map(|o| o.volume)
                .fold((0.0_f32, 0_u32), |(s, c), v| (s + v, c + 1));
            if count > 0 {
                Some(sum / count as f32)
            } else {
                self.volume_level
            }
        } else {
            self.volume_level
        }?;
        Some((level.clamp(0.0, 1.0) * 100.0).round() as u8)
    }

    /// Key of the playing item, from title, artist and album.
    pub fn item_key(&self) -> u64 {
        let mut h = DefaultHasher::new();
        (&self.title, &self.artist, &self.album).hash(&mut h);
        h.finish()
    }

    /// True if the item without an image may keep the image of the item before. Some apps
    /// (for example Telia) send the channel logo with a short first item, then the program
    /// without an image.
    pub fn keep_artwork(&self, item: u32) -> bool {
        const SHORT_ITEM: Duration = Duration::from_secs(2);
        !self.artwork_kept
            && self.artwork_item == item.wrapping_sub(1)
            && self.prev_item_lasted.is_some_and(|d| d < SHORT_ITEM)
    }

    /// Build the output combinations: the Apple TV alone, then the Apple TV with 1 to 4 devices.
    /// Names include the Apple TV, because it always stays in the group.
    fn rebuild_output_combos(&mut self) {
        const MAX_COMBOS: usize = 64;
        const MAX_GROUP: usize = 4;

        // Candidates: devices on the network and devices in the group, without the Apple TV.
        let mut candidates: Vec<(&str, &str)> = self
            .airplay_devices
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();
        for o in &self.outputs {
            if Some(o.uid.as_str()) != self.own_output_uid.as_deref()
                && !candidates.iter().any(|(u, _)| *u == o.uid)
            {
                candidates.push((&o.uid, &o.name));
            }
        }
        candidates.sort_unstable_by(|a, b| a.1.cmp(b.1));

        let mut combos = vec![OutputCombo {
            name: self.name.clone(),
            uids: Vec::new(),
        }];
        let mut indexes = Vec::with_capacity(MAX_GROUP);
        'outer: for size in 1..=MAX_GROUP.min(candidates.len()) {
            indexes.clear();
            indexes.extend(0..size);
            loop {
                if combos.len() >= MAX_COMBOS {
                    break 'outer;
                }
                let mut uids: Vec<String> = indexes
                    .iter()
                    .map(|&i| candidates[i].0.to_string())
                    .collect();
                uids.sort_unstable();
                let mut name = self.name.clone();
                for &i in &indexes {
                    name.push_str(", ");
                    name.push_str(candidates[i].1);
                }
                combos.push(OutputCombo { name, uids });
                if !next_combination(&mut indexes, candidates.len()) {
                    break;
                }
            }
        }
        self.output_combos = combos;
    }
}

/// Move to the next combination of indexes in lexicographic order.
fn next_combination(indexes: &mut [usize], n: usize) -> bool {
    let k = indexes.len();
    let Some(i) = (0..k).rev().find(|&i| indexes[i] != i + n - k) else {
        return false;
    };
    indexes[i] += 1;
    for j in i + 1..k {
        indexes[j] = indexes[j - 1] + 1;
    }
    true
}

/// Artwork file per item key, the oldest first. `None` if the item had more than one image.
#[derive(Debug, Default)]
pub struct ItemArtwork(VecDeque<(u64, Option<Box<str>>)>);

impl ItemArtwork {
    const KEEP: usize = 32;

    pub fn remember(&mut self, item_key: u64, name: Box<str>) {
        // A title used by many items (for example "No program information") has more than one image.
        let name = match self.0.iter().position(|(k, _)| *k == item_key) {
            Some(i) => self
                .0
                .remove(i)
                .and_then(|(_, old)| old)
                .filter(|old| *old == name),
            None => Some(name),
        };
        if self.0.len() >= Self::KEEP {
            self.0.pop_front();
        }
        self.0.push_back((item_key, name));
    }

    /// File seen before for the item.
    pub fn get(&self, item_key: u64) -> Option<&str> {
        self.0.iter().find(|(k, _)| *k == item_key)?.1.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(uid: &str, name: &str, volume: Option<f32>) -> OutputDevice {
        let mut o = OutputDevice::default();
        o.uid = uid.into();
        o.name = name.into();
        o.volume = volume;
        o
    }

    #[test]
    fn now_playing_update_marks_only_changed_groups() {
        let mut s = Snapshot::default();
        let mut np = NowPlaying::default();
        np.title = Some("Song".into());
        // A new item without an artwork id: ask the Apple TV for the image.
        assert_eq!(Dirty::METADATA | Dirty::ARTWORK, s.update_now_playing(&np));
        assert_eq!(Dirty::NONE, s.update_now_playing(&np));

        np.state = Some(PlaybackState::Playing);
        np.elapsed = Some(10.0);
        let d = s.update_now_playing(&np);
        assert!(d.any(Dirty::PLAYBACK) && d.any(Dirty::POSITION));
        assert!(!d.any(Dirty::METADATA));
    }

    #[test]
    fn artwork_stays_until_a_new_one_is_there() {
        let mut s = Snapshot::default();
        let mut np = NowPlaying::default();
        np.title = Some("Channel".into());
        np.artwork_id = Some("a".into());
        s.update_now_playing(&np);
        s.artwork_url = Some("saved".into());

        let item = s.item_seq;
        np.artwork_id = None;
        assert!(s.update_now_playing(&np).any(Dirty::ARTWORK));
        assert!(s.artwork_fetch);
        assert_eq!(Some("saved"), s.artwork_url.as_deref());
        assert_eq!(item, s.item_seq);

        np.title = Some("Other channel".into());
        assert!(s.update_now_playing(&np).any(Dirty::ARTWORK));
        assert_ne!(item, s.item_seq);
    }

    #[test]
    fn artwork_again_when_the_state_comes() {
        let mut s = Snapshot::default();
        let mut np = NowPlaying::default();
        np.title = Some("Show".into());
        np.artwork_id = Some("a".into());
        s.update_now_playing(&np);
        np.state = Some(PlaybackState::Playing);
        assert!(s.update_now_playing(&np).any(Dirty::ARTWORK));
        np.state = Some(PlaybackState::Paused);
        assert!(!s.update_now_playing(&np).any(Dirty::ARTWORK));
    }

    #[test]
    fn companion_artwork_after_the_item() {
        let mut s = Snapshot::default();
        let mut np = NowPlaying::default();
        np.title = Some("Show".into());
        np.artwork_id = Some("a".into());
        s.update_now_playing(&np);
        assert!(s.artwork_fetch);

        let image = Bytes::from_static(b"\xff\xd8\xff");
        np.artwork_data = Some(image.clone());
        assert_eq!(Dirty::ARTWORK, s.update_now_playing(&np));
        // The same buffer again is no change.
        np.artwork_data = Some(image);
        assert_eq!(Dirty::NONE, s.update_now_playing(&np));
        // A lost image keeps the shown one.
        np.artwork_data = None;
        assert_eq!(Dirty::NONE, s.update_now_playing(&np));
        assert!(s.artwork_data.is_none());
    }

    #[test]
    fn item_artwork() {
        let mut art = ItemArtwork::default();
        art.remember(1, "a".into());
        art.remember(1, "a".into());
        assert_eq!(Some("a"), art.get(1));
        // One title with two images: not used.
        art.remember(2, "a".into());
        art.remember(2, "b".into());
        assert_eq!(None, art.get(2));
        art.remember(2, "b".into());
        assert_eq!(None, art.get(2));
        for k in 10..10 + ItemArtwork::KEEP as u64 {
            art.remember(k, "x".into());
        }
        assert_eq!(None, art.get(1));
    }

    #[test]
    fn short_item_passes_its_image_on_once() {
        let mut s = Snapshot {
            artwork_item: 1,
            prev_item_lasted: Some(Duration::from_secs(1)),
            ..Snapshot::default()
        };
        assert!(s.keep_artwork(2));
        assert!(!s.keep_artwork(3));
        s.artwork_kept = true;
        assert!(!s.keep_artwork(2));
        s.artwork_kept = false;
        s.prev_item_lasted = Some(Duration::from_secs(60));
        assert!(!s.keep_artwork(2));
    }

    #[test]
    fn position_moves_with_rate() {
        let s = Snapshot {
            elapsed: Some(10.0),
            elapsed_timestamp: Some(100.0),
            rate: Some(1.0),
            duration: Some(20.0),
            ..Snapshot::default()
        };
        assert_eq!(Some(15.0), s.position(105.0));
        assert_eq!(Some(20.0), s.position(200.0));

        let paused = Snapshot {
            rate: Some(0.0),
            ..s
        };
        assert_eq!(Some(10.0), paused.position(105.0));
    }

    #[test]
    fn current_chapter_from_position() {
        let mut c1 = Chapter::default();
        c1.start = Some(0.0);
        let mut c2 = Chapter::default();
        c2.start = Some(60.0);
        let s = Snapshot {
            elapsed: Some(75.0),
            chapters: vec![c1, c2],
            ..Snapshot::default()
        };
        assert_eq!(Some(1), s.current_chapter(0.0));
    }

    #[test]
    fn output_combinations() {
        let mut s = Snapshot {
            name: "Living Room".into(),
            own_output_uid: Some("ATV".into()),
            ..Snapshot::default()
        };
        s.set_airplay_devices(vec![
            ("B".into(), "Bedroom".into()),
            ("K".into(), "Kitchen".into()),
        ]);
        let names: Vec<_> = s.output_combos.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            vec![
                "Living Room",
                "Living Room, Bedroom",
                "Living Room, Kitchen",
                "Living Room, Bedroom, Kitchen"
            ],
            names
        );

        s.update_volume(&{
            let mut v = Volume::default();
            v.outputs = vec![
                output("ATV", "Living Room", Some(0.5)),
                output("K", "Kitchen", Some(0.3)),
            ];
            v
        });
        assert_eq!(Some(2), s.current_output_combo());
    }

    #[test]
    fn rename_updates_output_names() {
        let mut s = Snapshot {
            name: "Old".into(),
            ..Snapshot::default()
        };
        s.set_airplay_devices(vec![("K".into(), "Kitchen".into())]);
        assert_eq!(Dirty::OUTPUTS, s.set_name("New"));
        assert_eq!(Dirty::NONE, s.set_name("New"));
        let names: Vec<_> = s.output_combos.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(vec!["New", "New, Kitchen"], names);
    }

    #[test]
    fn apps_sorted_case_insensitive() {
        let app = |name: &str| AppInfo {
            bundle_id: name.into(),
            name: name.into(),
        };
        let mut s = Snapshot::default();
        s.set_apps(vec![app("b"), app("C"), app("a")]);
        let names: Vec<_> = s.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(vec!["a", "b", "C"], names);
    }

    #[test]
    fn output_combinations_are_capped() {
        let mut s = Snapshot::default();
        let devices = (0..10)
            .map(|i| (format!("U{i}"), format!("D{i}")))
            .collect();
        s.set_airplay_devices(devices);
        assert_eq!(64, s.output_combos.len());
    }

    #[test]
    fn global_volume_is_average() {
        let mut s = Snapshot {
            global_volume: true,
            ..Snapshot::default()
        };
        let mut v = Volume::default();
        v.level = Some(0.2);
        v.outputs = vec![output("A", "A", Some(0.4)), output("B", "B", Some(0.6))];
        s.update_volume(&v);
        assert_eq!(Some(50), s.volume_percent());
        s.global_volume = false;
        assert_eq!(Some(20), s.volume_percent());
    }

    #[test]
    fn output_volume_keeps_the_combinations() {
        let mut s = Snapshot::default();
        let mut v = Volume::default();
        v.outputs = vec![output("A", "A", Some(0.4)), output("B", "B", Some(0.6))];
        assert!(s.update_volume(&v).any(Dirty::OUTPUTS));
        v.outputs[1].volume = Some(0.7);
        let d = s.update_volume(&v);
        assert!(d.any(Dirty::VOLUME) && !d.any(Dirty::OUTPUTS));
        assert_eq!(Some(0.7), s.outputs[1].volume);
    }
}
