// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The checks. They follow the steps of `tools/manual_test.sh` that need no person.

use crate::oracle::{self, Oracle};
use crate::ws::{field_ids, field_value};
use crate::{Check, Ctx, Fail};
use ratv::mrp::PlaybackState;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

macro_rules! ensure {
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(Fail::new(format!($($msg)+)));
        }
    };
}

const SELECTS: [&str; 10] = [
    "app",
    "audio_output",
    "audio_language",
    "subtitles",
    "account",
    "chapter",
    "up_next",
    "playback_speed",
    "shuffle",
    "repeat",
];

const SENSORS: [&str; 13] = [
    "app",
    "audio_output",
    "audio_language",
    "subtitles",
    "playback_speed",
    "shuffle",
    "repeat",
    "episode",
    "genre",
    "content_rating",
    "audio_format",
    "is_live",
    "volume_control",
];

/// Simple commands of the media player.
const SIMPLE_COMMANDS: [&str; 20] = [
    "APP_SWITCHER",
    "SCREENSAVER",
    "CONTROL_CENTER",
    "SKIP_FORWARD",
    "SKIP_BACKWARD",
    "FAST_FORWARD_BEGIN",
    "FAST_FORWARD_END",
    "REWIND_BEGIN",
    "REWIND_END",
    "SWIPE_LEFT",
    "SWIPE_RIGHT",
    "SWIPE_UP",
    "SWIPE_DOWN",
    "PLAY",
    "PAUSE",
    "PLAY_PAUSE_KEY",
    "LIKE",
    "DISLIKE",
    "NEXT_CHAPTER",
    "PREVIOUS_CHAPTER",
];

/// Longest entity id that the Remote accepts.
const MAX_ENTITY_ID_LEN: usize = 36;

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

