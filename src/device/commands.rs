// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Turn entity actions into Apple TV calls.

use crate::device::browse::{APP_PREFIX, QUEUE_PREFIX};
use crate::device::state::{Power, Snapshot};
use crate::entities::media_player::{Action, SelectOp, Swipe};
use crate::entities::{SelectKind, select};
use crate::errors::ServiceError;
use ratv::mrp::{Command, LanguageOption, PlaybackState, RepeatMode, ShuffleMode};
use ratv::{AppleTv, Key};
use std::time::Duration;

/// Skip interval in seconds if the app does not report one.
const DEFAULT_SKIP: f64 = 10.0;
/// Time for a swipe.
const SWIPE_TIME: Duration = Duration::from_millis(200);

/// Active fast forward or rewind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seek {
    FastForward,
    Rewind,
}

/// One call to the Apple TV.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    TurnOn,
    TurnOff,
    Press(Key),
    Hold(Key, Duration),
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    BeginFastForward,
    EndFastForward,
    BeginRewind,
    EndRewind,
    Seek(f64),
    Skip(f64),
    AppSwitcher,
    ContextMenu,
    ControlCenter,
    /// Level from 0.0 to 1.0, and other outputs that get the same level.
    Volume(f32, Vec<String>),
    Mute(bool),
    Repeat(RepeatMode),
    Shuffle(ShuffleMode),
    Swipe(Swipe),
    LaunchApp(String),
    /// All output uids of the new group, with the Apple TV.
    SetOutputs(Vec<String>),
    PlayQueueItem(usize),
    EnableLanguage(LanguageOption),
    DisableSubtitles,
    NextChapter,
    PreviousChapter,
    Like,
    Dislike,
    /// Account id and name.
    SwitchAccount(String, String),
    PlaybackRate(f32),
    Sleep(Duration),
}

/// State that changes how actions are run.
pub struct Planner<'a> {
    pub s: &'a Snapshot,
    /// Changed by the plan. Use it only if the plan succeeds.
    pub seek: &'a mut Option<Seek>,
    /// Power and mute for toggles: the last set values until the device reports them.
    pub power: Power,
    pub muted: Option<bool>,
    pub screensaver: bool,
}

