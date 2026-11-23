// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Remote entity: buttons, UI page and commands. Commands use the media player mapping.

use crate::device::state::{Power, Snapshot};
use crate::entities::media_player::{self, Action, SimpleCommand};
use crate::errors::ServiceError;
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::time::Duration;
use uc_api::intg::{
    AvailableIntgEntity, IntgRemoteCommand, IntgRemoteFeature, IntgRemoteOptionField,
};

/// Highest repeat count of a command.
const MAX_REPEAT: u64 = 20;
/// Longest hold time of a key in milliseconds. The device command lock is held for this time.
const MAX_HOLD_MS: u64 = 10_000;
/// Longest pause between commands in milliseconds.
const MAX_DELAY_MS: u64 = 5_000;

/// Media player commands offered as simple commands of the remote.
const MEDIA_PLAYER_COMMANDS: &[&str] = &[
    "BACK",
    "HOME",
    "VOLUME_UP",
    "VOLUME_DOWN",
    "MUTE",
    "MUTE_TOGGLE",
    "CURSOR_UP",
    "CURSOR_DOWN",
    "CURSOR_LEFT",
    "CURSOR_RIGHT",
    "CURSOR_ENTER",
    "CHANNEL_UP",
    "CHANNEL_DOWN",
    "PREVIOUS",
    "PLAY_PAUSE",
    "NEXT",
    "MENU",
    "STOP",
    "GUIDE",
    "CONTEXT_MENU",
];

pub fn definition(mut entity: AvailableIntgEntity) -> AvailableIntgEntity {
    use strum::VariantArray;

    entity.features = Some(
        [
            IntgRemoteFeature::SendCmd,
            IntgRemoteFeature::OnOff,
            IntgRemoteFeature::Toggle,
        ]
        .iter()
        .map(|f| f.as_ref().to_string())
        .collect(),
    );
    let simple: Vec<&str> = MEDIA_PLAYER_COMMANDS
        .iter()
        .copied()
        .chain(SimpleCommand::VARIANTS.iter().map(AsRef::as_ref))
        .collect();
    let mut options = Map::new();
    options.insert(
        IntgRemoteOptionField::SimpleCommands.as_ref().into(),
        json!(simple),
    );
    options.insert(
        IntgRemoteOptionField::ButtonMapping.as_ref().into(),
        button_mapping(),
    );
    options.insert(
        IntgRemoteOptionField::UserInterface.as_ref().into(),
        json!({ "pages": [main_page()] }),
    );
    entity.options = Some(options);
    entity
}

/// Entity state of the remote.
pub fn state(s: &Snapshot) -> &'static str {
    if !s.connected {
        "UNAVAILABLE"
    } else if s.power == Power::Off {
        "OFF"
    } else {
        "ON"
    }
}

fn button_mapping() -> Value {
    let short =
        |button: &str, cmd: &str| json!({ "button": button, "short_press": { "cmd_id": cmd } });
    json!([
        short("BACK", "BACK"),
        short("DPAD_DOWN", "CURSOR_DOWN"),
        short("DPAD_LEFT", "CURSOR_LEFT"),
        short("DPAD_RIGHT", "CURSOR_RIGHT"),
        short("DPAD_MIDDLE", "CURSOR_ENTER"),
        short("DPAD_UP", "CURSOR_UP"),
        short("VOLUME_UP", "VOLUME_UP"),
        short("VOLUME_DOWN", "VOLUME_DOWN"),
        short("MUTE", "MUTE_TOGGLE"),
        short("POWER", "TOGGLE"),
        short("PREV", SimpleCommand::RewindBegin.as_ref()),
        short("PLAY", "PLAY_PAUSE"),
        short("NEXT", SimpleCommand::FastForwardBegin.as_ref()),
        short("STOP", "STOP"),
        short("MENU", "CONTEXT_MENU"),
        short("CHANNEL_UP", "CHANNEL_UP"),
        short("CHANNEL_DOWN", "CHANNEL_DOWN"),
        short("HOME", "HOME"),
    ])
}