async fn entity_list(ctx: &mut Ctx) -> Result<Vec<Value>, Fail> {
    let list = ctx
        .client
        .request_ok("get_available_entities", json!({}))
        .await?;
    Ok(list["available_entities"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

async fn entity_ids(ctx: &mut Ctx) -> Result<BTreeSet<String>, Fail> {
    Ok(entity_list(ctx)
        .await?
        .iter()
        .filter_map(|e| e["entity_id"].as_str().map(str::to_owned))
        .collect())
}

async fn subscribe_all(ctx: &mut Ctx) -> Result<usize, Fail> {
    let ids: Vec<String> = entity_ids(ctx).await?.into_iter().collect();
    ctx.client
        .request_ok("subscribe_events", json!({ "entity_ids": ids }))
        .await?;
    ctx.client.refresh_states().await?;
    Ok(ids.len())
}

/// Wait until the media player is not `UNAVAILABLE`. Returns the state.
async fn wait_available(ctx: &mut Ctx, secs: u64) -> Result<String, Fail> {
    let mp = ctx.mp();
    let state = ctx
        .client
        .wait_attr(&mp, "state", secs, |v| {
            v.as_str().is_some_and(|s| s != "UNAVAILABLE")
        })
        .await?;
    Ok(state.as_str().unwrap_or_default().to_owned())
}

fn mp_state(ctx: &Ctx) -> String {
    ctx.client.text(&ctx.mp(), "state")
}

fn playing(ctx: &Ctx) -> bool {
    matches!(mp_state(ctx).as_str(), "PLAYING" | "PAUSED" | "BUFFERING")
}

/// Response code of a command that is allowed to fail. Fails on a timeout or a slow answer.
async fn command_code(
    ctx: &mut Ctx,
    entity_type: &str,
    entity_id: &str,
    cmd_id: &str,
    params: Value,
) -> Result<u64, Fail> {
    let (response, took) = ctx
        .client
        .command(entity_type, entity_id, cmd_id, params)
        .await?;
    ensure!(
        took < Duration::from_secs(10),
        "{cmd_id} took {} ms",
        took.as_millis()
    );
    Ok(response["code"].as_u64().unwrap_or_default())
}

async fn mp_command(ctx: &mut Ctx, cmd_id: &str, params: Value) -> Result<(), Fail> {
    let mp = ctx.mp();
    ctx.client
        .command_ok("media_player", &mp, cmd_id, params)
        .await
}

async fn press(ctx: &mut Ctx, command: &str) -> Result<(), Fail> {
    let remote = ctx.remote();
    ctx.client
        .command_ok("remote", &remote, "send_cmd", json!({ "command": command }))
        .await
}

fn closed_lines(ctx: &Ctx) -> usize {
    ctx.driver.log_text().matches("connection closed").count()
}

fn connects(ctx: &Ctx) -> usize {
    ctx.driver.log_text().matches("connected in").count()
}

/// The oracle, or a skip when there is none.
fn oracle(ctx: &Ctx) -> Result<&Oracle, Fail> {
    ctx.oracle
        .as_ref()
        .ok_or_else(|| Fail::skip("no second connection to compare with"))
}

// ---------------------------------------------------------------- setup

pub async fn metadata(ctx: &mut Ctx) -> Check {
    let version = ctx
        .client
        .request_ok("get_driver_version", Value::Null)
        .await?;
    let metadata = ctx
        .client
        .request_ok("get_driver_metadata", Value::Null)
        .await?;
    ensure!(
        metadata["driver_id"] == "appletv_rs",
        "driver_id is {}",
        metadata["driver_id"]
    );
    ensure!(
        metadata["setup_data_schema"].is_object(),
        "no setup_data_schema"
    );
    Ok(format!("driver {}", version["version"]["driver"]))
}

async fn start_setup(ctx: &mut Ctx, reconfigure: bool) -> Result<Value, Fail> {
    ctx.client
        .request_ok(
            "setup_driver",
            json!({ "reconfigure": reconfigure, "setup_data": {} }),
        )
        .await?;
    ctx.client.next_page(10).await
}

async fn abort(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.client
        .send(json!({
            "kind": "event",
            "msg": "abort_driver_setup",
            "cat": "DEVICE",
            "msg_data": { "error": "USER_ABORT" },
        }))
        .await?;
    ctx.client.listen_ms(500).await
}

/// Invalid input on the first setup pages shows the page again.
pub async fn setup_input(ctx: &mut Ctx) -> Check {
    let page = start_setup(ctx, false).await?;
    ensure!(
        field_ids(&page).contains(&"restore_from_backup".to_owned()),
        "first page: {page}"
    );
    ctx.client
        .answer(json!({ "restore_from_backup": "false" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).contains(&"address".to_owned()),
        "discover page: {page}"
    );
    ctx.client
        .answer(json!({ "address": "not an address" }))
        .await?;
    let again = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&again).contains(&"address".to_owned()),
        "no discover page after a bad address: {again}"
    );
    abort(ctx).await?;
    Ok("bad address refused, setup aborted".into())
}

/// The driver config with the pairing, as backup text.
fn backup_text(ctx: &Ctx) -> Result<String, Fail> {
    let text = std::fs::read_to_string(&ctx.opt.config).map_err(|e| {
        Fail::new(format!(
            "{}: {e}. Pair the driver one time with the manual test, then use its config.json",
            ctx.opt.config.display()
        ))
    })?;
    let devices: Value = serde_json::from_str(&text)
        .map_err(|e| Fail::new(format!("{}: {e}", ctx.opt.config.display())))?;
    ensure!(
        devices.as_array().is_some_and(|d| !d.is_empty()),
        "no devices in {}",
        ctx.opt.config.display()
    );
    Ok(json!({ "version": 1, "devices": devices }).to_string())
}

/// First setup with the pairing of `--config`, through a restore.
pub async fn setup_restore(ctx: &mut Ctx) -> Check {
    let backup = backup_text(ctx)?;
    let page = start_setup(ctx, false).await?;
    ensure!(
        field_ids(&page).contains(&"restore_from_backup".to_owned()),
        "first page: {page}"
    );
    ctx.client
        .answer(json!({ "debug_log": "true", "restore_from_backup": "true" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).contains(&"restore_data".to_owned()),
        "restore page: {page}"
    );
    // A wrong text keeps the page.
    ctx.client
        .answer(json!({ "restore_data": "not a backup" }))
        .await?;
    ctx.client.next_page(10).await?;
    ctx.client.answer(json!({ "restore_data": backup })).await?;
    ctx.client.setup_done(20).await?;
    Ok("restored the pairing of the configuration file".into())
}

pub async fn entities(ctx: &mut Ctx) -> Check {
    let list = entity_list(ctx).await?;
    let mp = list
        .iter()
        .filter_map(|e| e["entity_id"].as_str())
        .find(|id| id.starts_with("media_player."))
        .ok_or_else(|| Fail::new("no media player entity"))?;
    mp.trim_start_matches("media_player.").clone_into(&mut ctx.key);
    let mut expected: BTreeSet<String> = [ctx.mp(), ctx.remote(), ctx.voice()].into();
    expected.extend(SELECTS.iter().map(|k| ctx.select(k)));
    expected.extend(SENSORS.iter().map(|k| ctx.sensor(k)));
    let ids: BTreeSet<String> = list
        .iter()
        .filter_map(|e| e["entity_id"].as_str().map(str::to_owned))
        .collect();
    let missing: Vec<_> = expected.difference(&ids).collect();
    let extra: Vec<_> = ids.difference(&expected).collect();
    ensure!(
        missing.is_empty() && extra.is_empty(),
        "missing {missing:?}, unexpected {extra:?}"
    );
    let bad: Vec<_> = ids
        .iter()
        .filter(|id| {
            id.len() > MAX_ENTITY_ID_LEN
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
        .collect();
    ensure!(bad.is_empty(), "invalid entity ids: {bad:?}");
    let unnamed: Vec<_> = list
        .iter()
        .filter(|e| e["name"]["en"].as_str().is_none_or(str::is_empty))
        .filter_map(|e| e["entity_id"].as_str())
        .collect();
    ensure!(unnamed.is_empty(), "entities without a name: {unnamed:?}");
    Ok(format!("key {}, {} entities", ctx.key, ids.len()))
}

pub async fn states(ctx: &mut Ctx) -> Check {
    ctx.client.send_event("connect").await?;
    let count = subscribe_all(ctx).await?;
    let state = wait_available(ctx, 30).await?;
    ctx.was_on = state != "OFF";
    ctx.client.listen(2).await?;
    let Some(address) = ctx.opt.address else {
        return Err(Fail::new("no address"));
    };
    let closed = closed_lines(ctx);
    let oracle = match Oracle::connect(address, &ctx.opt.credentials).await {
        Ok(oracle) => {
            ctx.oracle = Some(oracle);
            // The second connection must not push the driver out.
            ctx.client.listen(3).await?;
            ensure!(
                closed_lines(ctx) == closed && mp_state(ctx) != "UNAVAILABLE",
                "the driver lost the Apple TV when the second connection started"
            );
            "values are compared with a second connection".to_owned()
        }
        Err(e) => format!("NO second connection ({e}): values are not compared"),
    };
    Ok(format!("{count} entities, state {state}, {oracle}"))
}

// ---------------------------------------------------------------- values

/// Compare the driver values with the second connection. Returns the differences.
async fn mismatches(ctx: &Ctx) -> Result<Vec<String>, Fail> {
    let oracle = oracle(ctx)?;
    let c = &ctx.client;
    let mp = ctx.mp();
    let mut out = Vec::new();

    let awake = oracle::is_awake(oracle.power().await?);
    let state = c.text(&mp, "state");
    if awake == (state == "OFF") {
        out.push(format!("state {state}, Apple TV awake {awake}"));
    }

    if let Some(volume) = oracle.volume() {
        let expected = match volume.available {
            Some(true) => "on",
            Some(false) => "off",
            None => "",
        };
        let sensor = c.text(&ctx.sensor("volume_control"), "value");
        if !expected.is_empty() && sensor != expected {
            out.push(format!("volume_control {sensor}, Apple TV {expected}"));
        }
        // With more outputs, the driver can show the average of them.
        if let Some(level) = volume.level.filter(|_| volume.outputs.len() <= 1) {
            let shown = c.attr(&mp, "volume").and_then(Value::as_f64);
            let level = f64::from(level) * 100.0;
            if shown.is_none_or(|v| (v - level).abs() > 1.0) {
                out.push(format!("volume {shown:?}, Apple TV {level:.0}"));
            }
        }
        if let Some(muted) = volume.muted {
            let shown = c.attr(&mp, "muted").and_then(Value::as_bool);
            if shown != Some(muted) {
                out.push(format!("muted {shown:?}, Apple TV {muted}"));
            }
        }
    }

    if awake && let Some(np) = oracle.now_playing() {
        let shown = c.text(&mp, "media_title");
        if let Some(title) = np.title.as_deref().filter(|t| !t.is_empty())
            && !shown.contains(title)
        {
            out.push(format!("media_title {shown:?}, Apple TV {title:?}"));
        }
        if let Some(name) = np.app.as_deref().and_then(|b| oracle.app_name(b)) {
            let source = c.text(&mp, "source");
            if source != name {
                out.push(format!("source {source:?}, Apple TV {name:?}"));
            }
        }
    }

    let source = c.text(&mp, "source");
    for (entity, attr) in [
        (ctx.sensor("app"), "value"),
        (ctx.select("app"), "current_option"),
    ] {
        let shown = c.text(&entity, attr);
        if shown != source {
            out.push(format!(
                "{entity} {shown:?}, media player source {source:?}"
            ));
        }
    }
    let names: Vec<&str> = oracle.apps.iter().map(|a| a.name.as_str()).collect();
    let list = strings(c.attr(&mp, "source_list").unwrap_or(&Value::Null));
    let missing = names
        .iter()
        .filter(|n| !list.iter().any(|l| l == *n))
        .count();
    if missing > 0 {
        out.push(format!(
            "{missing} apps of the Apple TV not in the source list"
        ));
    }

    for kind in SELECTS {
        let id = ctx.select(kind);
        let current = c.text(&id, "current_option");
        let options = strings(c.attr(&id, "options").unwrap_or(&Value::Null));
        if !current.is_empty() && !options.contains(&current) {
            out.push(format!("{id}: {current:?} is not an option"));
        }
    }
    Ok(out)
}

/// Wait until the driver shows the values of the Apple TV.
async fn verify(ctx: &mut Ctx, secs: u64) -> Result<(), Fail> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let diff = mismatches(ctx).await?;
        if diff.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(diff.join("; ")));
        }
        ctx.client.listen(1).await?;
    }
}

pub async fn values(ctx: &mut Ctx) -> Check {
    oracle(ctx)?;
    // Companion and MRP state comes at different times: compare after it settles.
    verify(ctx, 5).await?;
    let mp = ctx.mp();
    let empty: Vec<&str> = SENSORS
        .iter()
        .filter(|k| ctx.client.text(&ctx.sensor(k), "value").is_empty())
        .copied()
        .collect();
    Ok(format!(
        "state {}, source {:?}, sensors without a value: {empty:?}",
        mp_state(ctx),
        ctx.client.text(&mp, "source")
    ))
}

pub async fn remote_ui(ctx: &mut Ctx) -> Check {
    let list = entity_list(ctx).await?;
    let find = |id: String| list.iter().find(|e| e["entity_id"] == id.as_str()).cloned();
    let remote = find(ctx.remote()).ok_or_else(|| Fail::new("no remote entity"))?;
    let features = strings(&remote["features"]);
    for f in ["send_cmd", "on_off", "toggle"] {
        ensure!(
            features.iter().any(|x| x == f),
            "remote features: {features:?}"
        );
    }
    let simple = strings(&remote["options"]["simple_commands"]);
    let missing: Vec<_> = SIMPLE_COMMANDS
        .iter()
        .filter(|c| !simple.iter().any(|s| s == *c))
        .collect();
    ensure!(
        missing.is_empty(),
        "remote simple commands missing: {missing:?}"
    );
    let pages = remote["options"]["user_interface"]["pages"]
        .as_array()
        .map_or(0, Vec::len);
    ensure!(pages > 0, "no UI pages");
    ensure!(
        remote["options"]["button_mapping"].is_array(),
        "no button mapping"
    );
    let mp = find(ctx.mp()).ok_or_else(|| Fail::new("no media player entity"))?;
    let mp_simple = strings(&mp["options"]["simple_commands"]);
    ensure!(
        SIMPLE_COMMANDS
            .iter()
            .all(|c| mp_simple.iter().any(|s| s == *c)),
        "media player simple commands: {mp_simple:?}"
    );
    Ok(format!("{} simple commands, {pages} pages", simple.len()))
}

// ---------------------------------------------------------------- wake and sleep

pub async fn standby(ctx: &mut Ctx) -> Check {
    let before = connects(ctx);
    ctx.client.send_event("enter_standby").await?;
    ctx.client.listen(3).await?;
    let in_standby = mp_state(ctx);
    ensure!(
        in_standby != "UNAVAILABLE",
        "state in standby: {in_standby}"
    );
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    while connects(ctx) == before {
        ensure!(
            start.elapsed() < Duration::from_secs(8),
            "no new connection after exit_standby"
        );
        ctx.client.listen_ms(100).await?;
    }
    let took = start.elapsed().as_millis();
    ctx.client.refresh_states().await?;
    let after = wait_available(ctx, 5).await?;
    if ctx.oracle.is_some() {
        verify(ctx, 5).await?;
    }
    Ok(format!(
        "connected {took} ms after exit_standby, state {after}"
    ))
}

pub async fn remote_wake(ctx: &mut Ctx) -> Check {
    let start = Instant::now();
    ctx.reconnect().await?;
    ctx.client.refresh_states().await?;
    let first = mp_state(ctx);
    let took = start.elapsed().as_millis();
    ensure!(
        first != "UNAVAILABLE",
        "first state after a new connect: {first}"
    );
    subscribe_all(ctx).await?;
    Ok(format!("state {first} after {took} ms"))
}

/// Wait until the second connection reports the power state.
async fn wait_power(ctx: &mut Ctx, awake: bool, secs: u64) -> Result<(), Fail> {
    let Some(oracle) = ctx.oracle.as_ref() else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(secs);
    while oracle::is_awake(oracle.power().await?) != awake {
        ensure!(
            Instant::now() < deadline,
            "the Apple TV is not {} after {secs} s",
            if awake { "awake" } else { "asleep" }
        );
        sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}

async fn turn_on(ctx: &mut Ctx) -> Result<(), Fail> {
    if mp_state(ctx) != "OFF" {
        return Ok(());
    }
    mp_command(ctx, "on", json!({})).await?;
    let mp = ctx.mp();
    ctx.client
        .wait_attr(&mp, "state", 15, |v| v != "OFF")
        .await?;
    wait_power(ctx, true, 10).await
}

pub async fn power(ctx: &mut Ctx) -> Check {
    if ctx.opt.no_power {
        return Err(Fail::skip("--no-power"));
    }
    let mp = ctx.mp();
    turn_on(ctx).await?;
    let start = Instant::now();
    mp_command(ctx, "off", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 15, "OFF").await?;
    let off = start.elapsed().as_millis();
    wait_power(ctx, false, 10).await?;
    // A command other than power turns the Apple TV on first.
    let start = Instant::now();
    press(ctx, "HOME").await?;
    ctx.client
        .wait_attr(&mp, "state", 15, |v| v != "OFF")
        .await?;
    let on = start.elapsed().as_millis();
    wait_power(ctx, true, 10).await?;
    mp_command(ctx, "toggle", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 15, "OFF").await?;
    mp_command(ctx, "toggle", json!({})).await?;
    ctx.client
        .wait_attr(&mp, "state", 15, |v| v != "OFF")
        .await?;
    Ok(format!(
        "off after {off} ms, on by HOME after {on} ms, toggle works"
    ))
}

// ---------------------------------------------------------------- commands

pub async fn remote_commands(ctx: &mut Ctx) -> Check {
    turn_on(ctx).await?;
    let remote = ctx.remote();
    press(ctx, "HOME").await?;
    for command in ["CURSOR_DOWN", "CURSOR_UP", "CURSOR_RIGHT", "CURSOR_LEFT"] {
        press(ctx, command).await?;
    }
    // Repeat and delay.
    let start = Instant::now();
    ctx.client
        .command_ok(
            "remote",
            &remote,
            "send_cmd",
            json!({ "command": "CURSOR_RIGHT", "repeat": 3, "delay": 200 }),
        )
        .await?;
    let repeat = start.elapsed().as_millis();
    ensure!(repeat >= 400, "repeat 3 with delay 200 took {repeat} ms");
    ctx.client
        .command_ok(
            "remote",
            &remote,
            "send_cmd_sequence",
            json!({ "sequence": ["CURSOR_LEFT", "CURSOR_LEFT", "CURSOR_LEFT"], "delay": 100 }),
        )
        .await?;
    // Hold: a long press on HOME opens Control Center.
    ctx.client
        .command_ok(
            "remote",
            &remote,
            "send_cmd",
            json!({ "command": "HOME", "hold": 1500 }),
        )
        .await?;
    press(ctx, "BACK").await?;
    let code = command_code(
        ctx,
        "remote",
        &remote,
        "send_cmd",
        json!({ "command": "NOT_A_COMMAND" }),
    )
    .await?;
    ensure!(code == 400, "unknown command: code {code}");
    press(ctx, "HOME").await?;
    Ok(format!(
        "repeat 3 took {repeat} ms, unknown command gives 400"
    ))
}

pub async fn simple_commands(ctx: &mut Ctx) -> Check {
    turn_on(ctx).await?;
    let mp = ctx.mp();
    let mut failed = Vec::new();
    // Each command with the command that closes what it opened. LIKE and DISLIKE are left
    // out: they change the ratings of the user.
    let pairs: [(&str, Option<&str>); 7] = [
        ("APP_SWITCHER", Some("HOME")),
        ("CONTROL_CENTER", Some("BACK")),
        ("SWIPE_DOWN", None),
        ("SWIPE_UP", None),
        ("SWIPE_RIGHT", None),
        ("SWIPE_LEFT", None),
        ("SCREENSAVER", Some("BACK")),
    ];
    for (command, close) in pairs {
        let code = command_code(ctx, "media_player", &mp, command, json!({})).await?;
        if code != 200 {
            failed.push(format!("{command}: {code}"));
        }
        ctx.client.listen_ms(800).await?;
        if let Some(close) = close {
            press(ctx, close).await?;
            ctx.client.listen_ms(500).await?;
        }
    }
    press(ctx, "HOME").await?;
    ensure!(failed.is_empty(), "{}", failed.join(", "));
    Ok("navigation commands work".into())
}

pub async fn app_select(ctx: &mut Ctx) -> Check {
    turn_on(ctx).await?;
    let mp = ctx.mp();
    let select = ctx.select("app");
    let options = strings(ctx.client.attr(&select, "options").unwrap_or(&Value::Null));
    ensure!(!options.is_empty(), "no apps");
    let current = ctx.client.text(&mp, "source");
    // Settings is on every Apple TV and plays nothing.
    let target = ctx
        .oracle
        .as_ref()
        .and_then(|o| o.app_name("com.apple.TVSettings"))
        .map(str::to_owned)
        .or_else(|| options.iter().find(|o| **o != current).cloned())
        .ok_or_else(|| Fail::new("no other app"))?;
    let start = Instant::now();
    ctx.client
        .command_ok(
            "select",
            &select,
            "select_option",
            json!({ "option": target }),
        )
        .await?;
    ctx.client.wait_text(&mp, "source", 10, &target).await?;
    let took = start.elapsed().as_millis();
    ctx.client
        .wait_text(&select, "current_option", 2, &target)
        .await?;
    let code = command_code(
        ctx,
        "select",
        &select,
        "select_option",
        json!({ "option": "Not an app" }),
    )
    .await?;
    ensure!(code != 200, "an unknown app was accepted");
    press(ctx, "HOME").await?;
    Ok(format!(
        "{target} shown after {took} ms, unknown app: code {code}"
    ))
}

pub async fn volume(ctx: &mut Ctx) -> Check {
    turn_on(ctx).await?;
    let mp = ctx.mp();
    let control = ctx.client.text(&ctx.sensor("volume_control"), "value");
    if control == "off" {
        let (response, _) = ctx
            .client
            .command("media_player", &mp, "volume_up", json!({}))
            .await?;
        ensure!(
            response["code"] == 503,
            "volume_up without volume control: {response}"
        );
        let text = response["msg_data"].to_string();
        ensure!(
            text.contains("not available"),
            "volume_up error text: {text}"
        );
        return Ok("no volume control: volume_up gives 503".into());
    }
    let before = ctx
        .client
        .attr(&mp, "volume")
        .and_then(Value::as_f64)
        .ok_or_else(|| Fail::new("no volume"))?;
    mp_command(ctx, "volume_down", json!({})).await?;
    ctx.client
        .wait_attr(&mp, "volume", 5, |v| v.as_f64().is_some_and(|v| v < before))
        .await?;
    mp_command(ctx, "volume_up", json!({})).await?;
    ctx.client
        .wait_attr(&mp, "volume", 5, |v| {
            v.as_f64().is_some_and(|v| (v - before).abs() <= 1.0)
        })
        .await?;
    if ctx.oracle.is_some() {
        verify(ctx, 3).await?;
    }
    Ok(format!("volume {before} down and up"))
}

// ---------------------------------------------------------------- playback

fn need_playback(ctx: &Ctx) -> Result<(), Fail> {
    if playing(ctx) {
        Ok(())
    } else {
        Err(Fail::skip(
            "nothing is playing: start a video with chapters and languages before the run",
        ))
    }
}

/// Wait until the second connection reports the playback state.
async fn wait_playback(ctx: &mut Ctx, expected: PlaybackState) -> Result<(), Fail> {
    let Some(oracle) = ctx.oracle.as_ref() else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while oracle.now_playing().and_then(|np| np.state) != Some(expected) {
        ensure!(
            Instant::now() < deadline,
            "the Apple TV is not {expected:?} after 5 s"
        );
        sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

fn position(ctx: &Ctx) -> Option<f64> {
    ctx.client
        .attr(&ctx.mp(), "media_position")
        .and_then(Value::as_f64)
}

pub async fn playback(ctx: &mut Ctx) -> Check {
    need_playback(ctx)?;
    let mp = ctx.mp();
    mp_command(ctx, "PAUSE", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PAUSED").await?;
    wait_playback(ctx, PlaybackState::Paused).await?;
    mp_command(ctx, "PLAY", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PLAYING").await?;
    wait_playback(ctx, PlaybackState::Playing).await?;
    mp_command(ctx, "play_pause", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PAUSED").await?;
    mp_command(ctx, "play_pause", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PLAYING").await?;

    let mut notes = Vec::new();
    let duration = ctx
        .client
        .attr(&mp, "media_duration")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    if let Some(start) = position(ctx).filter(|_| duration > 120.0) {
        let target = (start + 60.0).min(duration - 30.0);
        mp_command(ctx, "seek", json!({ "media_position": target })).await?;
        ctx.client
            .wait_attr(&mp, "media_position", 5, |v| {
                v.as_f64().is_some_and(|p| (p - target).abs() < 5.0)
            })
            .await?;
        mp_command(ctx, "SKIP_BACKWARD", json!({})).await?;
        ctx.client
            .wait_attr(&mp, "media_position", 5, |v| {
                v.as_f64().is_some_and(|p| p < target - 5.0)
            })
            .await?;
        notes.push(format!("seek to {target:.0} s and skip back work"));
    } else {
        notes.push("no duration: seek not tested".into());
    }

    // Fast forward runs until the end command.
    mp_command(ctx, "FAST_FORWARD_BEGIN", json!({})).await?;
    ctx.client.listen(2).await?;
    mp_command(ctx, "FAST_FORWARD_END", json!({})).await?;
    ctx.client.listen(1).await?;
    let after = mp_state(ctx);
    ensure!(after == "PLAYING", "state after fast forward: {after}");

    let speed = ctx.select("playback_speed");
    let options = strings(ctx.client.attr(&speed, "options").unwrap_or(&Value::Null));
    if let Some(fast) = options.iter().find(|o| o.as_str() != "1x") {
        let fast = fast.clone();
        ctx.client
            .command_ok("select", &speed, "select_option", json!({ "option": fast }))
            .await?;
        ctx.client
            .wait_text(&speed, "current_option", 5, &fast)
            .await?;
        ctx.client
            .command_ok("select", &speed, "select_option", json!({ "option": "1x" }))
            .await?;
        ctx.client
            .wait_text(&speed, "current_option", 5, "1x")
            .await?;
        notes.push(format!("speed {fast} and back"));
    }
    if ctx.oracle.is_some() {
        verify(ctx, 3).await?;
    }
    Ok(notes.join(", "))
}

/// Choose each option of a select and wait until it is shown. Returns the options tested.
async fn sweep(ctx: &mut Ctx, kind: &str) -> Result<usize, Fail> {
    let id = ctx.select(kind);
    let options = strings(ctx.client.attr(&id, "options").unwrap_or(&Value::Null));
    if options.len() < 2 {
        return Ok(0);
    }
    let original = ctx.client.text(&id, "current_option");
    for option in options.iter().filter(|o| **o != original) {
        ctx.client
            .command_ok("select", &id, "select_option", json!({ "option": option }))
            .await?;
        ctx.client
            .wait_text(&id, "current_option", 8, option)
            .await
            .map_err(|e| Fail::new(format!("{kind}: {e}")))?;
    }
    if !original.is_empty() {
        ctx.client
            .command_ok(
                "select",
                &id,
                "select_option",
                json!({ "option": original }),
            )
            .await?;
        ctx.client
            .wait_text(&id, "current_option", 8, &original)
            .await?;
    }
    Ok(options.len())
}

pub async fn languages(ctx: &mut Ctx) -> Check {
    need_playback(ctx)?;
    let audio = sweep(ctx, "audio_language").await?;
    let subtitles = sweep(ctx, "subtitles").await?;
    let sensor = ctx.client.text(&ctx.sensor("subtitles"), "value");
    let select = ctx.client.text(&ctx.select("subtitles"), "current_option");
    ensure!(
        sensor == select,
        "subtitles sensor {sensor:?}, select {select:?}"
    );
    if audio + subtitles == 0 {
        return Err(Fail::skip(
            "the video has one audio language and no subtitles",
        ));
    }
    Ok(format!(
        "{audio} audio languages, {subtitles} subtitle options"
    ))
}

/// Read a URL of the driver. Returns the status line and the headers.
async fn http_get(url: &str) -> Result<(String, usize), Fail> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Fail::new(format!("not an http URL: {url}")))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(host).await?;
    stream
        .write_all(format!("GET /{path} HTTP/1.0\r\nHost: {host}\r\n\r\n").as_bytes())
        .await?;
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut body))
        .await
        .map_err(|_| Fail::new("no answer in 5 s"))??;
    let end = body
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| Fail::new("no HTTP header"))?;
    let head = String::from_utf8_lossy(&body[..end]).into_owned();
    Ok((head, body.len() - end - 4))
}

pub async fn artwork(ctx: &mut Ctx) -> Check {
    let url = ctx.client.text(&ctx.mp(), "media_image_url");
    if url.is_empty() {
        return Err(Fail::skip("no artwork"));
    }
    let own = format!("http://127.0.0.1:{}/", ctx.driver.port);
    if !url.starts_with(&own) {
        return Ok(format!("URL of the app: {url}"));
    }
    let (head, size) = http_get(&url).await?;
    let lower = head.to_ascii_lowercase();
    ensure!(head.contains(" 200 "), "artwork: {head}");
    ensure!(
        lower.contains("content-type: image/"),
        "artwork type: {head}"
    );
    ensure!(size > 0, "empty artwork");
    let (missing, _) = http_get(&format!("{own}artwork/{}/missing.png", ctx.key)).await?;
    ensure!(missing.contains(" 404 "), "unknown artwork: {missing}");
    Ok(format!("image of {size} bytes from the driver"))
}

// ---------------------------------------------------------------- reconfiguration

pub async fn abort_setup(ctx: &mut Ctx) -> Check {
    start_setup(ctx, true).await?;
    abort(ctx).await?;
    let state = wait_available(ctx, 10).await?;
    press(ctx, "HOME").await?;
    Ok(format!("state {state}, commands work after the abort"))
}

fn read_config(ctx: &Ctx) -> Result<Value, Fail> {
    let text = std::fs::read_to_string(ctx.driver.config_file())?;
    serde_json::from_str(&text).map_err(|e| Fail::new(format!("config.json: {e}")))
}

fn device_config(ctx: &Ctx) -> Result<Value, Fail> {
    read_config(ctx)?
        .as_array()
        .and_then(|a| a.iter().find(|d| d["key"] == ctx.key.as_str()).cloned())
        .ok_or_else(|| Fail::new("the device is not in config.json"))
}

/// Change the options of the device in the setup.
async fn set_options(ctx: &mut Ctx, global_volume: bool) -> Result<(), Fail> {
    let key = ctx.key.clone();
    let address = device_config(ctx)?["address"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    start_setup(ctx, true).await?;
    ctx.client
        .answer(json!({ "choice": key, "action": "configure" }))
        .await?;
    // The page comes after a scan.
    let page = ctx.client.next_page(15).await?;
    ensure!(
        field_ids(&page).contains(&"global_volume".to_owned()),
        "configure page: {page}"
    );
    ctx.client
        .answer(json!({
            "address": address,
            "global_volume": global_volume.to_string(),
            "device": "",
        }))
        .await?;
    ctx.client.setup_done(10).await
}

pub async fn configure(ctx: &mut Ctx) -> Check {
    set_options(ctx, false).await?;
    let off = device_config(ctx)?["global_volume"] == false;
    set_options(ctx, true).await?;
    let on = device_config(ctx)?["global_volume"] == true;
    ensure!(off && on, "global_volume not saved: off {off}, on {on}");
    wait_available(ctx, 20).await?;
    Ok("global_volume saved".into())
}

pub async fn backup_restore(ctx: &mut Ctx) -> Check {
    let key = ctx.key.clone();
    let ids = entity_ids(ctx).await?;
    start_setup(ctx, true).await?;
    ctx.client
        .answer(json!({ "choice": key, "action": "backup" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    let backup = field_value(&page, "backup_data")
        .filter(|b| !b.is_empty())
        .ok_or_else(|| Fail::new("no backup text"))?;
    ctx.client.answer(json!({})).await?;
    ctx.client.setup_done(10).await?;
    let parsed: Value =
        serde_json::from_str(&backup).map_err(|e| Fail::new(format!("backup text: {e}")))?;
    ensure!(
        parsed["devices"][0]["key"] == key.as_str(),
        "backup has another device"
    );

    start_setup(ctx, true).await?;
    ctx.client
        .answer(json!({ "choice": key, "action": "restore" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).contains(&"restore_data".to_owned()),
        "restore page: {page}"
    );
    ctx.client
        .answer(json!({ "restore_data": "{\"version\": 1, \"devices\": []}" }))
        .await?;
    ctx.client.next_page(10).await?;
    ctx.client.answer(json!({ "restore_data": backup })).await?;
    ctx.client.setup_done(20).await?;
    let after = entity_ids(ctx).await?;
    ensure!(after == ids, "entities differ after the restore");
    subscribe_all(ctx).await?;
    wait_available(ctx, 30).await?;
    Ok(format!("backup of {} characters restored", backup.len()))
}

/// Start the driver again.
async fn restart(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.driver.start().await?;
    ctx.reconnect().await
}

pub async fn driver_restart(ctx: &mut Ctx) -> Check {
    let ids = entity_ids(ctx).await?;
    ctx.driver.stop().await;
    let start = Instant::now();
    restart(ctx).await?;
    let after = entity_ids(ctx).await?;
    ensure!(after == ids, "entities differ after the restart");
    ctx.client.forget_states();
    subscribe_all(ctx).await?;
    let state = wait_available(ctx, 30).await?;
    let took = start.elapsed().as_millis();
    let apps = strings(
        ctx.client
            .attr(&ctx.mp(), "source_list")
            .unwrap_or(&Value::Null),
    )
    .len();
    if ctx.oracle.is_some() {
        verify(ctx, 10).await?;
    }
    Ok(format!(
        "state {state} after {took} ms, {apps} apps from the cache"
    ))
}

pub async fn config_file(ctx: &mut Ctx) -> Check {
    let device = device_config(ctx)?;
    for field in [
        "id",
        "key",
        "name",
        "known_ids",
        "credentials",
        "global_volume",
    ] {
        ensure!(device.get(field).is_some(), "config.json has no {field}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(ctx.driver.config_file())?
            .permissions()
            .mode()
            & 0o777;
        ensure!(mode == 0o600, "config.json mode {mode:o}");
    }
    let cache = ctx.driver.dir.join(format!("cache_{}.json", ctx.key));
    ensure!(cache.exists(), "no cache file");
    Ok(format!(
        "address {}, mode 600, cache file",
        device["address"]
    ))
}

pub async fn broken_config(ctx: &mut Ctx) -> Check {
    let good = std::fs::read_to_string(ctx.driver.config_file())?;
    ctx.driver.stop().await;
    std::fs::write(ctx.driver.config_file(), "[{\"broken\": ")?;
    let result = async {
        restart(ctx).await?;
        ctx.client
            .request_ok("get_driver_version", Value::Null)
            .await?;
        ensure!(ctx.driver.alive(), "the driver ended");
        let moved = bad_files(ctx).len();
        ensure!(moved == 1, "{moved} moved files");
        Ok("the driver runs, the file was moved".to_owned())
    }
    .await;
    ctx.driver.stop().await;
    for file in bad_files(ctx) {
        let _ = std::fs::remove_file(file);
    }
    std::fs::write(ctx.driver.config_file(), good)?;
    restart(ctx).await?;
    ctx.client.forget_states();
    subscribe_all(ctx).await?;
    wait_available(ctx, 30).await?;
    result
}

fn bad_files(ctx: &Ctx) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(&ctx.driver.dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.json.bad-")
        })
        .map(|e| e.path())
        .collect()
}

// ---------------------------------------------------------------- end

pub async fn restore_power(ctx: &mut Ctx) -> Check {
    if ctx.was_on || mp_state(ctx) == "OFF" {
        return Ok(String::new());
    }
    mp_command(ctx, "off", json!({})).await?;
    ctx.client.wait_text(&ctx.mp(), "state", 15, "OFF").await?;
    Ok("the Apple TV is off again, like at the start".into())
}

pub async fn log(ctx: &mut Ctx) -> Check {
    let text = ctx.driver.log_text();
    let credentials: Vec<String> = read_config(ctx)?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["credentials"].as_str().map(str::to_owned))
        .collect();
    // A part of the secret is enough to find it.
    ensure!(
        !credentials
            .iter()
            .any(|c| c.len() > 24 && text.contains(&c[c.len() - 24..])),
        "the log contains the credentials"
    );
    ensure!(!text.contains("panicked"), "the driver panicked");
    let lines = |tag: &str| -> Vec<&str> { text.lines().filter(|l| l.contains(tag)).collect() };
    // The broken file check causes one ERROR line.
    let errors: Vec<&str> = lines(" ERROR ")
        .into_iter()
        .filter(|l| !l.contains("config.json.bad-"))
        .collect();
    let warnings = lines(" WARN ");
    for line in errors.iter().chain(&warnings) {
        println!("      {line}");
    }
    ensure!(errors.is_empty(), "{} ERROR lines in the log", errors.len());
    Ok(format!(
        "{} warnings, no errors, no credentials. Log: {}",
        warnings.len(),
        ctx.driver.log.display()
    ))
}