impl Planner<'_> {
    /// Add the calls of an action.
    #[allow(clippy::too_many_lines)]
    pub fn plan(&mut self, action: &Action, out: &mut Vec<Call>) -> Result<(), ServiceError> {
        if action.needs_power() && self.power == Power::Off {
            out.push(Call::TurnOn);
            self.power = Power::On;
        }
        self.end_seek(action, out);
        let s = self.s;
        let call = match action {
            Action::On => Call::TurnOn,
            Action::Off => Call::TurnOff,
            Action::Toggle => {
                if self.power == Power::On {
                    Call::TurnOff
                } else {
                    Call::TurnOn
                }
            }
            Action::Press(k) => Call::Press(*k),
            Action::Hold(k, d) => Call::Hold(*k, *d),
            Action::PlayPause if self.screensaver => {
                // Wake from the screensaver first. The Python driver does the same.
                out.push(Call::Press(Key::Menu));
                if matches!(s.state, Some(PlaybackState::Paused)) {
                    out.push(Call::Press(Key::Select));
                }
                return Ok(());
            }
            Action::PlayPause => Call::PlayPause,
            Action::PlayPauseKey => Call::Press(Key::PlayPause),
            Action::Play => Call::Play,
            Action::Pause => Call::Pause,
            Action::Stop => Call::Stop,
            Action::Next => Call::Next,
            Action::Previous => Call::Previous,
            Action::FastForwardBegin => {
                *self.seek = Some(Seek::FastForward);
                Call::BeginFastForward
            }
            Action::RewindBegin => {
                *self.seek = Some(Seek::Rewind);
                Call::BeginRewind
            }
            Action::FastForwardEnd => Call::EndFastForward,
            Action::RewindEnd => Call::EndRewind,
            Action::Seek(p) => Call::Seek(*p),
            Action::Skip(forward) => {
                // ratv seeks instead if the app has no skip command.
                if *forward {
                    Call::Skip(s.skip_forward_interval.unwrap_or(DEFAULT_SKIP))
                } else {
                    Call::Skip(-s.skip_backward_interval.unwrap_or(DEFAULT_SKIP).abs())
                }
            }
            Action::Volume(v) => {
                let others = if s.global_volume {
                    s.grouped_uids().map(str::to_string).collect()
                } else {
                    Vec::new()
                };
                Call::Volume(f32::from(*v) / 100.0, others)
            }
            Action::VolumeUp => Call::Press(Key::VolumeUp),
            Action::VolumeDown => Call::Press(Key::VolumeDown),
            Action::MuteToggle => Call::Mute(self.muted != Some(true)),
            Action::Mute(m) => Call::Mute(*m),
            Action::Repeat(m) => Call::Repeat(*m),
            Action::Shuffle(m) => Call::Shuffle(*m),
            Action::Swipe(d) => Call::Swipe(*d),
            Action::AppSwitcher => Call::AppSwitcher,
            Action::ContextMenu => Call::ContextMenu,
            Action::ControlCenter => Call::ControlCenter,
            Action::SelectSource(name) => {
                let app =
                    s.apps.iter().find(|a| a.name == *name).ok_or_else(|| {
                        ServiceError::BadRequest(format!("Unknown source: {name}"))
                    })?;
                Call::LaunchApp(app.bundle_id.clone())
            }
            Action::SelectSoundMode(name) => {
                let i = s
                    .output_combos
                    .iter()
                    .position(|c| c.name == *name)
                    .ok_or_else(|| {
                        ServiceError::BadRequest(format!("Unknown sound mode: {name}"))
                    })?;
                self.outputs(i)?
            }
            Action::PlayMedia(id) => play_media(s, id)?,
            Action::CycleAudioTrack => {
                let next = next_language(&s.audio_languages, s.audio_language.as_ref())
                    .ok_or_else(|| ServiceError::BadRequest("No audio tracks".into()))?;
                Call::EnableLanguage(next.clone())
            }
            Action::CycleSubtitle => {
                if s.subtitle_languages.is_empty() {
                    return Err(ServiceError::BadRequest("No subtitles".into()));
                }
                // Off, then each subtitle, then off again. Shown as off like the select.
                let off = s.captions_enabled == Some(false) && !s.subtitles_auto;
                let current = s
                    .subtitle_language
                    .as_ref()
                    .filter(|_| !off)
                    .and_then(|c| language_index(&s.subtitle_languages, c));
                match current {
                    Some(i) if i + 1 >= s.subtitle_languages.len() => Call::DisableSubtitles,
                    Some(i) => Call::EnableLanguage(s.subtitle_languages[i + 1].clone()),
                    None => Call::EnableLanguage(s.subtitle_languages[0].clone()),
                }
            }
            Action::NextChapter => Call::NextChapter,
            Action::PreviousChapter => Call::PreviousChapter,
            Action::Like => Call::Like,
            Action::Dislike => Call::Dislike,
            Action::Select(kind, op) => self.select(*kind, op)?,
        };
        check_support(s, &call)?;
        // Later actions of the same command see the new power state.
        match call {
            Call::TurnOn => self.power = Power::On,
            Call::TurnOff => self.power = Power::Off,
            _ => {}
        }
        out.push(call);
        Ok(())
    }

    /// End an active fast forward or rewind before other playback actions.
    fn end_seek(&mut self, action: &Action, out: &mut Vec<Call>) {
        let begin = match action {
            Action::FastForwardBegin => Some(Seek::FastForward),
            Action::RewindBegin => Some(Seek::Rewind),
            _ => None,
        };
        if begin.is_none() && !action.ends_seek() {
            return;
        }
        let active = self.seek.take();
        // The end actions send their own call. Also end the other one if it is active.
        let own = match action {
            Action::FastForwardEnd => Some(Seek::FastForward),
            Action::RewindEnd => Some(Seek::Rewind),
            _ => None,
        };
        if own.is_some() {
            match active {
                Some(seek) if Some(seek) != own => out.push(match seek {
                    Seek::FastForward => Call::EndFastForward,
                    Seek::Rewind => Call::EndRewind,
                }),
                _ => {}
            }
            return;
        }
        match active {
            Some(seek) if Some(seek) != begin => out.push(match seek {
                Seek::FastForward => Call::EndFastForward,
                Seek::Rewind => Call::EndRewind,
            }),
            _ => {}
        }
    }

    fn select(&self, kind: SelectKind, op: &SelectOp) -> Result<Call, ServiceError> {
        let s = self.s;
        let now = super::now();
        let options = select::options(s, kind);
        let current = select::current(s, kind, now);
        let i = select::resolve(op, &options, current.as_deref())?;
        Ok(match kind {
            SelectKind::App => Call::LaunchApp(s.apps[i].bundle_id.clone()),
            SelectKind::AudioOutput => self.outputs(i)?,
            SelectKind::AudioLanguage => Call::EnableLanguage(s.audio_languages[i].clone()),
            SelectKind::Subtitles if i == 0 => Call::DisableSubtitles,
            SelectKind::Subtitles => Call::EnableLanguage(s.subtitle_languages[i - 1].clone()),
            SelectKind::Account => {
                let a = &s.accounts[i];
                Call::SwitchAccount(a.id.clone(), a.name.clone())
            }
            SelectKind::Chapter => {
                let start = s.chapters[i]
                    .start
                    .ok_or_else(|| ServiceError::BadRequest("Chapter has no start".into()))?;
                Call::Seek(start)
            }
            SelectKind::UpNext => Call::PlayQueueItem(i),
            SelectKind::PlaybackSpeed => Call::PlaybackRate(s.supported_rates[i]),
            SelectKind::Shuffle => Call::Shuffle(match i {
                1 => ShuffleMode::Songs,
                2 => ShuffleMode::Albums,
                _ => ShuffleMode::Off,
            }),
            SelectKind::Repeat => Call::Repeat(match i {
                1 => RepeatMode::All,
                2 => RepeatMode::One,
                _ => RepeatMode::Off,
            }),
        })
    }

    /// Call to switch to an output combination.
    fn outputs(&self, combo: usize) -> Result<Call, ServiceError> {
        let own = self.s.own_output_uid.as_ref().ok_or_else(|| {
            ServiceError::ServiceUnavailable("Output id of the Apple TV is not known".into())
        })?;
        let combo = &self.s.output_combos[combo];
        let mut uids = Vec::with_capacity(combo.uids.len() + 1);
        uids.push(own.clone());
        uids.extend(combo.uids.iter().cloned());
        Ok(Call::SetOutputs(uids))
    }
}

