// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Select entities: option lists, current option and command mapping.

use crate::device::state::{Dirty, Snapshot};
use crate::entities::SelectKind;
use crate::entities::media_player::{SelectOp, current_app};
use crate::errors::ServiceError;
use ratv::mrp::{LanguageOption, QueueItem, RepeatMode, ShuffleMode};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::str::FromStr;
use uc_api::SelectCommand;

/// Option name for no subtitles.
pub const SUBTITLES_OFF: &str = "Off";
/// Option name for subtitles set to Auto.
pub const SUBTITLES_AUTO: &str = "Auto";
pub const SHUFFLE_OPTIONS: [&str; 3] = ["Off", "Songs", "Albums"];
pub const REPEAT_OPTIONS: [&str; 3] = ["Off", "All", "One"];

/// Groups that change the attributes of a select.
pub const fn dirty_groups(kind: SelectKind) -> Dirty {
    match kind {
        SelectKind::App => Dirty::APPS.or(Dirty::APP),
        SelectKind::AudioLanguage | SelectKind::Subtitles => Dirty::LANGUAGES.or(Dirty::LANGUAGE),
        SelectKind::Chapter => Dirty::CHAPTERS.or(Dirty::CHAPTER),
        SelectKind::PlaybackSpeed => Dirty::RATES.or(Dirty::RATE),
        _ => list_groups(kind),
    }
}

/// Groups that change the options of a select. The others only change the current option.
const fn list_groups(kind: SelectKind) -> Dirty {
    match kind {
        SelectKind::App => Dirty::APPS,
        SelectKind::AudioOutput => Dirty::OUTPUTS,
        SelectKind::AudioLanguage | SelectKind::Subtitles => Dirty::LANGUAGES,
        SelectKind::Account => Dirty::ACCOUNTS,
        SelectKind::Chapter => Dirty::CHAPTERS,
        SelectKind::UpNext => Dirty::QUEUE,
        SelectKind::PlaybackSpeed => Dirty::RATES,
        SelectKind::Shuffle | SelectKind::Repeat => Dirty::SHUFFLE_REPEAT,
    }
}

/// Insert the attributes of a select if one of its groups is dirty.
pub fn attributes(
    s: &Snapshot,
    kind: SelectKind,
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
    if !dirty.any(dirty_groups(kind) | Dirty::CONNECTION) {
        return;
    }
    // Only the current option changed. The list is not built.
    if !dirty.any(list_groups(kind) | Dirty::CONNECTION) {
        out.insert(
            "current_option".into(),
            current(s, kind, now).unwrap_or_default().into(),
        );
        return;
    }
    let options = options(s, kind);
    let current = match kind {
        SelectKind::Chapter => s.current_chapter(now).and_then(|i| options.get(i).cloned()),
        _ => current(s, kind, now),
    };
    out.insert("current_option".into(), current.unwrap_or_default().into());
    out.insert("options".into(), json!(options));
}

/// Option names of a select.
pub fn options(s: &Snapshot, kind: SelectKind) -> Vec<Cow<'_, str>> {
    match kind {
        SelectKind::App => s
            .apps
            .iter()
            .map(|a| Cow::Borrowed(a.name.as_str()))
            .collect(),
        SelectKind::AudioOutput => s
            .output_combos
            .iter()
            .map(|c| Cow::Borrowed(c.name.as_str()))
            .collect(),
        SelectKind::AudioLanguage => language_names(&s.audio_languages),
        SelectKind::Subtitles => subtitle_names(s),
        SelectKind::Account => s
            .accounts
            .iter()
            .map(|a| Cow::Borrowed(a.name.as_str()))
            .collect(),
        SelectKind::Chapter => unique(chapter_names(s)),
        SelectKind::UpNext => unique(s.up_next.iter().map(queue_item_name)),
        SelectKind::PlaybackSpeed => s
            .supported_rates
            .iter()
            .map(|r| Cow::Owned(rate_name(*r)))
            .collect(),
        SelectKind::Shuffle => SHUFFLE_OPTIONS.iter().map(|o| Cow::Borrowed(*o)).collect(),
        SelectKind::Repeat => REPEAT_OPTIONS.iter().map(|o| Cow::Borrowed(*o)).collect(),
    }
}