fn main_page() -> Value {
    let item = |y: u8, cmd: &str, text: &str| {
        json!({
            "command": { "cmd_id": cmd },
            "location": { "x": 0, "y": y },
            "size": { "height": 1, "width": 1 },
            "text": text,
            "type": "text",
        })
    };
    json!({
        "page_id": "apple_tv_commands_main",
        "name": "Main",
        "grid": { "width": 1, "height": 4 },
        "items": [
            item(0, SimpleCommand::AppSwitcher.as_ref(), "App Switcher"),
            item(1, SimpleCommand::Screensaver.as_ref(), "Screensaver"),
            item(2, SimpleCommand::ControlCenter.as_ref(), "Control Center"),
            item(3, "GUIDE", "Guide"),
        ],
    })
}

/// Commands of the remote entity.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteRun {
    /// Actions to run in order.
    pub actions: Vec<Action>,
    /// Number of runs of the whole list.
    pub repeat: u64,
    /// Pause between commands.
    pub delay: Duration,
}

impl RemoteRun {
    /// A run that takes time: more than one command, a repeat or a held key. The delay is only
    /// used between commands, so it alone does not make a run long.
    /// `stop_send` stops only such runs.
    pub fn is_long(&self) -> bool {
        self.actions.len() > 1
            || self.repeat > 1
            || self.actions.iter().any(|a| matches!(a, Action::Hold(..)))
    }
}