/// Fail fast for calls the app does not support. The Apple TV does not answer them, so they
/// would only end with a timeout.
fn check_support(s: &Snapshot, call: &Call) -> Result<(), ServiceError> {
    let (command, name) = match call {
        Call::Repeat(_) => (Command::ChangeRepeatMode, "repeat"),
        Call::Shuffle(_) => (Command::ChangeShuffleMode, "shuffle"),
        Call::PlaybackRate(_) => (Command::ChangePlaybackRate, "playback speed"),
        _ => return Ok(()),
    };
    if s.commands.contains(&command) {
        return Ok(());
    }
    Err(ServiceError::NotSupported(if s.commands.is_empty() {
        format!("Nothing is playing: {name} is not available")
    } else {
        format!("The app does not support {name}")
    }))
}

/// Play an app or an Up Next item.
fn play_media(s: &Snapshot, id: &str) -> Result<Call, ServiceError> {
    if let Some(queue_id) = id.strip_prefix(QUEUE_PREFIX) {
        // Only the index works: the device rejects content ids.
        let index = s
            .up_next
            .iter()
            .position(|q| q.id.as_deref() == Some(queue_id))
            .ok_or_else(|| ServiceError::NotFound(format!("Item not in Up Next: {queue_id}")))?;
        return Ok(Call::PlayQueueItem(index));
    }
    let bundle_id = id.strip_prefix(APP_PREFIX).unwrap_or(id);
    if bundle_id.is_empty() {
        return Err(ServiceError::BadRequest("Empty media_id".into()));
    }
    Ok(Call::LaunchApp(bundle_id.to_string()))
}

fn language_index(options: &[LanguageOption], current: &LanguageOption) -> Option<usize> {
    options
        .iter()
        .position(|o| o.identifier.is_some() && o.identifier == current.identifier)
        .or_else(|| options.iter().position(|o| o == current))
}

fn next_language<'a>(
    options: &'a [LanguageOption],
    current: Option<&LanguageOption>,
) -> Option<&'a LanguageOption> {
    let next = current
        .and_then(|c| language_index(options, c))
        .map_or(0, |i| (i + 1) % options.len().max(1));
    options.get(next)
}

/// Run the calls in order. Stops at the first error.
pub async fn exec_all(tv: &AppleTv, calls: Vec<Call>) -> Result<(), ratv::Error> {
    for call in calls {
        exec(tv, call).await?;
    }
    Ok(())
}