/// Current option of a select.
pub fn current(s: &Snapshot, kind: SelectKind, now: f64) -> Option<Cow<'_, str>> {
    match kind {
        SelectKind::App => current_app(s).map(Cow::Borrowed),
        SelectKind::AudioOutput => s
            .current_output_combo()
            .map(|i| Cow::Borrowed(s.output_combos[i].name.as_str())),
        SelectKind::AudioLanguage => {
            current_language(&s.audio_languages, s.audio_language.as_ref())
        }
        SelectKind::Subtitles => {
            if s.subtitle_languages.is_empty() {
                None
            } else if s.subtitles_auto {
                Some(Cow::Borrowed(SUBTITLES_AUTO))
            } else if s.captions_enabled == Some(false) {
                // Companion says that no subtitles are shown.
                Some(Cow::Borrowed(SUBTITLES_OFF))
            } else {
                current_language(&s.subtitle_languages, s.subtitle_language.as_ref())
                    .or(Some(Cow::Borrowed(SUBTITLES_OFF)))
            }
        }
        SelectKind::Account => s.current_account.as_deref().map(Cow::Borrowed),
        SelectKind::Chapter => nth_unique(chapter_names(s), s.current_chapter(now)?),
        // Up Next has no current item.
        SelectKind::UpNext => None,
        SelectKind::PlaybackSpeed => s
            .rate
            .filter(|r| *r > 0.0)
            .map(|r| Cow::Owned(rate_name(r))),
        SelectKind::Shuffle => Some(Cow::Borrowed(match s.shuffle? {
            ShuffleMode::Songs => SHUFFLE_OPTIONS[1],
            ShuffleMode::Albums => SHUFFLE_OPTIONS[2],
            _ => SHUFFLE_OPTIONS[0],
        })),
        SelectKind::Repeat => Some(Cow::Borrowed(match s.repeat? {
            RepeatMode::All => REPEAT_OPTIONS[1],
            RepeatMode::One => REPEAT_OPTIONS[2],
            _ => REPEAT_OPTIONS[0],
        })),
    }
}

/// Map a select command.
pub fn command(
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> Result<SelectOp, ServiceError> {
    let cmd = SelectCommand::from_str(cmd_id)
        .map_err(|_| ServiceError::BadRequest(format!("Unknown command: {cmd_id}")))?;
    let cycle = || {
        params
            .and_then(|p| p.get("cycle"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    Ok(match cmd {
        SelectCommand::SelectOption => {
            let option = params
                .and_then(|p| p.get("option"))
                .and_then(Value::as_str)
                .ok_or_else(|| ServiceError::BadRequest("Missing parameter: option".into()))?;
            SelectOp::Option(option.into())
        }
        SelectCommand::SelectFirst => SelectOp::First,
        SelectCommand::SelectLast => SelectOp::Last,
        SelectCommand::SelectNext => SelectOp::Next { cycle: cycle() },
        SelectCommand::SelectPrevious => SelectOp::Previous { cycle: cycle() },
    })
}

/// Index of the option to select.
pub fn resolve(
    op: &SelectOp,
    options: &[Cow<'_, str>],
    current: Option<&str>,
) -> Result<usize, ServiceError> {
    let len = options.len();
    if len == 0 {
        return Err(ServiceError::BadRequest("No options available".into()));
    }
    let pos = current.and_then(|c| options.iter().position(|o| o == c));
    let index = match op {
        SelectOp::Option(name) => options.iter().position(|o| o == name),
        SelectOp::First => Some(0),
        SelectOp::Last => Some(len - 1),
        SelectOp::Next { cycle } => match pos {
            None => Some(0),
            Some(p) if p + 1 < len => Some(p + 1),
            Some(_) => cycle.then_some(0),
        },
        SelectOp::Previous { cycle } => match pos {
            None => Some(len - 1),
            Some(p) if p > 0 => Some(p - 1),
            Some(_) => cycle.then_some(len - 1),
        },
    };
    index.ok_or_else(|| ServiceError::BadRequest("Invalid option".into()))
}

/// Display names of language options. Names are made unique.
pub fn language_names(options: &[LanguageOption]) -> Vec<Cow<'_, str>> {
    unique(raw_language_names(options))
}

/// Language names before they are made unique.
fn raw_language_names(options: &[LanguageOption]) -> impl Iterator<Item = Cow<'_, str>> {
    options.iter().enumerate().map(|(i, o)| {
        o.display_name
            .as_deref()
            .or(o.language_tag.as_deref())
            .map_or_else(|| Cow::Owned(format!("Track {}", i + 1)), Cow::Borrowed)
    })
}

/// Chapter names before they are made unique.
fn chapter_names(s: &Snapshot) -> impl Iterator<Item = Cow<'_, str>> {
    s.chapters.iter().enumerate().map(|(i, c)| {
        c.title
            .as_deref()
            .map_or_else(|| format!("Chapter {}", i + 1).into(), Cow::Borrowed)
    })
}

/// "Off", then the subtitles. The Auto option is named "Auto" instead of its tag.
fn subtitle_names(s: &Snapshot) -> Vec<Cow<'_, str>> {
    let mut names = language_names(&s.subtitle_languages);
    if names.is_empty() {
        return names;
    }
    for (name, o) in names.iter_mut().zip(&s.subtitle_languages) {
        if o.is_auto() {
            *name = Cow::Borrowed(SUBTITLES_AUTO);
        }
    }
    names.insert(0, Cow::Borrowed(SUBTITLES_OFF));
    names
}

fn current_language<'a>(
    options: &'a [LanguageOption],
    current: Option<&LanguageOption>,
) -> Option<Cow<'a, str>> {
    let current = current?;
    let i = options
        .iter()
        .position(|o| o.identifier.is_some() && o.identifier == current.identifier)
        .or_else(|| options.iter().position(|o| o == current))?;
    nth_unique(raw_language_names(options), i)
}