/// Map a remote entity command.
pub fn command(
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> Result<RemoteRun, ServiceError> {
    let cmd = IntgRemoteCommand::from_str(cmd_id)
        .map_err(|_| ServiceError::BadRequest(format!("Unknown command: {cmd_id}")))?;
    let single = |action| RemoteRun {
        actions: vec![action],
        repeat: 1,
        delay: Duration::ZERO,
    };
    match cmd {
        IntgRemoteCommand::On => Ok(single(Action::On)),
        IntgRemoteCommand::Off => Ok(single(Action::Off)),
        IntgRemoteCommand::Toggle => Ok(single(Action::Toggle)),
        IntgRemoteCommand::SendCmd => {
            let command = media_player::param_str(params, "command")?;
            let action = with_hold(map_command(command)?, hold(params));
            Ok(RemoteRun {
                actions: vec![action],
                repeat: repeat(params),
                delay: delay(params),
            })
        }
        IntgRemoteCommand::SendCmdSequence => {
            let sequence = params
                .and_then(|p| p.get("sequence"))
                .and_then(Value::as_array)
                .ok_or_else(|| ServiceError::BadRequest("Missing parameter: sequence".into()))?;
            let hold = hold(params);
            let actions = sequence
                .iter()
                .map(|c| {
                    c.as_str()
                        .ok_or_else(|| {
                            ServiceError::BadRequest("Invalid command in sequence".into())
                        })
                        .and_then(map_command)
                        .map(|a| with_hold(a, hold))
                })
                .collect::<Result<_, _>>()?;
            Ok(RemoteRun {
                actions,
                repeat: repeat(params),
                delay: delay(params),
            })
        }
        // The device actor handles it before the parameters are checked.
        IntgRemoteCommand::StopSend => Err(ServiceError::NotSupported(cmd_id.into())),
    }
}

/// Map a command name: simple commands as they are, media player commands in lower case.
fn map_command(command: &str) -> Result<Action, ServiceError> {
    if command.starts_with("remote.") {
        return Err(ServiceError::BadRequest(format!(
            "Unsupported command: {command}"
        )));
    }
    if SimpleCommand::from_str(command).is_ok() {
        return media_player::command(command, None);
    }
    media_player::command(&command.to_ascii_lowercase(), None)
}

/// Repeat count. The Remote sends 4 for a held button: this is changed to 1 until the
/// Remote is fixed.
fn repeat(params: Option<&Map<String, Value>>) -> u64 {
    match param_u64(params, "repeat") {
        0 | 4 => 1,
        r => r.min(MAX_REPEAT),
    }
}

/// Time before a key is released. 0 is a normal press.
fn hold(params: Option<&Map<String, Value>>) -> Duration {
    Duration::from_millis(param_u64(params, "hold").min(MAX_HOLD_MS))
}

/// Hold the key of `action` for `hold`. Actions without a key are not changed.
fn with_hold(action: Action, hold: Duration) -> Action {
    match action.key() {
        Some(key) if !hold.is_zero() => Action::Hold(key, hold),
        _ => action,
    }
}

fn delay(params: Option<&Map<String, Value>>) -> Duration {
    Duration::from_millis(param_u64(params, "delay").min(MAX_DELAY_MS))
}

fn param_u64(params: Option<&Map<String, Value>>, name: &str) -> u64 {
    params
        .and_then(|p| p.get(name))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratv::Key;

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn send_cmd() {
        let p = params(json!({ "command": "CURSOR_UP", "repeat": 3, "delay": 100 }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert_eq!(vec![Action::Press(Key::Up)], run.actions);
        assert_eq!(3, run.repeat);
        assert_eq!(Duration::from_millis(100), run.delay);
    }

    #[test]
    fn long_runs() {
        let run = |v: Value| command("send_cmd", Some(&params(v))).expect("ok");
        assert!(!run(json!({ "command": "HOME" })).is_long());
        assert!(run(json!({ "command": "HOME", "repeat": 2 })).is_long());
        assert!(!run(json!({ "command": "HOME", "delay": 100 })).is_long());
        assert!(run(json!({ "command": "HOME", "hold": 1000 })).is_long());
        let p = params(json!({ "sequence": ["HOME", "BACK"] }));
        assert!(
            command("send_cmd_sequence", Some(&p))
                .expect("ok")
                .is_long()
        );
        let p = params(json!({ "sequence": ["HOME"] }));
        assert!(
            !command("send_cmd_sequence", Some(&p))
                .expect("ok")
                .is_long()
        );
    }

    #[test]
    fn repeat_quirk() {
        for (value, expected) in [(0, 1), (1, 1), (4, 1), (5, 5), (50, MAX_REPEAT)] {
            let p = params(json!({ "command": "HOME", "repeat": value }));
            assert_eq!(
                expected,
                command("send_cmd", Some(&p)).expect("ok").repeat,
                "repeat {value}"
            );
        }
    }

    #[test]
    fn hold_turns_key_into_long_press() {
        let p = params(json!({ "command": "CURSOR_ENTER", "hold": 2000 }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert_eq!(
            vec![Action::Hold(Key::Select, Duration::from_secs(2))],
            run.actions
        );

        let p = params(json!({ "command": "CONTEXT_MENU" }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert_eq!(vec![Action::ContextMenu], run.actions);
    }

    #[test]
    fn hold_and_delay_are_limited() {
        let p = params(json!({ "command": "CURSOR_ENTER", "hold": u64::MAX, "delay": "999999" }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert_eq!(
            vec![Action::Hold(
                Key::Select,
                Duration::from_millis(MAX_HOLD_MS)
            )],
            run.actions
        );
        assert_eq!(Duration::from_millis(MAX_DELAY_MS), run.delay);

        let p = params(json!({ "sequence": ["HOME"], "delay": 60_000 }));
        let run = command("send_cmd_sequence", Some(&p)).expect("ok");
        assert_eq!(Duration::from_millis(MAX_DELAY_MS), run.delay);
    }

    #[test]
    fn sequence() {
        let p = params(json!({ "sequence": ["HOME", "APP_SWITCHER", "cursor_down"] }));
        let run = command("send_cmd_sequence", Some(&p)).expect("ok");
        assert_eq!(
            vec![
                Action::Press(Key::Home),
                Action::AppSwitcher,
                Action::Press(Key::Down)
            ],
            run.actions
        );

        // `hold` is used for each key. Actions without a key stay as they are.
        let p = params(json!({ "sequence": ["HOME", "APP_SWITCHER"], "hold": 500 }));
        let run = command("send_cmd_sequence", Some(&p)).expect("ok");
        let hold = Duration::from_millis(500);
        assert_eq!(
            vec![Action::Hold(Key::Home, hold), Action::AppSwitcher],
            run.actions
        );
    }

    #[test]
    fn rejects_remote_prefix() {
        let p = params(json!({ "command": "remote.on" }));
        assert!(command("send_cmd", Some(&p)).is_err());
    }
}