/// Run one call.
pub async fn exec(tv: &AppleTv, call: Call) -> Result<(), ratv::Error> {
    match call {
        Call::TurnOn => tv.turn_on().await,
        Call::TurnOff => tv.turn_off().await,
        Call::Press(k) => tv.press(k).await,
        Call::Hold(k, d) => tv.hold(k, d).await,
        Call::PlayPause => tv.play_pause().await,
        Call::Play => tv.play().await,
        Call::Pause => tv.pause().await,
        Call::Stop => tv.stop().await,
        Call::Next => tv.next().await,
        Call::Previous => tv.previous().await,
        Call::BeginFastForward => tv.begin_fast_forward().await,
        Call::EndFastForward => tv.end_fast_forward().await,
        Call::BeginRewind => tv.begin_rewind().await,
        Call::EndRewind => tv.end_rewind().await,
        Call::Seek(p) => tv.seek(p).await,
        Call::Skip(sec) => tv.skip(sec).await,
        Call::AppSwitcher => tv.app_switcher().await,
        Call::ContextMenu => tv.context_menu().await,
        Call::ControlCenter => tv.control_center().await,
        Call::Volume(level, others) => {
            tv.set_volume(level).await?;
            if let Some(mrp) = tv.mrp() {
                for uid in &others {
                    mrp.set_output_volume(uid, level).await?;
                }
            }
            Ok(())
        }
        Call::Mute(m) => tv.set_muted(m).await,
        Call::Repeat(m) => tv.set_repeat(m).await,
        Call::Shuffle(m) => tv.set_shuffle(m).await,
        Call::Swipe(d) => {
            let (from, to) = match d {
                Swipe::Left => ((0.8, 0.5), (0.2, 0.5)),
                Swipe::Right => ((0.2, 0.5), (0.8, 0.5)),
                Swipe::Up => ((0.5, 0.8), (0.5, 0.2)),
                Swipe::Down => ((0.5, 0.2), (0.5, 0.8)),
            };
            tv.swipe(from, to, SWIPE_TIME).await
        }
        Call::LaunchApp(id) => tv.launch_app(&id).await,
        Call::SetOutputs(uids) => {
            let mrp = tv.mrp().ok_or(ratv::Error::NotConnected("MRP"))?;
            let uids: Vec<&str> = uids.iter().map(String::as_str).collect();
            mrp.set_outputs(&uids).await
        }
        Call::PlayQueueItem(i) => tv.play_queue_item(i).await,
        Call::EnableLanguage(o) => tv.enable_language(&o).await,
        Call::DisableSubtitles => tv.disable_subtitles().await,
        Call::NextChapter => tv.next_chapter().await,
        Call::PreviousChapter => tv.previous_chapter().await,
        Call::Like => tv.set_liked(true).await,
        Call::Dislike => tv.set_disliked(true).await,
        Call::SwitchAccount(id, _) => tv.switch_account(&id).await,
        Call::PlaybackRate(r) => tv.set_playback_rate(r).await,
        Call::Sleep(d) => {
            tokio::time::sleep(d).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::state::AppInfo;

    fn plan(s: &Snapshot, seek: &mut Option<Seek>, actions: &[Action]) -> Vec<Call> {
        let mut p = Planner {
            s,
            seek,
            power: s.power,
            muted: s.muted,
            screensaver: false,
        };
        let mut out = Vec::new();
        for a in actions {
            p.plan(a, &mut out).expect("plan");
        }
        out
    }

    #[test]
    fn power_on_before_commands() {
        let s = Snapshot {
            power: Power::Off,
            ..Snapshot::default()
        };
        let calls = plan(
            &s,
            &mut None,
            &[Action::Press(Key::Up), Action::Press(Key::Down)],
        );
        assert_eq!(
            vec![Call::TurnOn, Call::Press(Key::Up), Call::Press(Key::Down)],
            calls
        );
        assert_eq!(vec![Call::TurnOn], plan(&s, &mut None, &[Action::Toggle]));
        assert_eq!(
            vec![Call::TurnOn, Call::Press(Key::Up)],
            plan(&s, &mut None, &[Action::On, Action::Press(Key::Up)])
        );
        assert_eq!(
            vec![Call::TurnOn, Call::TurnOff],
            plan(&s, &mut None, &[Action::Toggle, Action::Toggle])
        );
    }

    #[test]
    fn seek_is_ended_before_playback() {
        let s = Snapshot::default();
        let mut seek = None;
        assert_eq!(
            vec![Call::BeginFastForward],
            plan(&s, &mut seek, &[Action::FastForwardBegin])
        );
        assert_eq!(Some(Seek::FastForward), seek);
        assert_eq!(
            vec![Call::EndFastForward, Call::BeginRewind],
            plan(&s, &mut seek, &[Action::RewindBegin])
        );
        assert_eq!(
            vec![Call::EndRewind, Call::Pause],
            plan(&s, &mut seek, &[Action::Pause])
        );
        assert_eq!(None, seek);
        assert_eq!(
            vec![Call::EndRewind],
            plan(&s, &mut seek, &[Action::RewindEnd])
        );
        // The wrong end button still ends the active seek.
        plan(&s, &mut seek, &[Action::FastForwardBegin]);
        assert_eq!(
            vec![Call::EndFastForward, Call::EndRewind],
            plan(&s, &mut seek, &[Action::RewindEnd])
        );
        assert_eq!(None, seek);
    }

    #[test]
    fn screensaver_play_pause() {
        let s = Snapshot {
            state: Some(PlaybackState::Paused),
            ..Snapshot::default()
        };
        let mut seek = None;
        let mut p = Planner {
            s: &s,
            seek: &mut seek,
            power: s.power,
            muted: None,
            screensaver: true,
        };
        let mut out = Vec::new();
        p.plan(&Action::PlayPause, &mut out).expect("plan");
        assert_eq!(vec![Call::Press(Key::Menu), Call::Press(Key::Select)], out);
    }

    #[test]
    fn play_media_ids() {
        let mut item = ratv::mrp::QueueItem::default();
        item.id = Some("42".into());
        let s = Snapshot {
            up_next: vec![ratv::mrp::QueueItem::default(), item],
            apps: vec![AppInfo {
                bundle_id: "com.apple.TVMusic".into(),
                name: "Music".into(),
            }],
            ..Snapshot::default()
        };
        assert_eq!(
            Call::PlayQueueItem(1),
            play_media(&s, "queue:42").expect("ok")
        );
        assert!(play_media(&s, "queue:7").is_err());
        assert_eq!(
            Call::LaunchApp("com.apple.TVMusic".into()),
            play_media(&s, "app:com.apple.TVMusic").expect("ok")
        );
        assert_eq!(
            Call::LaunchApp("com.apple.TVMusic".into()),
            play_media(&s, "com.apple.TVMusic").expect("ok")
        );
    }

    #[test]
    fn unsupported_commands_fail_fast() {
        let mut s = Snapshot::default();
        let mut p = Planner {
            s: &s,
            seek: &mut None,
            power: Power::Unknown,
            muted: None,
            screensaver: false,
        };
        let mut out = Vec::new();
        assert!(matches!(
            p.plan(&Action::Repeat(RepeatMode::All), &mut out),
            Err(ServiceError::NotSupported(_))
        ));
        s.commands = vec![Command::ChangeRepeatMode];
        assert_eq!(
            vec![Call::Repeat(RepeatMode::All)],
            plan(&s, &mut None, &[Action::Repeat(RepeatMode::All)])
        );
        let mut p = Planner {
            s: &s,
            seek: &mut None,
            power: Power::Unknown,
            muted: None,
            screensaver: false,
        };
        assert!(
            p.plan(&Action::Shuffle(ShuffleMode::Songs), &mut out)
                .is_err()
        );
    }

    #[test]
    fn skip_uses_app_interval() {
        let mut s = Snapshot::default();
        assert_eq!(
            vec![Call::Skip(10.0), Call::Skip(-10.0)],
            plan(&s, &mut None, &[Action::Skip(true), Action::Skip(false)])
        );
        s.skip_forward_interval = Some(30.0);
        s.skip_backward_interval = Some(15.0);
        assert_eq!(
            vec![Call::Skip(30.0), Call::Skip(-15.0)],
            plan(&s, &mut None, &[Action::Skip(true), Action::Skip(false)])
        );
    }

    #[test]
    fn global_volume_sets_other_outputs() {
        let mut o = ratv::mrp::OutputDevice::default();
        o.uid = "K".into();
        let s = Snapshot {
            global_volume: true,
            own_output_uid: Some("ATV".into()),
            outputs: vec![o],
            ..Snapshot::default()
        };
        assert_eq!(
            vec![Call::Volume(0.5, vec!["K".into()])],
            plan(&s, &mut None, &[Action::Volume(50)])
        );
    }
}