fn queue_item_name(item: &QueueItem) -> Cow<'_, str> {
    match (item.title.as_deref(), item.artist.as_deref()) {
        (Some(t), Some(a)) => Cow::Owned(format!("{t} - {a}")),
        (Some(t), None) => Cow::Borrowed(t),
        (None, Some(a)) => Cow::Borrowed(a),
        (None, None) => Cow::Borrowed("Unknown"),
    }
}

/// Format a playback rate, for example `1x` or `1.5x`.
pub fn rate_name(rate: f32) -> String {
    let s = format!("{rate:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}x")
}

/// Add a number to repeated names: "English", "English (2)".
fn unique<'a>(names: impl Iterator<Item = Cow<'a, str>>) -> Vec<Cow<'a, str>> {
    let mut out: Vec<Cow<'a, str>> = Vec::new();
    for name in names {
        if out.contains(&name) {
            let mut n = 2;
            let unique = loop {
                let candidate = format!("{name} ({n})");
                if !out.iter().any(|o| *o == candidate) {
                    break candidate;
                }
                n += 1;
            };
            out.push(Cow::Owned(unique));
        } else {
            out.push(name);
        }
    }
    out
}

/// Name at index `i` of [`unique`]. Only the names up to `i` are built.
fn nth_unique<'a>(names: impl Iterator<Item = Cow<'a, str>>, i: usize) -> Option<Cow<'a, str>> {
    let mut names = unique(names.take(i.saturating_add(1)));
    if names.len() == i.saturating_add(1) {
        names.pop()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(names: &[&'static str]) -> Vec<Cow<'static, str>> {
        names.iter().map(|n| Cow::Borrowed(*n)).collect()
    }

    #[test]
    fn resolve_options() {
        let o = opts(&["a", "b", "c"]);
        assert_eq!(
            1,
            resolve(&SelectOp::Option("b".into()), &o, None).expect("ok")
        );
        assert!(resolve(&SelectOp::Option("x".into()), &o, None).is_err());
        assert_eq!(0, resolve(&SelectOp::First, &o, Some("b")).expect("ok"));
        assert_eq!(2, resolve(&SelectOp::Last, &o, Some("b")).expect("ok"));
        assert_eq!(
            2,
            resolve(&SelectOp::Next { cycle: true }, &o, Some("b")).expect("ok")
        );
        assert_eq!(
            0,
            resolve(&SelectOp::Next { cycle: true }, &o, Some("c")).expect("ok")
        );
        assert!(resolve(&SelectOp::Next { cycle: false }, &o, Some("c")).is_err());
        assert_eq!(
            2,
            resolve(&SelectOp::Previous { cycle: true }, &o, Some("a")).expect("ok")
        );
        assert!(resolve(&SelectOp::First, &[], None).is_err());
    }

    #[test]
    fn rate_names() {
        assert_eq!("1x", rate_name(1.0));
        assert_eq!("0.5x", rate_name(0.5));
        assert_eq!("1.25x", rate_name(1.25));
        assert_eq!("2x", rate_name(2.0));
    }

    #[test]
    fn names_are_unique() {
        let names = unique(
            ["English", "English", "German", "English"]
                .into_iter()
                .map(Cow::Borrowed),
        );
        assert_eq!(
            opts(&["English", "English (2)", "German", "English (3)"]),
            names
        );
    }

    #[test]
    fn nth_unique_name() {
        let names = ["English", "English", "German", "English"];
        let all = unique(names.into_iter().map(Cow::Borrowed));
        for (i, name) in all.iter().enumerate() {
            assert_eq!(
                Some(name),
                nth_unique(names.into_iter().map(Cow::Borrowed), i).as_ref()
            );
        }
        assert_eq!(None, nth_unique(names.into_iter().map(Cow::Borrowed), 4));
    }

    #[test]
    fn chapter_position_update_sends_current_only() {
        let chapter = |start: f64| {
            let mut c = ratv::mrp::Chapter::default();
            c.start = Some(start);
            c
        };
        let s = Snapshot {
            connected: true,
            elapsed: Some(75.0),
            chapters: vec![chapter(0.0), chapter(60.0)],
            ..Snapshot::default()
        };
        let mut out = Map::new();
        attributes(&s, SelectKind::Chapter, Dirty::POSITION, 0.0, &mut out);
        assert!(out.is_empty());

        let mut out = Map::new();
        attributes(&s, SelectKind::Chapter, Dirty::CHAPTER, 0.0, &mut out);
        assert_eq!(None, out.get("options"));
        assert_eq!(Some(&json!("Chapter 2")), out.get("current_option"));

        let mut out = Map::new();
        attributes(&s, SelectKind::Chapter, Dirty::CHAPTERS, 0.0, &mut out);
        assert_eq!(Some(&json!(["Chapter 1", "Chapter 2"])), out.get("options"));
        assert_eq!(Some(&json!("Chapter 2")), out.get("current_option"));
    }

    #[test]
    fn rate_change_sends_only_the_current_option() {
        let s = Snapshot {
            supported_rates: vec![1.0, 2.0],
            rate: Some(2.0),
            ..Snapshot::default()
        };
        let mut out = Map::new();
        attributes(&s, SelectKind::PlaybackSpeed, Dirty::RATE, 0.0, &mut out);
        assert_eq!(None, out.get("options"));
        assert!(out.contains_key("current_option"));

        let mut out = Map::new();
        attributes(&s, SelectKind::PlaybackSpeed, Dirty::RATES, 0.0, &mut out);
        assert!(out.contains_key("options"));
    }

    #[test]
    fn subtitles_have_off_option() {
        let en = LanguageOption {
            display_name: Some("English".into()),
            identifier: Some("en".into()),
            ..LanguageOption::default()
        };
        let s = Snapshot {
            subtitle_languages: vec![en],
            ..Snapshot::default()
        };
        assert_eq!(
            opts(&["Off", "English"]),
            options(&s, SelectKind::Subtitles)
        );
        assert_eq!(
            Some(Cow::Borrowed("Off")),
            current(&s, SelectKind::Subtitles, 0.0)
        );
    }

    #[test]
    fn captions_off_shows_off() {
        let en = LanguageOption {
            display_name: Some("English".into()),
            identifier: Some("en".into()),
            ..LanguageOption::default()
        };
        let mut s = Snapshot {
            subtitle_languages: vec![en.clone()],
            subtitle_language: Some(en),
            ..Snapshot::default()
        };
        assert_eq!(
            Some(Cow::Borrowed("English")),
            current(&s, SelectKind::Subtitles, 0.0)
        );
        s.captions_enabled = Some(false);
        assert_eq!(
            Some(Cow::Borrowed("Off")),
            current(&s, SelectKind::Subtitles, 0.0)
        );
    }

    #[test]
    fn subtitles_auto_is_named_auto() {
        let option = |tag: &str| LanguageOption {
            language_tag: Some(tag.into()),
            identifier: Some(tag.into()),
            ..LanguageOption::default()
        };
        let auto = option("__AUTO__");
        let s = Snapshot {
            subtitle_languages: vec![auto.clone(), option("en")],
            subtitle_language: Some(auto),
            subtitles_auto: true,
            ..Snapshot::default()
        };
        assert_eq!(
            opts(&["Off", "Auto", "en"]),
            options(&s, SelectKind::Subtitles)
        );
        assert_eq!(
            Some(Cow::Borrowed("Auto")),
            current(&s, SelectKind::Subtitles, 0.0)
        );

        // The Auto entry keeps its name when another subtitle is on.
        let s = Snapshot {
            subtitle_language: Some(option("en")),
            subtitles_auto: false,
            ..s
        };
        assert_eq!(
            opts(&["Off", "Auto", "en"]),
            options(&s, SelectKind::Subtitles)
        );
        assert_eq!(
            Some(Cow::Borrowed("en")),
            current(&s, SelectKind::Subtitles, 0.0)
        );
    }

    #[test]
    fn select_commands() {
        let p = json!({ "option": "b" }).as_object().cloned();
        assert_eq!(
            SelectOp::Option("b".into()),
            command("select_option", p.as_ref()).expect("ok")
        );
        let p = json!({ "cycle": false }).as_object().cloned();
        assert_eq!(
            SelectOp::Next { cycle: false },
            command("select_next", p.as_ref()).expect("ok")
        );
        assert_eq!(
            SelectOp::Previous { cycle: true },
            command("select_previous", None).expect("ok")
        );
    }
}
