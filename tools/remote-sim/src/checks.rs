// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The checks. Some ask the owner on the terminal: to start the right video, to choose a speed
//! that tvOS does not take from the second connection, whether Siri answered, and to restart the
//! Apple TV. Without a terminal they do not ask. `siri_direct` compares Siri through ratv
//! directly with Siri through the driver. `tools/manual_test.sh` has the steps that are not
//! checked here yet.

use crate::driver::log::{DriverLog, RawLog, driver_log, has_any, log_after};
use crate::driver::{Driver, LogMark, RustLog};
use crate::oracle::{self, Content, Oracle};
use crate::ws::{Client, field_ids, field_value};
use crate::{Check, Ctx, Fail};
use actix_ws_client::WebSocketConfig;
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

/// Bundle id of Settings. It is on every Apple TV and plays nothing.
const SETTINGS: &str = "com.apple.TVSettings";

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

/// The [`CLOSED`] lines of the driver in its log after `mark`, joined with " | ". Empty if there
/// is none.
fn closed_lines(driver: &Driver, mark: LogMark) -> String {
    driver_log(driver, mark).lines_with(&CLOSED).join(" | ")
}

/// Wait up to `secs` until a driver line after `mark` has `pattern` in its message.
async fn wait_log(ctx: &mut Ctx, mark: LogMark, pattern: &str, secs: u64) -> Result<(), Fail> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !driver_log(&ctx.driver, mark).contains(pattern) {
        ensure!(
            Instant::now() < deadline,
            "no driver log line with '{pattern}' in {secs} s"
        );
        ctx.client.listen_ms(100).await?;
    }
    Ok(())
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

/// Pair the driver with the Apple TV at `--address` through the setup flow. The PIN is read
/// from the terminal.
pub async fn pair_driver(ctx: &mut Ctx) -> Check {
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
    let address = ctx.opt.address.map(|a| a.to_string()).unwrap_or_default();
    ctx.client.answer(json!({ "address": address })).await?;
    let mut page = ctx.client.next_page(30).await?;
    if field_ids(&page).contains(&"choice".to_owned()) {
        let choice = field_value(&page, "choice")
            .filter(|c| !c.is_empty())
            .ok_or_else(|| Fail::new(format!("no device to choose: {page}")))?;
        ctx.client.answer(json!({ "choice": choice })).await?;
        page = ctx.client.next_page(30).await?;
    }
    ensure!(
        field_ids(&page).contains(&"pin".to_owned()),
        "PIN page: {page}"
    );
    println!("Type the PIN shown on the Apple TV and press Enter:");
    let pin = read_pin(ctx).await?;
    ctx.client.answer(json!({ "pin": pin })).await?;
    ctx.client.setup_done(30).await?;
    Ok("driver paired".into())
}

/// Read the PIN from the terminal.
async fn read_pin(ctx: &mut Ctx) -> Result<String, Fail> {
    // A PIN from a pipe is kept.
    if terminal() {
        drop_typed_lines(ctx);
    }
    let line = read_line(ctx).await?.unwrap_or_default();
    let pin = line.trim();
    ensure!(!pin.is_empty(), "no PIN");
    Ok(pin.to_owned())
}

/// Lines of the terminal. One thread reads them, so no typed line is lost between two reads.
/// The channel closes at the end of the input.
pub fn stdin_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Throw away the lines typed before a prompt, so they do not answer it.
fn drop_typed_lines(ctx: &mut Ctx) {
    if let Some(rx) = ctx.stdin.as_mut() {
        while rx.try_recv().is_ok() {}
    }
}

/// Read a line from the terminal. `None` at the end of the input. The WebSocket is read
/// meanwhile, so a closed connection fails the check and the states stay current.
async fn read_line(ctx: &mut Ctx) -> Result<Option<String>, Fail> {
    let rx = ctx.stdin.get_or_insert_with(stdin_lines);
    loop {
        tokio::select! {
            line = rx.recv() => return Ok(line),
            result = ctx.client.listen_ms(500) => result?,
        }
    }
}

/// What the owner answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    Done,
    Yes,
    No,
    Skip,
    /// The input ended, so the owner cannot answer.
    NoInput,
    /// Stdin is not a terminal, so the question is not asked.
    NoTerminal,
}

/// Kind of question to the owner.
#[derive(Clone, Copy)]
enum Ask {
    /// Something to do. Enter means it is done, s skips. The texts say what each key means.
    Action {
        done: &'static str,
        skip: &'static str,
    },
    /// A yes or no question. The texts say what y, n and s mean.
    Question {
        yes: &'static str,
        no: &'static str,
        skip: &'static str,
    },
}

/// Report text for a question that is not asked, because stdin is not a terminal.
const NO_TERMINAL: &str = "not asked: no terminal";

/// What s + Enter means for most questions.
const SKIP_CHECK: &str = "skip this check";

/// Ask the owner on the terminal and wait for the answer. Without a terminal it returns
/// `NoTerminal`, at the end of the input `NoInput`. The question and the answer go to the report
/// of the step.
async fn ask(ctx: &mut Ctx, kind: Ask, text: &str) -> Result<Answer, Fail> {
    ask_hint(ctx, kind, text, None).await
}

/// [`ask`] with a `hint` line under the question. The report gets only the question.
async fn ask_hint(
    ctx: &mut Ctx,
    kind: Ask,
    text: &str,
    hint: Option<&str>,
) -> Result<Answer, Fail> {
    if !terminal() {
        ctx.answers.push(format!("{text:?}: {NO_TERMINAL}"));
        return Ok(Answer::NoTerminal);
    }
    let keys = keys_text(kind);
    drop_typed_lines(ctx);
    match hint {
        Some(hint) => println!("\n  >>> {text}\n  {hint}\n  {keys}"),
        None => println!("\n  >>> {text}\n  {keys}"),
    }
    let answer = loop {
        let Some(line) = read_line(ctx).await? else {
            break Answer::NoInput;
        };
        match parse_answer(kind, &line) {
            Some(answer) => break answer,
            None => println!("  Unknown answer. {keys}"),
        }
    };
    ctx.answers.push(format!("{text:?}: {answer:?}"));
    Ok(answer)
}

/// The line that says what each key does.
fn keys_text(kind: Ask) -> String {
    match kind {
        Ask::Action { done, skip } => format!("Enter = {done}, s + Enter = {skip}"),
        Ask::Question { yes, no, skip } => {
            format!("y + Enter = {yes}, n + Enter = {no}, s + Enter = {skip}")
        }
    }
}

/// The answer of a typed line. `None` for a line that is not a key of `kind`.
fn parse_answer(kind: Ask, line: &str) -> Option<Answer> {
    match (kind, line.trim().to_ascii_lowercase().as_str()) {
        (_, "s") => Some(Answer::Skip),
        (Ask::Action { .. }, "") => Some(Answer::Done),
        (Ask::Question { .. }, "y") => Some(Answer::Yes),
        (Ask::Question { .. }, "n") => Some(Answer::No),
        _ => None,
    }
}

/// [`need_playback`], after a wait of up to 10 s for the driver to show the playback.
async fn driver_plays(ctx: &mut Ctx, seen: PlaybackSeen) -> Result<(), Fail> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while need_playback(ctx, seen).is_err() && Instant::now() < deadline {
        ctx.client.listen_ms(200).await?;
    }
    need_playback(ctx, seen)
}

/// True if the second connection has an open MRP connection.
fn has_mrp(ctx: &Ctx) -> bool {
    ctx.oracle.as_ref().is_some_and(Oracle::has_mrp)
}

/// What the second connection says is missing for `need`. `None` without it or its MRP.
fn content_missing(ctx: &Ctx, need: Content) -> Option<String> {
    ctx.oracle
        .as_ref()
        .filter(|o| o.has_mrp())
        .and_then(|o| o.missing(need))
}

/// True if stdin is a terminal, so the owner can answer.
pub fn terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Wait up to `secs` until the second connection reports `need`. Returns what is still
/// missing.
async fn wait_content(ctx: &mut Ctx, need: Content, secs: u64) -> Result<Option<String>, Fail> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let missing = content_missing(ctx, need);
        if missing.is_none() || Instant::now() >= deadline {
            return Ok(missing);
        }
        ctx.client.listen_ms(500).await?;
    }
}

/// Remember the content as not started, with `reason`. Keeps the least content.
fn decline(ctx: &mut Ctx, need: Content, reason: String) {
    // Nothing plays or no title: no content can be checked without the owner.
    let need = if content_missing(ctx, Content::Playing).is_some() {
        Content::Playing
    } else {
        need
    };
    if ctx.declined.as_ref().is_none_or(|(d, _)| need < *d) {
        ctx.declined = Some((need, reason));
    }
}

/// Check that the Apple TV plays `need`. If not, open the app that played before when nothing
/// plays or the item has no title, then ask the owner until it is right or the owner skips.
/// When MRP of the second connection shows the content, it waits up to 10 s for the driver.
/// Without the second connection or its MRP, only the driver state counts, at once, see
/// [`need_playback`].
async fn need_content(ctx: &mut Ctx, need: Content, seen: PlaybackSeen) -> Result<(), Fail> {
    if !has_mrp(ctx) {
        return need_playback(ctx, seen);
    }
    let Some(mut missing) = content_missing(ctx, need) else {
        return content_found(ctx, need, seen).await;
    };
    // Content that was not started before: do not ask again.
    if let Some((_, reason)) = ctx.declined.as_ref().filter(|(d, _)| *d <= need) {
        return Err(Fail::skip(reason.clone()));
    }
    // Nothing plays or the item has no title, for example after Settings came to the front:
    // open the app that played.
    if let Some(app) = ctx.played_app.clone()
        && content_missing(ctx, Content::Playing).is_some()
        && has_value(ctx, &ctx.mp(), "source_list", &app)
    {
        mp_command(ctx, "select_source", json!({ "source": app })).await?;
        match wait_content(ctx, need, 10).await? {
            None => return content_found(ctx, need, seen).await,
            Some(still) => missing = still,
        }
    }
    if !terminal() {
        decline(ctx, need, format!("{NO_TERMINAL}, needs {}", need.text()));
        return Err(Fail::skip(format!(
            "{NO_TERMINAL}, needs {}: {missing}",
            need.text()
        )));
    }
    // Ask until the content is right or the owner skips. Each prompt says what is wrong now.
    let mut keys = Ask::Action {
        done: "it plays",
        skip: "skip the checks that need it",
    };
    loop {
        let text = content_prompt(ctx, need, &missing);
        let answer = ask(ctx, keys, &text).await?;
        if answer != Answer::Done {
            let why = match answer {
                Answer::NoInput => "not started (no input)",
                Answer::NoTerminal => NO_TERMINAL,
                Answer::Skip | Answer::Done | Answer::Yes | Answer::No => "the owner skipped",
            };
            let reason = format!("{why}: {}: {missing}", need.text());
            decline(ctx, need, reason.clone());
            return Err(Fail::skip(reason));
        }
        // Time for the new item to load.
        match wait_content(ctx, need, 5).await? {
            None => return content_found(ctx, need, seen).await,
            Some(still) => missing = still,
        }
        keys = Ask::Action {
            done: "check again",
            skip: "skip the checks that need it",
        };
    }
}

/// Prompt for missing content: what plays now, what is missing, and what to open.
fn content_prompt(ctx: &Ctx, need: Content, missing: &str) -> String {
    let start = "in the Apple TV app, start it and wait until it plays.";
    match ctx.oracle.as_ref().and_then(Oracle::now_text) {
        Some(now) => format!(
            "Now playing: {now}. It is not {}: {missing}. {} {start}",
            need.text(),
            need.open()
        ),
        None => format!("Nothing plays. Open {} {start}", need.text()),
    }
}

/// MRP of the second connection shows the content: remember its app, then wait for the driver,
/// see [`driver_plays`].
async fn content_found(ctx: &mut Ctx, need: Content, seen: PlaybackSeen) -> Result<(), Fail> {
    // Only an item that plays: a paused item can be an old item of another app. A paused movie
    // counts: chapters and languages rule out such an old item.
    let app = ctx.oracle.as_ref().and_then(|o| {
        o.active_app()
            .or_else(|| (need == Content::Movie).then(|| o.playing_app()).flatten())
    });
    if let Some(app) = app {
        ctx.played_app = Some(app.to_owned());
    }
    driver_plays(ctx, seen).await.map_err(|_| {
        Fail::skip(format!(
            "the Apple TV plays, the driver shows {} after 10 s",
            mp_state(ctx)
        ))
    })
}

/// The driver config with the pairing, as backup text.
fn backup_text(ctx: &Ctx) -> Result<String, Fail> {
    let text = std::fs::read_to_string(&ctx.opt.config).map_err(|e| {
        Fail::new(format!(
            "{}: {e}. Pair the driver one time with `remote-sim pair-driver <address>`",
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
    mp.trim_start_matches("media_player.")
        .clone_into(&mut ctx.key);
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
    let state = match wait_available(ctx, 30).await {
        Ok(state) => state,
        Err(e)
            if driver_log(&ctx.driver, ctx.driver.log_start())
                .contains("pairing is not valid anymore") =>
        {
            return Err(Fail::new(format!(
                "{e}. The driver pairing in {} is not valid anymore: run `remote-sim \
                 pair-driver <address>`",
                ctx.opt.config.display()
            )));
        }
        Err(e) => return Err(e),
    };
    ctx.was_on = state != "OFF";
    ctx.client.listen(2).await?;
    let Some(address) = ctx.opt.address else {
        return Err(Fail::new("no address"));
    };
    let mark = ctx.driver.log_len();
    let oracle = match Oracle::connect(address, &ctx.opt.credentials).await {
        Ok(oracle) => {
            ctx.oracle = Some(oracle);
            // The second connection must not push the driver out.
            ctx.client.listen(3).await?;
            // Now playing comes some time after the connect.
            ctx.played_app = ctx
                .oracle
                .as_ref()
                .and_then(Oracle::active_app)
                .map(str::to_owned);
            let closed = closed_lines(&ctx.driver, mark);
            ensure!(
                closed.is_empty(),
                "a driver connection closed when the second connection started: {closed}"
            );
            ensure!(
                mp_state(ctx) != "UNAVAILABLE",
                "the media player is UNAVAILABLE after the second connection started"
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

    volume_mismatches(ctx, oracle, &mut out);
    if awake {
        now_playing_mismatches(ctx, oracle, &mut out);
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

/// Compare the volume values.
fn volume_mismatches(ctx: &Ctx, oracle: &Oracle, out: &mut Vec<String>) {
    let c = &ctx.client;
    let mp = ctx.mp();
    let Some(volume) = oracle.volume() else {
        return;
    };
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

/// Compare the title and the app that plays. On the Home Screen with nothing playing, the driver
/// clears an old paused item: an empty title or source is then accepted.
fn now_playing_mismatches(ctx: &Ctx, oracle: &Oracle, out: &mut Vec<String>) {
    let c = &ctx.client;
    let mp = ctx.mp();
    let home = oracle.home_screen();
    let np = oracle.now_playing();
    let shown = c.text(&mp, "media_title");
    // Like the driver: an episode shows its own title, not the show name.
    let title = np
        .subtitle
        .as_deref()
        .filter(|_| np.episode.is_some())
        .or(np.title.as_deref());
    if let Some(title) = title.filter(|t| !t.is_empty())
        && !shown.contains(title)
        && !(home && cleared(c.attr(&mp, "media_title")))
    {
        out.push(format!("media_title {shown:?}, Apple TV {title:?}"));
    }
    if let Some(name) = np.app.as_deref().and_then(|b| oracle.app_name(b)) {
        let source = c.text(&mp, "source");
        if source != name && !(home && cleared(c.attr(&mp, "source"))) {
            out.push(format!("source {source:?}, Apple TV {name:?}"));
        }
    }
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
    ctx.client.send_event("enter_standby").await?;
    ctx.client.listen(3).await?;
    let in_standby = mp_state(ctx);
    ensure!(
        in_standby != "UNAVAILABLE",
        "state in standby: {in_standby}"
    );
    let mark = ctx.driver.log_len();
    ctx.client.send_event("exit_standby").await?;
    let took = wait_joined(ctx, mark, Instant::now(), 8).await?;
    ctx.client.refresh_states().await?;
    let after = wait_available(ctx, 5).await?;
    let mut nothing = "";
    if ctx.oracle.is_some() {
        verify(ctx, 5).await?;
        // `verify` does not see an old title when the Apple TV has none, and accepts the old
        // title on the Home Screen. The driver clears both after a new connection.
        let oracle = ctx.oracle.as_ref();
        if oracle.is_some_and(Oracle::home_screen) {
            let mp = ctx.mp();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let left = item_left(ctx, &mp);
                if left.is_empty() {
                    break;
                }
                home_screen_or_skip(ctx)?;
                ensure!(
                    Instant::now() < deadline,
                    "the Apple TV shows the Home Screen and nothing plays, the driver shows {}{}",
                    left.join(", "),
                    no_check_note(ctx, mark)
                );
                ctx.client.listen_ms(200).await?;
            }
            nothing = ", Home Screen and no old item is shown";
        } else if oracle.and_then(Oracle::has_item) == Some(false) {
            no_old_item(ctx).await?;
            nothing = ", nothing plays and no old item is shown";
        }
    }
    Ok(format!(
        "full connect {took} ms after exit_standby, state {after}{nothing}"
    ))
}

/// A note for a FAIL text when the driver log after `mark` has no Home Screen check at a
/// connect. Empty if it has one.
fn no_check_note(ctx: &Ctx, mark: LogMark) -> &'static str {
    if driver_log(&ctx.driver, mark).contains(NO_PLAYER_CHECK) {
        ""
    } else {
        ", and the driver did not see the Home Screen at the connect"
    }
}

/// True for a cleared attribute: the driver sends "" or null.
fn cleared(v: Option<&Value>) -> bool {
    v.is_none_or(|v| v.is_null() || v.as_str() == Some(""))
}

/// Wait up to 5 s until the driver shows no title and no image, as the Apple TV has no item.
async fn no_old_item(ctx: &mut Ctx) -> Result<(), Fail> {
    let mp = ctx.mp();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let title = ctx.client.attr(&mp, "media_title");
        let image = ctx.client.attr(&mp, "media_image_url");
        if cleared(title) && cleared(image) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "the Apple TV has no item, the driver shows title {:?}, image {:?}",
            ctx.client.text(&mp, "media_title"),
            ctx.client.text(&mp, "media_image_url")
        );
        ctx.client.listen_ms(200).await?;
    }
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
    // No entity ids: all entities. The later checks need the events.
    ctx.client.request_ok("subscribe_events", json!({})).await?;
    Ok(format!(
        "state {first} after {took} ms, subscribed without ids"
    ))
}

/// The waits in ms in [`quick_wake`]: from an `exit_standby` to the next `enter_standby`, and from
/// that `enter_standby` to the next `exit_standby`. On the test device, Companion opens after 30 to
/// 100 ms and MRP joins after 50 to 200 ms. So the waits send the standby before Companion is
/// open, while MRP joins, about when MRP has joined and after the full connect. A wait of 0 after
/// a standby sends the next wake while the old connection still closes.
const QUICK_GAPS_MS: [(u64, u64); 6] = [(0, 0), (15, 0), (40, 0), (80, 10), (200, 0), (500, 500)];

/// Wait in s after the last `enter_standby` of the burst, so the driver has closed its
/// connection before the last `exit_standby`. The driver waits up to 2 s for a running command
/// before it closes.
const QUICK_SETTLE_SECS: u64 = 3;

/// Longest wait in s for the full connect after the last `exit_standby`.
const QUICK_CONNECT_SECS: u64 = 15;

/// Parts of the one driver log line of a new connect: both protocols in one go, or Companion
/// first with MRP next. A protocol that is added later logs "<protocol> connected in" without
/// them.
const CONNECT_LINES: [&str; 2] = ["] connected in ", ", MRP next"];

/// Checks the driver lines after the last `exit_standby` of [`quick_wake`]: exactly one connect,
/// no line at level WARN or ERROR and no closed connection. A failed or partial connect logs at
/// WARN.
fn after_quick_wake(log: &DriverLog) -> Result<(), String> {
    let connects = log.lines_with(&CONNECT_LINES);
    if connects.len() != 1 {
        return Err(format!(
            "{} connects after the last exit_standby, expected 1: {}",
            connects.len(),
            connects.join(" | ")
        ));
    }
    // One pass, so a line that is both is shown once.
    let bad: Vec<&str> = log
        .entries()
        .filter(|l| matches!(l.level, "WARN" | "ERROR") || has_any(l.message, &CLOSED))
        .map(|l| l.line)
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join(" | "))
    }
}

/// The standby and wake events of [`quick_wake`], up to the last standby and the wait after it.
async fn standby_burst(ctx: &mut Ctx) -> Result<(), Fail> {
    remote_sleep(ctx).await?;
    for (awake, asleep) in QUICK_GAPS_MS {
        ctx.client.send_event("exit_standby").await?;
        ctx.client.listen_ms(awake).await?;
        ctx.client.send_event("enter_standby").await?;
        ctx.client.listen_ms(asleep).await?;
    }
    ctx.client.listen(QUICK_SETTLE_SECS).await
}

/// The Remote wakes and goes to standby again with short gaps, so a standby comes while a connect
/// runs and a wake while the old connection closes. After the last wake the driver must connect
/// once, with no driver line at level WARN or ERROR and no closed connection, show the media
/// player as available and agree with the second connection if there is one. If the burst fails,
/// the Remote wakes again.
pub async fn quick_wake(ctx: &mut Ctx) -> Check {
    if let Err(e) = standby_burst(ctx).await {
        // Wake the Remote again, else the next checks run in standby. The first error counts.
        let mark = ctx.driver.log_len();
        if ctx.client.send_event("exit_standby").await.is_ok() {
            let _ = wait_joined(ctx, mark, Instant::now(), QUICK_CONNECT_SECS).await;
        }
        return Err(e);
    }
    let mark = ctx.driver.log_len();
    ctx.client.send_event("exit_standby").await?;
    let took = wait_joined(ctx, mark, Instant::now(), QUICK_CONNECT_SECS).await?;
    // A second connect or a close can come after the first connect.
    ctx.client.listen(2).await?;
    after_quick_wake(&driver_log(&ctx.driver, mark)).map_err(Fail::new)?;
    ctx.client.refresh_states().await?;
    let state = wait_available(ctx, 5).await?;
    let values = if ctx.oracle.is_some() {
        verify(ctx, 5).await?;
        ", values agree with the second connection"
    } else {
        ""
    };
    Ok(format!(
        "{} wakes, full connect {took} ms after the last exit_standby, state {state}{values}",
        QUICK_GAPS_MS.len() + 1
    ))
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
    // No wait: tvOS 27 ignores a wake within about 5 s of the sleep command, and ratv must handle
    // that. A command other than power turns the Apple TV on first.
    let start = Instant::now();
    press(ctx, "HOME").await?;
    ctx.client
        .wait_attr(&mp, "state", 15, |v| v != "OFF")
        .await?;
    let on = start.elapsed().as_millis();
    wait_power(ctx, true, 10).await?;
    mp_command(ctx, "toggle", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 15, "OFF").await?;
    wait_power(ctx, false, 10).await?;
    mp_command(ctx, "toggle", json!({})).await?;
    ctx.client
        .wait_attr(&mp, "state", 15, |v| v != "OFF")
        .await?;
    wait_power(ctx, true, 10).await?;
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
    // Repeat and delay. The answer comes before the run ends.
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
    ensure!(
        repeat < 400,
        "repeat 3 with delay 200 answered after {repeat} ms"
    );
    // A long run stops at `stop_send`.
    // Not "not sent: stopped by stop_send": that is a waiting command, not the running one.
    let stopped = "send_cmd stopped by stop_send";
    let mark = ctx.driver.log_len();
    ctx.client
        .command_ok(
            "remote",
            &remote,
            "send_cmd",
            json!({ "command": "CURSOR_LEFT", "repeat": 20, "delay": 500 }),
        )
        .await?;
    ctx.client.listen_ms(1500).await?;
    ctx.client
        .command_ok("remote", &remote, "stop_send", json!({}))
        .await?;
    wait_log(ctx, mark, stopped, 3).await?;
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
        "repeat 3 answered after {repeat} ms, stop_send stops a run, unknown command gives 400"
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
        .and_then(|o| o.app_name(SETTINGS))
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
    // The app list is device state: the command answers at once and the refusal is logged.
    let refused = "select_option not run";
    let mark = ctx.driver.log_len();
    let code = command_code(
        ctx,
        "select",
        &select,
        "select_option",
        json!({ "option": "Not an app" }),
    )
    .await?;
    ensure!(code == 200, "unknown app: code {code}");
    wait_log(ctx, mark, refused, 10).await?;
    press(ctx, "HOME").await?;
    Ok(format!(
        "{target} shown after {took} ms, unknown app refused in the log"
    ))
}

pub async fn volume(ctx: &mut Ctx) -> Check {
    turn_on(ctx).await?;
    let mp = ctx.mp();
    let control = ctx.client.text(&ctx.sensor("volume_control"), "value");
    if control == "off" {
        // Volume keys go to the Apple TV, which drops them without volume control (CEC or a
        // receiver). There is nothing to check.
        return Err(Fail::skip(
            "no volume control on the Apple TV (CEC or receiver needed)",
        ));
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

/// Which side can show that something plays.
#[derive(Clone, Copy)]
enum PlaybackSeen {
    /// Only the driver state counts.
    Driver,
    /// The driver state, or the second connection when there is one. The driver can clear now
    /// playing on the Home Screen while the Apple TV still has an item.
    DriverOrOracle,
}

fn need_playback(ctx: &Ctx, seen: PlaybackSeen) -> Result<(), Fail> {
    let oracle_plays = matches!(seen, PlaybackSeen::DriverOrOracle)
        && ctx.oracle.as_ref().is_some_and(|o| {
            matches!(
                o.playback_state(),
                Some(
                    PlaybackState::Playing
                        | PlaybackState::Paused
                        | PlaybackState::Interrupted
                        | PlaybackState::Seeking
                )
            )
        });
    if playing(ctx) || oracle_plays {
        Ok(())
    } else {
        Err(Fail::skip(format!(
            "nothing is playing: start {} before the run",
            Content::Movie.text()
        )))
    }
}

/// Wait until the second connection reports the playback state. Compared as media player
/// states, so Paused also accepts Interrupted.
async fn wait_playback(ctx: &mut Ctx, expected: PlaybackState) -> Result<(), Fail> {
    let Some(oracle) = ctx.oracle.as_ref() else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while entity_state(oracle.playback_state()) != entity_state(Some(expected)) {
        ensure!(
            Instant::now() < deadline,
            "the Apple TV is not {expected:?} after 5 s"
        );
        ctx.client.listen_ms(200).await?;
    }
    Ok(())
}

/// Wait until the second connection reports the playback rate.
async fn wait_rate(ctx: &mut Ctx, expected: f32) -> Result<(), Fail> {
    let Some(oracle) = ctx.oracle.as_ref() else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rate = oracle.rate();
        if rate.is_some_and(|r| (r - expected).abs() < 0.01) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Apple TV rate {rate:?} after 5 s, expected {expected}"
        );
        ctx.client.listen_ms(200).await?;
    }
}

/// True if the list attribute `attr` of `entity` has the text `name`.
fn has_value(ctx: &Ctx, entity: &str, attr: &str, name: &str) -> bool {
    ctx.client
        .attr(entity, attr)
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|v| v.as_str() == Some(name)))
}

/// Driver log line when the Home Screen clears now playing.
const HOME_CLEAR: &str = "Home Screen: clearing now playing";
/// Driver log line of the Home Screen check at a connect without a player.
const NO_PLAYER_CHECK: &str = "no player at the connect";
/// Driver log line when now playing shows again after the Home Screen clear.
const HOME_LEFT: &str = "now playing again after the Home Screen";
/// Driver log line when the connection closes during the Home Screen clear.
const HOME_CLOSED: &str = "Home Screen clear ended: connection closed";

/// True if the last Home Screen clear in the driver log since the driver start was not ended.
fn home_clear_active(ctx: &Ctx) -> bool {
    let log = driver_log(&ctx.driver, ctx.driver.log_start());
    let Some(clear) = log.last_with(&[HOME_CLEAR]) else {
        return false;
    };
    let ended = log.last_with(&[HOME_LEFT, HOME_CLOSED]);
    ended.is_none_or(|end| clear > end)
}

/// Open the app that plays, so it is in front and not the Home Screen. Waits until the driver
/// shows the app and the same state as the second connection. Returns a note, or a skip when
/// there is no app, the driver names an app that is not in its source list, the source list is
/// empty, the app stopped playback, the Apple TV stays in BUFFERING, or the driver keeps the
/// Home Screen clear. An app from the Apple TV that is not in a non-empty source list is a FAIL.
/// `standby` is true if the driver state was STANDBY or OFF at the start of the check.
///
/// The Home Screen skip needs STANDBY or OFF at the start and a clear that had not ended before
/// the app was opened and is still active at the time limit. A clear after the app was opened
/// is a FAIL.
///
/// When the Apple TV has no item, it skips if the driver shows no title and no image. A title
/// or image on the driver after 3 seconds is a FAIL.
///
/// `app` is the app to open, for example the app that played before HOME. `None` takes the app
/// of now playing. A given app that is not in the source list is a skip.
async fn bring_to_front(ctx: &mut Ctx, standby: bool, app: Option<String>) -> Result<String, Fail> {
    let mp = ctx.mp();
    let app = front_app(ctx, &mp, app)?;
    let title = ctx.client.text(&mp, "media_title");
    let clear_at_start = standby && home_clear_active(ctx);
    let front_mark = ctx.driver.log_len();
    mp_command(ctx, "select_source", json!({ "source": app })).await?;
    ctx.client.listen(1).await?;
    ctx.client.wait_text(&mp, "source", 10, &app).await?;
    // The steps below use the state after the app was opened.
    wait_front_state(ctx, &app, clear_at_start, front_mark).await?;
    // Some apps close a paused player when opened: the state stays, the item is gone. Only the
    // second connection tells this apart. An empty title on the driver can also be an app that
    // sends no title, or a driver error that this check must find.
    if ctx.oracle.as_ref().and_then(Oracle::has_item) == Some(false) {
        wait_item_cleared(ctx, &mp, &app, &title).await?;
    }
    let now = ctx.client.text(&mp, "media_title");
    Ok(if now == title {
        format!("{app} in front")
    } else if title.is_empty() {
        format!("{app} in front, item {now:?}")
    } else {
        format!("{app} in front, item changed from {title:?} to {now:?}")
    })
}

/// The app for [`bring_to_front`]: `app`, else the app of now playing, else the source of the
/// driver. An app that is not in the source list is a skip, or a FAIL when only the Apple TV
/// names it and the list has entries.
fn front_app(ctx: &Ctx, mp: &str, app: Option<String>) -> Result<String, Fail> {
    let device_app = app.clone().or_else(|| {
        ctx.oracle
            .as_ref()
            .and_then(Oracle::playing_app)
            .map(str::to_owned)
    });
    let from_driver = device_app.is_none() || app.is_some();
    let app = device_app.unwrap_or_else(|| ctx.client.text(mp, "source"));
    if app.is_empty() {
        return Err(Fail::skip(
            "no app to open: the Apple TV and the driver name no app that plays",
        ));
    }
    if has_value(ctx, mp, "source_list", &app) {
        return Ok(app);
    }
    let empty = ctx
        .client
        .attr(mp, "source_list")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if empty || from_driver {
        return Err(Fail::skip(format!(
            "no app to open: {app:?} is not in the source list"
        )));
    }
    Err(Fail::new(format!(
        "{app} plays on the Apple TV but is not in the driver's source list"
    )))
}

/// Waits until the states agree on PLAYING or PAUSED after `app` was opened. Opening the app can
/// restart the item, for example a live channel. A scan rate is set back to 1x once.
async fn wait_front_state(
    ctx: &mut Ctx,
    app: &str,
    clear_at_start: bool,
    front_mark: LogMark,
) -> Result<(), Fail> {
    let total = Duration::from_secs(15);
    let start = Instant::now();
    let deadline = start + total;
    let mut rate_reset = false;
    let hold = |s: &str| Duration::from_secs(if s == "STANDBY" { 5 } else { 1 });
    loop {
        let state = match wait_agree(ctx, start, total, hold, NoOracle::AnyStable).await {
            Ok(state) => state,
            Err(AgreeError::Other(e)) => return Err(e),
            Err(AgreeError::Timeout(e)) => {
                return Err(front_timeout(ctx, app, clear_at_start, front_mark, &e));
            }
        };
        match state.as_str() {
            "PLAYING" | "PAUSED" => return Ok(()),
            "STANDBY" => return Err(Fail::skip("app stopped playback when opened")),
            // BUFFERING: a scan rate, or the item loads.
            "BUFFERING" => {
                if Instant::now() >= deadline {
                    return Err(Fail::skip("Apple TV stays in BUFFERING"));
                }
                if !rate_reset && scan_rate(ctx) {
                    rate_reset = true;
                    speed_to_normal(ctx).await?;
                }
            }
            _ => return Err(Fail::new(format!("{app} in front: state {state:?}"))),
        }
    }
}

/// Result of [`wait_front_state`] when the states did not agree in time.
fn front_timeout(
    ctx: &Ctx,
    app: &str,
    clear_at_start: bool,
    front_mark: LogMark,
    e: &Fail,
) -> Fail {
    let device = ctx.oracle.as_ref().map(Oracle::playback_state);
    let shown = mp_state(ctx);
    if shown == "BUFFERING" && device.is_none_or(|d| d == Some(PlaybackState::Seeking)) {
        return Fail::skip("Apple TV stays in BUFFERING");
    }
    // The Home Screen clear stays until an app has media controls or now playing changes.
    let paused = device
        .is_some_and(|d| matches!(d, Some(PlaybackState::Paused | PlaybackState::Interrupted)));
    if shown == "STANDBY" && paused {
        if driver_log(&ctx.driver, front_mark).contains(HOME_CLEAR) {
            return Fail::new(format!("Home Screen clear fired with {app} in front"));
        }
        if clear_at_start && home_clear_active(ctx) {
            return Fail::skip(format!(
                "driver keeps the Home Screen clear: driver {shown}, Apple TV {}",
                device_text(device)
            ));
        }
    }
    Fail::new(format!("{app} in front: {}", e.msg))
}

/// The Apple TV plays at a rate other than 1x, for example while it scans forward.
fn scan_rate(ctx: &Ctx) -> bool {
    ctx.oracle
        .as_ref()
        .and_then(Oracle::rate)
        .is_some_and(|r| r > 0.0 && (r - 1.0).abs() >= 0.01)
}

/// Selects the 1x playback speed, if the driver offers it.
async fn speed_to_normal(ctx: &mut Ctx) -> Result<(), Fail> {
    let speed = ctx.select("playback_speed");
    if has_value(ctx, &speed, "options", "1x") {
        ctx.client
            .command_ok("select", &speed, "select_option", json!({ "option": "1x" }))
            .await?;
    }
    Ok(())
}

/// The Apple TV has no item: the driver must clear the title and the image within 3 seconds.
/// It sends "" for both. A cleared driver is a skip, because the check cannot go on.
async fn wait_item_cleared(ctx: &mut Ctx, mp: &str, app: &str, title: &str) -> Result<(), Fail> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // The Apple TV can send the item again: then check it as usual.
        if ctx.oracle.as_ref().and_then(Oracle::has_item) != Some(false) {
            return Ok(());
        }
        let no_title = cleared(ctx.client.attr(mp, "media_title"));
        let no_image = cleared(ctx.client.attr(mp, "media_image_url"));
        if no_title && no_image {
            return Err(Fail::skip(if title.is_empty() {
                format!("{app} has no item")
            } else {
                format!("{app} closed the player when opened")
            }));
        }
        if Instant::now() >= deadline {
            if !no_title {
                let shown = ctx.client.text(mp, "media_title");
                return Err(Fail::new(format!(
                    "driver shows {shown:?}, Apple TV has no item"
                )));
            }
            return Err(Fail::new("driver shows an image, Apple TV has no item"));
        }
        ctx.client.listen_ms(200).await?;
    }
}

/// Media player state for a playback state of the Apple TV. Copy of the playback part of
/// `state` in `src/entities/media_player.rs` of the driver.
fn entity_state(state: Option<PlaybackState>) -> &'static str {
    match state {
        Some(PlaybackState::Playing) => "PLAYING",
        Some(PlaybackState::Paused | PlaybackState::Interrupted) => "PAUSED",
        Some(PlaybackState::Seeking) => "BUFFERING",
        _ => "STANDBY",
    }
}

/// Text for the state of the second connection: `None` when there is no second connection.
fn device_text(device: Option<Option<PlaybackState>>) -> String {
    match device {
        None => "no oracle".into(),
        Some(None) => "oracle reports nothing".into(),
        Some(Some(state)) => format!("{state:?}"),
    }
}

/// What `wait_agree` accepts without the second connection.
#[derive(Clone, Copy)]
enum NoOracle {
    /// Any driver state that does not change.
    AnyStable,
    /// Only PLAYING.
    Playing,
}

/// Error of `wait_agree`.
enum AgreeError {
    /// The states did not agree in time.
    Timeout(Fail),
    /// Another error, for example the connection closed.
    Other(Fail),
}

impl From<Fail> for AgreeError {
    fn from(e: Fail) -> Self {
        Self::Other(e)
    }
}

impl AgreeError {
    /// The error with `prefix` before the message.
    fn prefixed(self, prefix: &str) -> Fail {
        let (Self::Timeout(e) | Self::Other(e)) = self;
        Fail {
            msg: format!("{prefix}: {}", e.msg),
            skip: e.skip,
        }
    }
}

/// Wait until the driver shows the playback state of the second connection for `hold(state)`.
/// Returns the state. Times out `total` after `start`.
async fn wait_agree(
    ctx: &mut Ctx,
    start: Instant,
    total: Duration,
    hold: impl Fn(&str) -> Duration,
    no_oracle: NoOracle,
) -> Result<String, AgreeError> {
    let deadline = start + total;
    let mut agreed: Option<(String, Instant)> = None;
    loop {
        let shown = mp_state(ctx);
        let device = ctx.oracle.as_ref().map(Oracle::playback_state);
        let expected = match device {
            Some(d) => entity_state(d),
            None => match no_oracle {
                NoOracle::AnyStable => shown.as_str(),
                NoOracle::Playing => "PLAYING",
            },
        };
        if shown == expected {
            if agreed.as_ref().is_none_or(|(s, _)| *s != shown) {
                agreed = Some((shown.clone(), Instant::now()));
            }
            if agreed
                .as_ref()
                .is_some_and(|(_, t)| t.elapsed() >= hold(&shown))
            {
                return Ok(shown);
            }
        } else {
            agreed = None;
        }
        if Instant::now() >= deadline {
            return Err(AgreeError::Timeout(Fail::new(format!(
                "driver state {shown}, expected {expected} ({}): not the same long enough in {} s",
                device_text(device),
                total.as_secs()
            ))));
        }
        ctx.client.listen_ms(200).await?;
    }
}

fn position(ctx: &Ctx) -> Option<f64> {
    ctx.client
        .attr(&ctx.mp(), "media_position")
        .and_then(Value::as_f64)
}

/// Wait after a play state change before the next PLAY, PAUSE or play_pause. The Apple TV can
/// ignore such a command right after a change.
const TOGGLE_SETTLE_MS: u64 = 700;

pub async fn playback(ctx: &mut Ctx) -> Check {
    need_content(ctx, Content::Movie, PlaybackSeen::DriverOrOracle).await?;
    let standby = matches!(mp_state(ctx).as_str(), "STANDBY" | "OFF");
    let mut notes = vec![bring_to_front(ctx, standby, None).await?];
    let mp = ctx.mp();
    mp_command(ctx, "PAUSE", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PAUSED").await?;
    wait_playback(ctx, PlaybackState::Paused).await?;
    ctx.client.listen_ms(TOGGLE_SETTLE_MS).await?;
    mp_command(ctx, "PLAY", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PLAYING").await?;
    wait_playback(ctx, PlaybackState::Playing).await?;
    ctx.client.listen_ms(TOGGLE_SETTLE_MS).await?;
    mp_command(ctx, "play_pause", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PAUSED").await?;
    wait_playback(ctx, PlaybackState::Paused).await?;
    ctx.client.listen_ms(TOGGLE_SETTLE_MS).await?;
    mp_command(ctx, "play_pause", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PLAYING").await?;
    wait_playback(ctx, PlaybackState::Playing).await?;

    let duration = ctx
        .client
        .attr(&mp, "media_duration")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let live = ctx.oracle.as_ref().is_some_and(Oracle::is_live);
    if live {
        notes.push("seek not tested: the Apple TV reports live content".into());
    } else if duration <= 0.0 {
        notes.push("seek not tested: no duration".into());
    } else if duration <= 120.0 {
        notes.push(format!(
            "seek not tested: duration {duration:.0} s, needs more than 120 s"
        ));
    } else if let Some(start) = position(ctx) {
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
        notes.push("seek not tested: no position".into());
    }

    let mut agreed = None;
    if live {
        notes.push("fast forward not tested: the Apple TV reports live content".into());
    } else if duration <= 0.0 {
        // An item without a duration can be live: the Apple TV can stop fast forward and pause.
        notes.push("fast forward not tested: no duration".into());
    } else {
        // Fast forward runs until the end command.
        mp_command(ctx, "FAST_FORWARD_BEGIN", json!({})).await?;
        ctx.client.listen(2).await?;
        mp_command(ctx, "FAST_FORWARD_END", json!({})).await?;
        ctx.client.listen(1).await?;
        let total = Duration::from_secs(5);
        let after = wait_agree(
            ctx,
            Instant::now(),
            total,
            |_| Duration::from_secs(1),
            NoOracle::Playing,
        )
        .await
        .map_err(|e| e.prefixed("after fast forward"))?;
        notes.push(format!("state after fast forward {after}"));
        agreed = Some(after);
    }

    // The speed step needs playback. First the states must agree (known after fast forward).
    // Then send PLAY, not play_pause: a toggle could pause.
    let state = match agreed {
        Some(state) => state,
        None => {
            let total = Duration::from_secs(5);
            wait_agree(
                ctx,
                Instant::now(),
                total,
                |_| Duration::from_secs(1),
                NoOracle::AnyStable,
            )
            .await
            .map_err(|e| e.prefixed("before the speed step"))?
        }
    };
    let plays = match state.as_str() {
        "PLAYING" => true,
        "STANDBY" => false,
        "PAUSED" | "BUFFERING" => {
            mp_command(ctx, "PLAY", json!({})).await?;
            let played = match ctx.client.wait_text(&mp, "state", 5, "PLAYING").await {
                Ok(_) => wait_playback(ctx, PlaybackState::Playing).await,
                Err(e) => Err(e),
            };
            played.map_err(|e| Fail::new(format!("PLAY before the speed step: {}", e.msg)))?;
            notes.push("PLAY before the speed step".into());
            true
        }
        _ => return Err(Fail::new(format!("before the speed step: state {state:?}"))),
    };

    let speed = ctx.select("playback_speed");
    // Options are rates such as "1.5x". Find 1x and the highest rate above 1x, else the first
    // other rate above 0.
    let mut has_1x = false;
    let mut found: Option<(&str, f32)> = None;
    let options = ctx.client.attr(&speed, "options").and_then(Value::as_array);
    for o in options.into_iter().flatten().filter_map(Value::as_str) {
        if o == "1x" {
            has_1x = true;
        } else if let Some(rate) = o.strip_suffix('x').and_then(|r| r.parse::<f32>().ok())
            && rate > 0.0
            && found.is_none_or(|(_, f)| (f <= 1.0 && rate > 1.0) || (f > 1.0 && rate > f))
        {
            found = Some((o, rate));
        }
    }
    let other = if !plays {
        notes.push("speed not tested: nothing plays".into());
        None
    } else if !has_1x {
        notes.push("speed not tested: no 1x option".into());
        None
    } else if found.is_none() {
        notes.push("speed not tested: no speed other than 1x".into());
        None
    } else {
        found.map(|(o, rate)| (o.to_owned(), rate))
    };
    if let Some((other, rate)) = other {
        let tried = async {
            ctx.client
                .command_ok(
                    "select",
                    &speed,
                    "select_option",
                    json!({ "option": other }),
                )
                .await?;
            ctx.client
                .wait_text(&speed, "current_option", 5, &other)
                .await?;
            wait_rate(ctx, rate).await?;
            // A listed rate above 1x is a playback speed, not a scan: PLAYING, not BUFFERING.
            if rate > 1.0 {
                let shown = wait_agree(
                    ctx,
                    Instant::now(),
                    Duration::from_secs(5),
                    |_| Duration::from_secs(2),
                    NoOracle::Playing,
                )
                .await
                .map_err(|e| e.prefixed(&format!("at {other}")))?;
                ensure!(
                    shown == "PLAYING",
                    "state {shown} at {other}, expected PLAYING"
                );
            }
            Ok(())
        }
        .await;
        // Always go back to 1x, also after an error. The first error is returned.
        let reset = async {
            ctx.client
                .command_ok("select", &speed, "select_option", json!({ "option": "1x" }))
                .await?;
            ctx.client
                .wait_text(&speed, "current_option", 5, "1x")
                .await?;
            wait_rate(ctx, 1.0).await
        }
        .await;
        tried?;
        reset?;
        notes.push(format!("speed {other} and back"));
        if rate <= 1.0 {
            notes.push("PLAYING above 1x not tested: no option above 1x".into());
        }
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
    need_content(ctx, Content::Movie, PlaybackSeen::Driver).await?;
    let swept_audio = sweep(ctx, "audio_language").await?;
    // The Apple TV lists Auto only while it is current, so a sweep cannot set it back.
    let auto = ctx.client.text(&ctx.select("subtitles"), "current_option") == "Auto";
    let subtitles = if auto {
        0
    } else {
        sweep(ctx, "subtitles").await?
    };
    let subtitles_id = ctx.select("subtitles");
    wait_subtitles_sensor(ctx, &subtitles_id).await?;
    let (audio, shown_subtitles) = language_counts(ctx, &subtitles_id).await?;
    // Languages that came after the sweep were not tested.
    if !auto && subtitles == 0 && shown_subtitles > 0 {
        return Err(Fail::skip("subtitles came after the sweep"));
    }
    if swept_audio < 2 && audio >= 2 {
        return Err(Fail::skip("audio languages came after the sweep"));
    }
    // Use the sweep result (0 or at least 2), also when the item changed after the sweep.
    if auto && swept_audio == 0 {
        return Err(Fail::skip("subtitles are on Auto and were not swept"));
    }
    if !auto && swept_audio == 0 && subtitles == 0 {
        let no_item = match ctx.oracle.as_ref().and_then(Oracle::has_item) {
            Some(item) => !item,
            None => ctx.client.text(&ctx.mp(), "media_title").is_empty(),
        };
        return Err(Fail::skip(if audio == 1 {
            "the video has one audio language and no subtitles"
        } else if no_item {
            "no item plays"
        } else {
            "no audio or subtitle languages"
        }));
    }
    // The sweep gives 0 for a single option, so show the option count then.
    let tested = if swept_audio >= 2 { swept_audio } else { audio };
    let s = plural(tested);
    let audio_note = if swept_audio >= 2 {
        format!("{tested} audio language{s}")
    } else {
        format!("audio not swept: {tested} language{s}")
    };
    Ok(if auto {
        format!("{audio_note}, subtitles on Auto and not swept")
    } else {
        format!("{audio_note}, {subtitles} subtitle options")
    })
}

/// Waits until the subtitles sensor shows the option of the subtitles select. The driver sends
/// each entity in its own message, the select first. The sensor can be one message behind.
async fn wait_subtitles_sensor(ctx: &mut Ctx, subtitles_id: &str) -> Result<(), Fail> {
    let sensor_id = ctx.sensor("subtitles");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let sensor = ctx.client.text(&sensor_id, "value");
        let select = ctx.client.text(subtitles_id, "current_option");
        if sensor == select {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "subtitles sensor {sensor:?}, select {select:?}"
        );
        ctx.client.listen_ms(200).await?;
    }
}

/// Option counts of the audio and subtitles selects, checked against the Apple TV.
async fn language_counts(ctx: &mut Ctx, subtitles_id: &str) -> Result<(usize, usize), Fail> {
    // The driver has one audio option for each audio language of the Apple TV. The subtitles
    // select has no options without subtitles, else "Off" and one option for each subtitle (the
    // Auto entry is renamed, not added). A driver that is behind shows other counts for a short
    // time, so wait until the counts match.
    let audio_id = ctx.select("audio_language");
    let count = |c: &Client, id: &str| {
        c.attr(id, "options")
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    let expected_subtitles = |n: usize| if n == 0 { 0 } else { n + 1 };
    let deadline = Instant::now() + Duration::from_secs(3);
    let (device, audio, shown_subtitles) = loop {
        let device = ctx.oracle.as_ref().and_then(Oracle::language_counts);
        let audio = count(&ctx.client, &audio_id);
        let shown_subtitles = count(&ctx.client, subtitles_id);
        let ok = device.is_none_or(|(a, s)| audio == a && shown_subtitles == expected_subtitles(s));
        if ok || Instant::now() >= deadline {
            break (device, audio, shown_subtitles);
        }
        ctx.client.listen_ms(200).await?;
    };
    if let Some((device_audio, device_subtitles)) = device {
        let s = plural(device_audio);
        ensure!(
            device_audio == audio,
            "Apple TV has {device_audio} audio language{s}, driver {audio} options"
        );
        let expected = expected_subtitles(device_subtitles);
        let s = plural(device_subtitles);
        ensure!(
            shown_subtitles == expected,
            "Apple TV has {device_subtitles} subtitle language{s}, driver {shown_subtitles} \
             options, expected {expected}"
        );
    }
    Ok((audio, shown_subtitles))
}

/// "s" for a count other than 1.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
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

/// Read an artwork URL of the driver. Returns a note. A URL of the app is not read.
async fn fetch_artwork(ctx: &Ctx, url: &str) -> Result<String, Fail> {
    let own = format!("http://127.0.0.1:{}/", ctx.driver.port);
    if !url.starts_with(&own) {
        return Ok(format!("URL of the app: {url}"));
    }
    let (head, size) = http_get(url).await?;
    let lower = head.to_ascii_lowercase();
    ensure!(head.contains(" 200 "), "artwork: {head}");
    ensure!(
        lower.contains("content-type: image/"),
        "artwork type: {head}"
    );
    ensure!(size > 0, "empty artwork");
    Ok(format!("image of {size} bytes from the driver"))
}

/// The image URL of the media player. It waits up to 5 s for it if `wait` is true or the second
/// connection has MRP: the driver can need a short time for the image of a new item. Empty if
/// there is none.
async fn wait_image(ctx: &mut Ctx, wait: bool) -> Result<String, Fail> {
    let mp = ctx.mp();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let url = ctx.client.text(&mp, "media_image_url");
        if !url.is_empty() || !(wait || has_mrp(ctx)) || Instant::now() >= deadline {
            return Ok(url);
        }
        ctx.client.listen_ms(200).await?;
    }
}

pub async fn artwork(ctx: &mut Ctx) -> Check {
    need_content(ctx, Content::Video, PlaybackSeen::Driver).await?;
    let url = wait_image(ctx, false).await?;
    if url.is_empty() {
        ensure!(
            !has_mrp(ctx),
            "the Apple TV has artwork, the driver shows no image after 5 s"
        );
        return Err(Fail::skip("no artwork"));
    }
    let note = fetch_artwork(ctx, &url).await?;
    let own = format!("http://127.0.0.1:{}/", ctx.driver.port);
    if url.starts_with(&own) {
        let (missing, _) = http_get(&format!("{own}artwork/{}/missing.png", ctx.key)).await?;
        ensure!(missing.contains(" 404 "), "unknown artwork: {missing}");
    }
    Ok(note)
}

// ---------------------------------------------------------------- at wake

/// Parts of the driver log lines of a full connect: both protocols in one go, or MRP after
/// Companion.
const FULL_CONNECT: [&str; 2] = ["] connected in ", "MRP connected in"];

/// True if a driver line of `log` shows a full connect.
fn joined(log: &DriverLog) -> bool {
    log.has_any(&FULL_CONNECT)
}

/// Send `enter_standby` like a Remote that sleeps. The driver closes its connection to the Apple
/// TV.
async fn remote_sleep(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.client.send_event("enter_standby").await?;
    ctx.client.listen(3).await
}

/// Wait until the driver log after `mark` shows a full connect. Returns the ms since `start`.
async fn wait_joined(
    ctx: &mut Ctx,
    mark: LogMark,
    start: Instant,
    secs: u64,
) -> Result<u128, Fail> {
    let deadline = start + Duration::from_secs(secs);
    loop {
        let log = driver_log(&ctx.driver, mark);
        if joined(&log) {
            return Ok(start.elapsed().as_millis());
        }
        if Instant::now() >= deadline {
            let mut msg = format!("no full connect {secs} s after exit_standby");
            for line in log.lines_with(&["did not connect"]) {
                msg.push_str(" | ");
                msg.push_str(line);
            }
            return Err(Fail::new(msg));
        }
        ctx.client.listen_ms(100).await?;
    }
}

/// Wait until the driver and the second connection agree on PLAYING or PAUSED. Returns the
/// state. Skips for another state.
async fn stable_playback(ctx: &mut Ctx) -> Result<String, Fail> {
    let state = wait_agree(
        ctx,
        Instant::now(),
        Duration::from_secs(10),
        |_| Duration::from_secs(1),
        NoOracle::AnyStable,
    )
    .await
    .map_err(|e| e.prefixed("before the check"))?;
    match state.as_str() {
        "PLAYING" | "PAUSED" => Ok(state),
        _ => Err(Fail::skip(format!("nothing plays: state {state}"))),
    }
}

/// Playback state of the Apple TV for a media player state.
fn playback_of(state: &str) -> PlaybackState {
    if state == "PLAYING" {
        PlaybackState::Playing
    } else {
        PlaybackState::Paused
    }
}

/// Send PLAY or PAUSE until the driver and the Apple TV show `state`.
async fn set_play_state(ctx: &mut Ctx, state: &str) -> Result<(), Fail> {
    let mp = ctx.mp();
    let cmd = if state == "PLAYING" { "PLAY" } else { "PAUSE" };
    mp_command(ctx, cmd, json!({})).await?;
    ctx.client.wait_text(&mp, "state", 10, state).await?;
    wait_playback(ctx, playback_of(state)).await
}

/// Driver log line when play_pause is sent again after a closed connection.
const SENT_AGAIN: &str = "play_pause not sent, sending again";

/// Make sure the video plays: send PLAY if needed, then wait until the driver and the second
/// connection show PLAYING for 3 s. Skips if it does not play.
async fn ensure_playing(ctx: &mut Ctx) -> Result<(), Fail> {
    if stable_playback(ctx).await? != "PLAYING" {
        set_play_state(ctx, "PLAYING")
            .await
            .map_err(|e| Fail::skip(format!("the video does not play: {}", e.msg)))?;
    }
    let state = wait_agree(
        ctx,
        Instant::now(),
        Duration::from_secs(10),
        |_| Duration::from_secs(3),
        NoOracle::Playing,
    )
    .await
    .map_err(|e| {
        Fail::skip(format!(
            "the video does not play: {}",
            e.prefixed("PLAY").msg
        ))
    })?;
    if state != "PLAYING" {
        return Err(Fail::skip(format!(
            "the video does not play: state {state}"
        )));
    }
    Ok(())
}

/// play_pause right after `exit_standby` runs once, after the connect. The driver must not clear
/// now playing as if the Home Screen showed.
pub async fn wake_command(ctx: &mut Ctx) -> Check {
    need_content(ctx, Content::Playing, PlaybackSeen::Driver).await?;
    let was = stable_playback(ctx).await?;
    let result = wake_toggle(ctx).await;
    // Set the play state from before the check back, on all paths. The first error is returned.
    let restore = set_play_state(ctx, &was).await;
    let note = result?;
    restore?;
    Ok(note)
}

/// The part of [`wake_command`] after the content check. It leaves the video paused.
async fn wake_toggle(ctx: &mut Ctx) -> Check {
    // A paused video can pause again after PLAY, for example without a player on the screen:
    // start from PLAYING.
    ensure_playing(ctx).await?;
    let before = "PLAYING";
    let toggled = "PAUSED";
    remote_sleep(ctx).await?;
    let mark = ctx.driver.log_len();
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    // Like a Remote that wakes on a key press: no wait for the connect.
    mp_command(ctx, "play_pause", json!({})).await?;
    let took = wait_joined(ctx, mark, start, 15).await?;
    ctx.client.refresh_states().await?;
    let mp = ctx.mp();
    ctx.client.wait_text(&mp, "state", 10, toggled).await?;
    wait_playback(ctx, playback_of(toggled)).await?;
    // A second toggle comes within a short time.
    ctx.client.listen(3).await?;
    let log = driver_log(&ctx.driver, mark);
    let sent = log.count(&["] play_pause -> "]);
    let again = log.contains(SENT_AGAIN);
    ensure!(
        sent == 1 || (sent == 2 && again),
        "{sent} 'play_pause ->' lines in the log, expected 1"
    );
    // The first try logs "play_pause failed" before it is sent again: that line is allowed.
    let base = ["NotConnected", "did not connect", "play_pause not sent:"];
    let split = log.first_with(&[SENT_AGAIN]).unwrap_or(0);
    let (head, tail) = (log.lines_before(split), log.lines_from(split));
    let mut bad: Vec<&str> = head
        .matches(&base)
        .filter(|l| !l.message.contains("play_pause failed:"))
        .map(|l| l.line)
        .collect();
    bad.extend(tail.lines_with(&base));
    bad.extend(tail.lines_with(&["play_pause failed"]));
    ensure!(bad.is_empty(), "{}", bad.join(" | "));
    ensure!(
        !log.contains(HOME_CLEAR),
        "the driver cleared now playing as if the Home Screen showed"
    );
    let shown = mp_state(ctx);
    ensure!(
        shown == toggled,
        "state {shown} 3 s later, expected {toggled}"
    );
    if let Some(o) = ctx.oracle.as_ref() {
        let device = o.playback_state();
        ensure!(
            entity_state(device) == toggled,
            "the Apple TV is {} 3 s later, expected {toggled}",
            device_text(Some(device))
        );
    }
    let again = if again {
        ", sent again after a closed connection"
    } else {
        ""
    };
    Ok(format!(
        "{before} -> {toggled} once, connected after {took} ms{again}"
    ))
}

/// The image shows again after a standby and wake.
pub async fn wake_artwork(ctx: &mut Ctx) -> Check {
    need_content(ctx, Content::Video, PlaybackSeen::Driver).await?;
    stable_playback(ctx).await?;
    let mp = ctx.mp();
    let before = wait_image(ctx, false).await?;
    if before.is_empty() {
        ensure!(
            !has_mrp(ctx),
            "the Apple TV has artwork, the driver shows no image after 5 s"
        );
        return Err(Fail::skip("no artwork before the standby"));
    }
    remote_sleep(ctx).await?;
    let mark = ctx.driver.log_len();
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    let took = wait_joined(ctx, mark, start, 15).await?;
    // The driver keeps the entity states over the standby. Read them again, so only what the
    // driver shows after the connect counts, not the cache of this client.
    ctx.client.forget_states();
    ctx.client.refresh_states().await?;
    // An image showed before the standby, so one is expected now.
    let url = wait_image(ctx, true).await?;
    ensure!(!url.is_empty(), "no image 5 s after the connect");
    let shown = start.elapsed().as_millis();
    fetch_artwork(ctx, &url).await?;
    // An artwork error or a new image can come after the first image.
    ctx.client.listen(2).await?;
    let log = driver_log(&ctx.driver, mark);
    let failed = log.lines_with(&["artwork failed"]);
    ensure!(failed.is_empty(), "{}", failed.join(" | "));
    let after = ctx.client.text(&mp, "media_image_url");
    ensure!(!after.is_empty(), "the image went away 2 s after it showed");
    let note = fetch_artwork(ctx, &after).await?;
    let later = if log.contains("is fetched when MRP connects") {
        ", fetched after the MRP join"
    } else {
        ""
    };
    // The driver keeps the URL from before the standby when the item did not change.
    let image = if after == before && url == before {
        "the URL from before the standby is kept".to_owned()
    } else {
        format!("new URL after {shown} ms")
    };
    Ok(format!("connected after {took} ms, {image}{later}, {note}"))
}

/// Set the rate on the Apple TV, like the speed menu of the player. If tvOS refuses
/// (`CommandFailed`) or ignores it, ask the owner to choose it on the TV. A closed second
/// connection or a rate that the app does not list (ratv does not send it) skips. Other errors
/// fail. `skip` is the text for the s key of that prompt.
/// Returns how the rate was set.
async fn set_device_rate(
    ctx: &mut Ctx,
    rate: f32,
    skip: &'static str,
) -> Result<&'static str, Fail> {
    let refused = match oracle(ctx)?.set_rate(rate).await {
        Ok(()) => match wait_rate(ctx, rate).await {
            Ok(()) => return Ok("set by the second connection"),
            Err(e) => {
                let now = ctx.oracle.as_ref().and_then(Oracle::rate);
                match now.filter(|r| (r - 1.0).abs() >= 0.01) {
                    // Another rate, for example a scan: end it with PLAY before the owner acts.
                    Some(other) => {
                        mp_command(ctx, "PLAY", json!({})).await?;
                        let _ = wait_rate(ctx, 1.0).await;
                        format!("tvOS set rate {other} instead of {rate}")
                    }
                    None => format!("tvOS ignored rate {rate}: {}", e.msg),
                }
            }
        },
        Err(e @ ratv::Error::CommandFailed { .. }) => format!("tvOS refused rate {rate}: {e}"),
        Err(ratv::Error::InvalidArgument(_)) => {
            return Err(Fail::skip(format!("rate {rate} is not listed by the app")));
        }
        // The MRP connection of the second connection closed: not an error of the driver.
        Err(
            e @ (ratv::Error::Disconnected | ratv::Error::Closed | ratv::Error::NotConnected(_)),
        ) => {
            return Err(Fail::skip(format!("rate {rate}: {e}")));
        }
        Err(e) => return Err(Fail::new(format!("rate {rate}: {e}"))),
    };
    let text = format!("{refused}. Choose {rate}x in the speed menu of the player on the TV.");
    match ask(ctx, Ask::Action { done: "done", skip }, &text).await? {
        Answer::Done => {}
        Answer::NoTerminal => {
            return Err(Fail::skip(format!("{refused}, {NO_TERMINAL}")));
        }
        Answer::NoInput => return Err(Fail::skip(format!("{refused} (no input)"))),
        Answer::Skip | Answer::Yes | Answer::No => return Err(Fail::skip(refused)),
    }
    wait_rate(ctx, rate)
        .await
        .map_err(|e| Fail::skip(format!("{refused}. Set on the TV: {}", e.msg)))?;
    Ok("set on the TV")
}

/// Speeds set on the Apple TV: 1.5x shows PLAYING, 2x is shown as information.
pub async fn speed(ctx: &mut Ctx) -> Check {
    oracle(ctx)?;
    // ratv sets the rate over MRP.
    if !has_mrp(ctx) {
        return Err(Fail::skip("the second connection has no MRP"));
    }
    need_content(ctx, Content::Playing, PlaybackSeen::Driver).await?;
    let before = stable_playback(ctx).await?;
    let result = speeds(ctx, before == "PAUSED").await;
    // Set the play state back on all paths. The first error is returned.
    let restore = if before == "PAUSED" {
        set_play_state(ctx, "PAUSED").await
    } else {
        Ok(())
    };
    let note = result?;
    restore?;
    Ok(note)
}

/// The rates of [`speed`]. `paused`: send PLAY first.
async fn speeds(ctx: &mut Ctx, paused: bool) -> Check {
    // ratv sends only rates that the app lists, and only with the rate command enabled.
    let oracle = oracle(ctx)?;
    if !oracle.accepts_rate(1.5) {
        return Err(Fail::skip(format!(
            "the app does not accept 1.5x, its speeds: {:?}",
            oracle.playback_speeds()
        )));
    }
    if paused {
        set_play_state(ctx, "PLAYING").await?;
    }
    let result = async {
        let how = set_device_rate(ctx, 1.5, SKIP_CHECK).await?;
        // Rate 1.5 is a playback speed, not a scan: PLAYING, not BUFFERING.
        let shown = wait_agree(
            ctx,
            Instant::now(),
            Duration::from_secs(5),
            |_| Duration::from_secs(2),
            NoOracle::Playing,
        )
        .await
        .map_err(|e| e.prefixed("at 1.5x"))?;
        ensure!(
            shown == "PLAYING",
            "state {shown} at 1.5x, expected PLAYING"
        );
        let mut notes = vec![format!("1.5x PLAYING ({how})")];
        if !ctx.oracle.as_ref().is_some_and(|o| o.accepts_rate(2.0)) {
            notes.push("2x not tested: the app does not accept 2x".to_owned());
            return Ok(notes);
        }
        match set_device_rate(ctx, 2.0, "skip the 2x test").await {
            Ok(how) => {
                ctx.client.listen(3).await?;
                let device = ctx.oracle.as_ref().map(Oracle::playback_state);
                notes.push(format!(
                    "2x: driver {}, Apple TV {} ({how})",
                    mp_state(ctx),
                    device_text(device)
                ));
            }
            Err(e) if e.skip => notes.push(format!("2x not tested: {}", e.msg)),
            Err(e) => return Err(e),
        }
        Ok(notes)
    }
    .await;
    // Always go back to 1x, also after an error. The first error is returned. PLAY ends a speed
    // or a scan; ratv sends rate 1.0 only when the app lists other speeds.
    let reset = async {
        let at_1x = ctx
            .oracle
            .as_ref()
            .and_then(Oracle::rate)
            .is_some_and(|r| (r - 1.0).abs() < 0.01);
        let mut note = "back to 1x";
        if !at_1x {
            mp_command(ctx, "PLAY", json!({})).await?;
            if wait_rate(ctx, 1.0).await.is_err() {
                // Without MRP the second connection does not see the rate.
                if !has_mrp(ctx) {
                    note = "1x not checked: the second connection has no MRP";
                } else {
                    let rate = ctx.oracle.as_ref().and_then(Oracle::rate);
                    let rate = rate.map_or_else(|| "unknown".to_owned(), |r| format!("{r}x"));
                    let text = format!(
                        "The rate is still {rate} after PLAY. Choose 1x (normal speed) in the \
                         speed menu of the player on the TV."
                    );
                    let keys = Ask::Action {
                        done: "done",
                        skip: SKIP_CHECK,
                    };
                    let answer = ask(ctx, keys, &text).await?;
                    if answer != Answer::Done {
                        let why = match answer {
                            Answer::NoInput => "not set (no input)",
                            Answer::NoTerminal => NO_TERMINAL,
                            Answer::Skip | Answer::Done | Answer::Yes | Answer::No => {
                                "the owner skipped"
                            }
                        };
                        return Err(Fail::skip(format!("the rate is not 1x, {why}")));
                    }
                    // If the owner's change on the TV did not work, it is not a driver fault.
                    wait_rate(ctx, 1.0)
                        .await
                        .map_err(|e| Fail::skip(format!("set 1x on the TV: {}", e.msg)))?;
                }
            }
        }
        let mp = ctx.mp();
        ctx.client.wait_text(&mp, "state", 5, "PLAYING").await?;
        Ok(note)
    }
    .await;
    let notes = result?;
    let reset = reset?;
    Ok(notes.join(", ") + ", " + reset)
}

/// HOME while an app plays clears now playing on the driver, or the Apple TV stops the app.
pub async fn home_playing(ctx: &mut Ctx) -> Check {
    need_content(ctx, Content::Playing, PlaybackSeen::Driver).await?;
    let before = stable_playback(ctx).await?;
    let mp = ctx.mp();
    if ctx.client.text(&mp, "source").is_empty() {
        return Err(Fail::skip("the driver shows no app"));
    }
    // The app to open again after HOME, not the item that now playing shows after it.
    let app_before = ctx.client.text(&mp, "source");
    let title_before = ctx.client.text(&mp, "media_title");
    let mark = ctx.driver.log_len();
    let start = Instant::now();
    press(ctx, "HOME").await?;
    let result = async {
        // The Apple TV can also stop the app: then now playing shows another app or nothing,
        // and the driver has nothing to clear.
        // Without a title before HOME, another item cannot be told apart: only the clear counts.
        let left_app = |ctx: &Ctx| {
            let source = ctx.client.text(&mp, "source");
            let title = ctx.client.text(&mp, "media_title");
            !title_before.is_empty()
                && mp_state(ctx) != "PLAYING"
                && (source.is_empty()
                    || title.is_empty()
                    || source != app_before
                    || title != title_before)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let cleared_by_driver = loop {
            if driver_log(&ctx.driver, mark).contains(HOME_CLEAR) {
                break true;
            }
            if left_app(ctx) {
                break false;
            }
            if Instant::now() >= deadline {
                break true;
            }
            ctx.client.listen_ms(200).await?;
        };
        if !cleared_by_driver {
            let shown = mp_state(ctx);
            if let Some(o) = ctx.oracle.as_ref() {
                ensure!(
                    o.playback_state() != Some(PlaybackState::Playing),
                    "the Apple TV plays on the Home Screen"
                );
            }
            return Ok(format!(
                "the Apple TV stopped {app_before} ({title_before:?}) {} ms after HOME, driver \
                 shows {:?} / {:?}, state {shown}",
                start.elapsed().as_millis(),
                ctx.client.text(&mp, "source"),
                ctx.client.text(&mp, "media_title")
            ));
        }
        if !driver_log(&ctx.driver, mark).contains(HOME_CLEAR) {
            // An app that plays audio in the background keeps playing on the Home Screen.
            let plays = ctx
                .oracle
                .as_ref()
                .is_some_and(|o| o.playback_state() == Some(PlaybackState::Playing));
            if plays {
                return Err(Fail::skip("the app keeps playing on the Home Screen"));
            }
            if !driver_log(&ctx.driver, mark).contains("screen changed") {
                return Err(Fail::skip(
                    "HOME did not reach the Home Screen: no screen change from the Apple TV",
                ));
            }
            let device = ctx.oracle.as_ref().map(Oracle::playback_state);
            return Err(Fail::new(format!(
                "no Home Screen clear in 10 s and the driver still shows {app_before}, driver \
                 state {}, Apple TV {}",
                mp_state(ctx),
                device_text(device)
            )));
        }
        let seen = start.elapsed().as_millis();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = item_left(ctx, &mp);
            if left.is_empty() {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "5 s after the Home Screen clear the driver shows {}",
                left.join(", ")
            );
            ctx.client.listen_ms(200).await?;
        }
        let shown = mp_state(ctx);
        ensure!(
            shown != "PLAYING",
            "driver state PLAYING on the Home Screen"
        );
        if let Some(o) = ctx.oracle.as_ref() {
            ensure!(
                o.playback_state() != Some(PlaybackState::Playing),
                "the Apple TV plays on the Home Screen"
            );
        }
        Ok(format!(
            "Home Screen clear {seen} ms after HOME, state {shown}"
        ))
    }
    .await;
    open_again(ctx, app_before, &before, result).await
}

/// The item fields that the driver still shows, as `name "value"`. Empty if source, title and
/// image are cleared.
fn item_left(ctx: &Ctx, mp: &str) -> Vec<String> {
    ["source", "media_title", "media_image_url"]
        .into_iter()
        .filter(|a| !cleared(ctx.client.attr(mp, a)))
        .map(|a| format!("{a} {:?}", ctx.client.text(mp, a)))
        .collect()
}

/// Open `app` again and set the play state `state`, for the later checks. Runs also after an
/// error of the check. Returns `result` with a note on the reopen added. A FAIL of `result`
/// comes first; a failed reopen is added to its text. A skip of `result` becomes a FAIL if the
/// reopen fails, else it stays as it is, with no note. If `result` is OK, a skip of the reopen is
/// only a note and a failed reopen is a FAIL.
async fn open_again(ctx: &mut Ctx, app: String, state: &str, result: Check) -> Check {
    let back = match bring_to_front(ctx, true, Some(app)).await {
        Ok(_) if mp_state(ctx) != state => set_play_state(ctx, state)
            .await
            .map(|()| "app open again")
            .map_err(|e| Fail {
                msg: format!("play state not restored: {}", e.msg),
                skip: e.skip,
            }),
        Ok(_) => Ok("app open again"),
        Err(e) => Err(Fail {
            msg: format!("app not open again: {}", e.msg),
            skip: e.skip,
        }),
    };
    let note = match result {
        Err(e) => {
            let join = if e.skip { "but" } else { "and" };
            return match back {
                Err(b) if !b.skip => Err(Fail::new(format!("{}, {join} {}", e.msg, b.msg))),
                _ => Err(e),
            };
        }
        Ok(note) => note,
    };
    match back {
        Ok(back) => Ok(format!("{note}, {back}")),
        Err(e) if e.skip => Ok(format!("{note}, {}", e.msg)),
        Err(e) => Err(Fail::new(format!("{note}, but {}", e.msg))),
    }
}

/// After a wake onto the Home Screen, the driver shows no old paused item. The video is paused
/// and HOME is pressed, so the Apple TV keeps the paused item in now playing.
pub async fn wake_home(ctx: &mut Ctx) -> Check {
    need_content(ctx, Content::Playing, PlaybackSeen::Driver).await?;
    let before = stable_playback(ctx).await?;
    let mp = ctx.mp();
    let app = ctx.client.text(&mp, "source");
    if app.is_empty() {
        return Err(Fail::skip("the driver shows no app"));
    }
    let result = wake_home_steps(ctx).await;
    open_again(ctx, app, &before, result).await
}

/// The part of [`wake_home`] after the content check. When it gets past HOME, it leaves the Apple
/// TV on the Home Screen.
async fn wake_home_steps(ctx: &mut Ctx) -> Check {
    ensure_playing(ctx).await?;
    set_play_state(ctx, "PAUSED").await?;
    let mp = ctx.mp();
    if cleared(ctx.client.attr(&mp, "media_title"))
        && cleared(ctx.client.attr(&mp, "media_image_url"))
    {
        return Err(Fail::skip(
            "the driver shows no title and no image before HOME",
        ));
    }
    let mark = ctx.driver.log_len();
    press(ctx, "HOME").await?;
    let home = wait_home(ctx, mark).await?;
    remote_sleep(ctx).await?;
    let mark = ctx.driver.log_len();
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    let took = wait_joined(ctx, mark, start, 15).await?;
    // The driver keeps the entity states over the standby. Read them again, so only what the
    // driver shows after the connect counts, not the cache of this client.
    ctx.client.forget_states();
    ctx.client.refresh_states().await?;
    // The driver clears the item 1.5 s after the connect, and again after an MRP join that shows
    // the item again. The states can show cleared from before the standby, so after a Home Screen
    // check at the connect the log after the wake must also have a clear that did not end.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        home_screen_or_skip(ctx)?;
        let left = item_left(ctx, &mp);
        let log = driver_log(&ctx.driver, mark);
        let ended = clear_ended(&log);
        let clear_missing = log.contains(NO_PLAYER_CHECK) && (!log.contains(HOME_CLEAR) || ended);
        if left.is_empty() && !clear_missing {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "old item after the wake onto the Home Screen: {}",
            if left.is_empty() && ended {
                "the driver log shows that the Home Screen clear after the wake ended, and no new \
                 clear after it"
                    .to_owned()
            } else if left.is_empty() {
                "5 s after the full connect the driver log has the Home Screen check at the \
                 connect but no clear after the wake"
                    .to_owned()
            } else if ended {
                format!(
                    "the driver shows {} again and does not clear it",
                    left.join(", ")
                )
            } else {
                format!(
                    "5 s after the full connect the driver shows {}{}",
                    left.join(", "),
                    no_check_note(ctx, mark)
                )
            }
        );
        ctx.client.listen_ms(200).await?;
    }
    let cleared_ms = start.elapsed().as_millis();
    // The item must stay cleared. A late MRP join can show it again until the next Home Screen
    // check clears it 1.5 s later, so a short show is accepted. After 5 s, wait up to 2.5 s more
    // until the item is cleared and each show has a later clear.
    let deadline = Instant::now() + Duration::from_secs(5);
    let last = deadline + Duration::from_millis(2500);
    loop {
        ctx.client.listen_ms(200).await?;
        home_screen_or_skip(ctx)?;
        let shown_again = clear_ended(&driver_log(&ctx.driver, mark));
        let left = item_left(ctx, &mp);
        let now = Instant::now();
        if now >= deadline && !shown_again && left.is_empty() {
            break;
        }
        ensure!(
            now < last,
            "old item after the wake onto the Home Screen: {}",
            if left.is_empty() {
                "the driver log shows that the Home Screen clear ended, and no new clear after \
                 it"
                .to_owned()
            } else if shown_again {
                format!(
                    "the driver shows {} again and does not clear it",
                    left.join(", ")
                )
            } else {
                format!(
                    "the driver shows {} again{}",
                    left.join(", "),
                    no_check_note(ctx, mark)
                )
            }
        );
    }
    let checked = if driver_log(&ctx.driver, mark).contains(NO_PLAYER_CHECK) {
        "Home Screen check at the connect"
    } else {
        "no Home Screen check at the connect"
    };
    Ok(format!(
        "{home}, connected after {took} ms, no old item {cleared_ms} ms after exit_standby, \
         {checked}"
    ))
}

/// Skips if the second connection shows that the Apple TV left the Home Screen, plays, or gives
/// no media control flags. Without a second connection it does not skip.
fn home_screen_or_skip(ctx: &Ctx) -> Result<(), Fail> {
    match ctx.oracle.as_ref() {
        Some(o) if !o.home_screen() => Err(Fail::skip(format!(
            "the Apple TV left the Home Screen, plays again, or gives no media control flags \
             after the wake (media control flags {})",
            flags_text(o)
        ))),
        _ => Ok(()),
    }
}

/// The media control flags of the second connection as `0x..`, or `none`.
fn flags_text(oracle: &Oracle) -> String {
    oracle
        .media_flags()
        .map_or_else(|| "none".to_owned(), |f| format!("{f:#x}"))
}

/// True if the last Home Screen clear in `log` ended (now playing again, or the connection
/// closed) and no new clear came after that. Also true for an end line without a clear before
/// it.
fn clear_ended(log: &DriverLog) -> bool {
    log.last_with(&[HOME_LEFT, HOME_CLOSED]) > log.last_with(&[HOME_CLEAR])
}

/// Wait up to 10 s after HOME until the driver clears now playing or the second connection
/// shows the Home Screen. Skips if the Apple TV does not reach the Home Screen, plays on it, or
/// has no item left. Returns a note.
async fn wait_home(ctx: &mut Ctx, mark: LogMark) -> Result<String, Fail> {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    loop {
        let clear = driver_log(&ctx.driver, mark).contains(HOME_CLEAR);
        if clear || ctx.oracle.as_ref().is_some_and(Oracle::home_screen) {
            break;
        }
        if Instant::now() >= deadline {
            let plays = ctx
                .oracle
                .as_ref()
                .is_some_and(|o| o.playback_state() == Some(PlaybackState::Playing));
            return Err(Fail::skip(match ctx.oracle.as_ref() {
                _ if plays => "the app plays on the Home Screen".to_owned(),
                Some(o) => format!(
                    "no Home Screen 10 s after HOME: no clear in the driver log and no Home \
                     Screen on the second connection (media control flags {})",
                    flags_text(o)
                ),
                None => "no Home Screen 10 s after HOME: no clear in the driver log".to_owned(),
            }));
        }
        ctx.client.listen_ms(200).await?;
    }
    let seen = start.elapsed().as_millis();
    // An app can drop its item a short time after HOME.
    ctx.client.listen(2).await?;
    let Some(oracle) = ctx.oracle.as_ref() else {
        return Ok(format!("Home Screen clear {seen} ms after HOME"));
    };
    match oracle.has_item() {
        Some(true) => Ok(format!(
            "Home Screen {seen} ms after HOME with the paused item"
        )),
        Some(false) => Err(Fail::skip(
            "the app removed its item on HOME: no old item is left to test",
        )),
        None => Ok(format!(
            "Home Screen {seen} ms after HOME, item not known: no MRP on the second connection"
        )),
    }
}

/// Text that `say` speaks for the Siri checks.
const SIRI_TEXT: &str = "What time is it";
/// Time Siri can take to answer after the audio ends. The prompt in [`siri_answer`] says 20 s.
const SIRI_ANSWER_MS: u64 = 20_000;
/// Audio chunk size: 64 ms of 16 kHz mono 16 bit audio.
const VOICE_CHUNK: usize = 2048;
/// Bytes of 1 ms of 16 kHz mono 16 bit audio.
const BYTES_PER_MS: usize = 32;
/// Silence after the speech, so the end of the speech is not cut. Short, so the audio ends
/// before the Apple TV ends the recording: on tvOS 27.2.1, when the TV ends it first, the
/// dimmed Siri overlay stays on the TV and blocks navigation until Siri and then Back twice are
/// pressed.
const SILENCE_AFTER_MS: usize = 300;
/// Silence after the speech with `--siri-hold`, like a user who holds the button past the end
/// of listening.
const SILENCE_HOLD_MS: usize = 1500;
/// Samples with an absolute value below this are always quiet.
const QUIET_LEVEL: u16 = 500;
/// Block size to find the start and end of the speech: 20 ms.
const QUIET_BLOCK: usize = 320;
/// Loud blocks in a row that count as speech: 60 ms.
const SPEECH_BLOCKS: usize = 3;

/// Speech of `text` as 16 kHz mono signed 16 bit PCM, made with macOS `say`, without the quiet
/// start and end, with `after_ms` of silence after it, see [`padded`]. `None` without `say`.
async fn speech(
    dir: &std::path::Path,
    text: &str,
    after_ms: usize,
) -> Result<Option<Vec<u8>>, Fail> {
    let file = dir.join("siri.wav");
    let status = tokio::process::Command::new("say")
        .arg("-o")
        .arg(&file)
        .arg("--data-format=LEI16@16000")
        .arg(text)
        .status()
        .await;
    match status {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Fail::new(format!("say: {e}"))),
        Ok(s) if !s.success() => return Err(Fail::new(format!("say: {s}"))),
        Ok(_) => {}
    }
    Ok(Some(padded(&tokio::fs::read(&file).await?, after_ms)?))
}

/// The samples of a WAV file without the quiet start and end, with `after_ms` of silence after
/// them.
fn padded(wav: &[u8], after_ms: usize) -> Result<Vec<u8>, Fail> {
    let speech = wav_pcm(wav)?;
    let speech = trim_quiet(&speech);
    ensure!(
        !speech.is_empty(),
        "no speech in the audio: silent or too quiet"
    );
    // Zero samples are silence.
    let mut pcm = Vec::with_capacity(speech.len() + after_ms * BYTES_PER_MS);
    pcm.extend_from_slice(speech);
    pcm.resize(pcm.len() + after_ms * BYTES_PER_MS, 0);
    Ok(pcm)
}

/// 16 bit little endian PCM without the quiet start and end. The PCM is read in blocks of
/// [`QUIET_BLOCK`] samples. A block is loud when its peak is at least [`QUIET_LEVEL`] and at
/// least 1/16 of the peak of all samples, so room noise is quiet. The speech starts with the
/// first and ends with the last run of [`SPEECH_BLOCKS`] or more loud blocks, so short clicks
/// are cut. Empty when there is no such run.
fn trim_quiet(pcm: &[u8]) -> &[u8] {
    let samples = pcm.as_chunks::<2>().0;
    let level = |b: &[u8; 2]| i16::from_le_bytes(*b).unsigned_abs();
    let peak = samples.iter().map(level).max().unwrap_or(0);
    let loud_level = QUIET_LEVEL.max(peak / 16);
    let (mut start, mut end) = (None, 0);
    let (mut run, mut run_start, mut pos) = (0, 0, 0);
    for block in samples.chunks(QUIET_BLOCK) {
        if block.iter().any(|b| level(b) >= loud_level) {
            if run == 0 {
                run_start = pos;
            }
            run += 1;
        } else {
            run = 0;
        }
        pos += block.len();
        if run >= SPEECH_BLOCKS {
            start.get_or_insert(run_start);
            end = pos;
        }
    }
    &pcm[start.unwrap_or(0) * 2..end * 2]
}

/// The samples of a WAV file. It must be 16 kHz mono signed 16 bit PCM.
fn wav_pcm(wav: &[u8]) -> Result<Vec<u8>, Fail> {
    ensure!(
        wav.len() >= 12 && &wav[..4] == b"RIFF" && &wav[8..12] == b"WAVE",
        "not a WAV file"
    );
    let mut rest = &wav[12..];
    let mut format_ok = false;
    while rest.len() >= 8 {
        let (id, len) = (
            &rest[..4],
            u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]),
        );
        let len = len as usize;
        let body = &rest[8..];
        ensure!(body.len() >= len, "cut WAV chunk");
        let data = &body[..len];
        match id {
            b"fmt " => {
                ensure!(data.len() >= 16, "short WAV format");
                let u16_at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
                let rate = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                // PCM, 1 channel, 16 kHz, 16 bit.
                format_ok = u16_at(0) == 1 && u16_at(2) == 1 && rate == 16_000 && u16_at(14) == 16;
            }
            b"data" => {
                ensure!(format_ok, "WAV is not 16 kHz mono 16 bit PCM");
                return Ok(data.to_vec());
            }
            _ => {}
        }
        // A chunk with an odd size has one pad byte.
        rest = &body[(len + (len & 1)).min(body.len())..];
    }
    Err(Fail::new("no audio data in the WAV file"))
}

/// A protobuf varint.
fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A length-delimited protobuf field.
fn field_bytes(out: &mut Vec<u8>, field: u8, data: &[u8]) {
    out.push(field << 3 | 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// A varint protobuf field.
fn field_varint(out: &mut Vec<u8>, field: u8, v: u64) {
    out.push(field << 3);
    varint(out, v);
}

/// A voice message of the Remote.
enum Voice<'a> {
    Begin,
    Data(&'a [u8]),
    End,
}

/// Binary `IntegrationMessage` with a voice message of `session_id`.
fn voice_message(msg: &Voice<'_>, session_id: u32) -> Vec<u8> {
    let samples = match msg {
        Voice::Data(samples) => samples.len(),
        _ => 0,
    };
    let mut inner = Vec::with_capacity(samples + 16);
    field_varint(&mut inner, 1, u64::from(session_id));
    let field = match msg {
        Voice::Begin => {
            // AudioConfiguration: 1 channel, 16 kHz, I16 (2), PCM (1).
            let mut cfg = Vec::with_capacity(12);
            field_varint(&mut cfg, 1, 1);
            field_varint(&mut cfg, 2, 16_000);
            field_varint(&mut cfg, 3, 2);
            field_varint(&mut cfg, 4, 1);
            field_bytes(&mut inner, 2, &cfg);
            1
        }
        Voice::Data(samples) => {
            field_bytes(&mut inner, 2, samples);
            2
        }
        Voice::End => 3,
    };
    let mut out = Vec::with_capacity(inner.len() + 4);
    field_bytes(&mut out, field, &inner);
    out
}

/// Set the Siri mode in the setup. Returns the mode before and the result of the change. A
/// failed change aborts the setup.
async fn set_siri_mode(ctx: &mut Ctx, mode: &str) -> Result<(String, Result<(), Fail>), Fail> {
    let page = start_setup(ctx, true).await?;
    let Some(old) = field_value(&page, "siri_mode") else {
        abort(ctx).await?;
        return Err(Fail::new(format!(
            "no siri_mode on the configuration page: {page}"
        )));
    };
    let key = ctx.key.clone();
    let mut done = ctx
        .client
        .answer(json!({ "choice": key, "action": "settings", "siri_mode": mode }))
        .await;
    if done.is_ok() {
        done = ctx.client.setup_done(10).await;
    }
    if done.is_err() {
        abort(ctx).await?;
    }
    Ok((old, done))
}

/// The speech for the Siri checks, made once with `say`. The quiet start and end are cut, then
/// [`SILENCE_AFTER_MS`] of silence follows, or [`SILENCE_HOLD_MS`] with `--siri-hold`. Skips
/// without `say`.
async fn siri_speech(ctx: &mut Ctx) -> Result<Vec<u8>, Fail> {
    if let Some(pcm) = &ctx.siri_pcm {
        return Ok(pcm.clone());
    }
    let after_ms = if ctx.opt.siri_hold {
        SILENCE_HOLD_MS
    } else {
        SILENCE_AFTER_MS
    };
    let Some(pcm) = speech(&ctx.driver.dir, SIRI_TEXT, after_ms).await? else {
        return Err(Fail::skip("no `say` command to make the speech"));
    };
    ctx.siri_pcm = Some(pcm.clone());
    Ok(pcm)
}

/// Which audio the Siri checks send.
fn voice_note(ctx: &Ctx) -> String {
    let (after_ms, hold, flag) = if ctx.opt.siri_hold {
        (SILENCE_HOLD_MS, "held ", " (--siri-hold)")
    } else {
        (SILENCE_AFTER_MS, "", "")
    };
    // The speech is the audio without the added silence.
    let speech = ctx
        .siri_pcm
        .as_ref()
        .map_or(0, |pcm| (pcm.len() / BYTES_PER_MS).saturating_sub(after_ms));
    format!("`say` voice, {speech} ms of speech, {hold}{after_ms} ms after the speech{flag}")
}

/// The Siri question to the owner. PASS on yes, FAIL on no, else SKIP. `note` starts each
/// result. The driver and the second connection stay open until the answer, because Siri
/// answers some seconds after the audio and a closed connection loses the answer. Without a
/// terminal or input it waits until [`SIRI_ANSWER_MS`] after `audio_end` instead. Then it
/// closes the answer with [`close_siri`].
async fn siri_answer(ctx: &mut Ctx, note: &str, audio_end: Instant) -> Check {
    let question = format!("Did Siri answer '{SIRI_TEXT}' and show the time?");
    let hint = Some("Siri can take up to 20 s to answer.");
    let keys = Ask::Question {
        yes: "it answered",
        no: "nothing came after 20 s",
        skip: "skip this check without waiting for the answer",
    };
    let answer = ask_hint(ctx, keys, &question, hint).await?;
    // A WebSocket error in a wait is a FAIL, not hidden behind the SKIP.
    let lost = if matches!(answer, Answer::NoTerminal | Answer::NoInput) {
        siri_late_answer(ctx, audio_end).await.err()
    } else {
        None
    };
    let closed = close_siri(ctx).await;
    if let Some(e) = lost {
        return Err(Fail::new(format!(
            "{note}, while waiting for the Siri answer: {}{closed}",
            e.msg
        )));
    }
    match answer {
        Answer::No => Err(Fail::new(format!(
            "{note}, Siri did not answer as expected{closed}"
        ))),
        Answer::Yes => Ok(format!("{note}, Siri answered{closed}")),
        Answer::NoTerminal => Err(Fail::skip(format!(
            "{note}, the answer on the TV was not checked, {NO_TERMINAL}{closed}"
        ))),
        Answer::Skip | Answer::NoInput | Answer::Done => Err(Fail::skip(format!(
            "{note}, the answer on the TV was not checked{closed}"
        ))),
    }
}

/// Close the Siri answer with Back, like a user. The Siri Remote must not be used for this.
/// Uses the remote entity of the driver while the connections are open, then waits 1 s.
/// Returns an empty note, or a note when the send failed.
async fn close_siri(ctx: &mut Ctx) -> String {
    let note = match press(ctx, "BACK").await {
        Ok(()) => String::new(),
        Err(e) => format!(", Siri not closed with Back: {}", e.msg),
    };
    let _ = ctx.client.listen_ms(1000).await;
    note
}

/// Siri with the audio of `siri_wake` (speech and silence of [`siri_speech`]), sent over the
/// second connection with ratv, without the driver. It shows whether Siri works with ratv
/// directly, to compare with `siri_wake`.
pub async fn siri_direct(ctx: &mut Ctx) -> Check {
    oracle(ctx)?;
    if ctx.oracle.as_ref().and_then(Oracle::siri_enabled) == Some(false) {
        return Err(Fail::skip("Siri is off on the Apple TV"));
    }
    let pcm = siri_speech(ctx).await?;
    let samples: Vec<i16> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    turn_on(ctx).await?;
    let sent = {
        let oracle = ctx
            .oracle
            .as_ref()
            .ok_or_else(|| Fail::skip("no second connection to compare with"))?;
        let send = oracle.ask_siri(&samples);
        tokio::pin!(send);
        // Read the WebSocket meanwhile, so a closed connection fails the check and the states stay
        // current.
        loop {
            tokio::select! {
                sent = &mut send => break sent,
                result = ctx.client.listen_ms(500) => result?,
            }
        }
    };
    let audio_end = Instant::now();
    // 16 samples are 1 ms.
    let total = samples.len() / 16;
    let voice = voice_note(ctx);
    let note = match sent {
        Ok((protocol, sent)) if sent < samples.len() => format!(
            "direct path without the driver, over {protocol}, {voice}, the Apple TV stopped \
             recording after {} ms of {total} ms of audio",
            sent / 16
        ),
        Ok((protocol, _)) => {
            format!("direct path without the driver, over {protocol}, {voice}, {total} ms of audio")
        }
        Err((e @ ratv::Error::SiriDisabled, _)) => {
            return Err(Fail::skip(format!("direct path without the driver: {e}")));
        }
        Err((e, started)) => {
            // No wait when no audio was sent.
            let closed = if started
                && !matches!(e, ratv::Error::SiriNoAudio | ratv::Error::SiriNotRecording)
            {
                // The check fails already.
                let _ = siri_late_answer(ctx, audio_end).await;
                close_siri(ctx).await
            } else {
                String::new()
            };
            return Err(Fail::new(format!(
                "direct path without the driver, ratv error: {e}{closed}"
            )));
        }
    };
    siri_answer(ctx, &note, audio_end).await
}

/// Settings file of the driver. It has the debug log option and the Siri mode.
const SETTINGS_FILE: &str = "settings.json";

/// Siri mode id in [`SETTINGS_FILE`] of the driver. The driver loads a missing or unknown value
/// as `auto`.
fn siri_mode(ctx: &Ctx) -> String {
    let text = std::fs::read_to_string(ctx.driver.dir.join(SETTINGS_FILE)).unwrap_or_default();
    let settings: Value = serde_json::from_str(&text).unwrap_or_default();
    match settings["siri_mode"].as_str() {
        Some("mrp") => "mrp".into(),
        _ => "auto".into(),
    }
}

/// Siri right after `exit_standby` in the Auto mode (Companion on tvOS 27), with the speech and
/// silence of [`siri_speech`]. MRP Siri does not work on tvOS 27 and later, so the check does not
/// use it.
pub async fn siri_wake(ctx: &mut Ctx) -> Check {
    if ctx.oracle.as_ref().and_then(Oracle::siri_enabled) == Some(false) {
        return Err(Fail::skip("Siri is off on the Apple TV"));
    }
    let pcm = siri_speech(ctx).await?;
    // Already Auto: the setup is not touched.
    if siri_mode(ctx) == "auto" {
        return siri_session(ctx, &pcm).await;
    }
    let (old, set) = set_siri_mode(ctx, "auto").await?;
    let result = match set {
        Ok(()) => siri_session(ctx, &pcm).await,
        Err(e) => Err(Fail::new(format!("Siri mode Auto: {}", e.msg))),
    };
    // Set the old mode back, also after an error. The first error is returned.
    let restore = if old == "auto" {
        Ok(())
    } else {
        set_siri_mode(ctx, &old).await.and_then(|(_, done)| done)
    };
    let note = result?;
    restore.map_err(|e| {
        Fail::new(format!(
            "{note}, but Siri mode {old} not set back: {}",
            e.msg
        ))
    })?;
    Ok(note)
}

/// Wake with exit_standby and send the speech to Siri through the driver.
async fn siri_session(ctx: &mut Ctx, pcm: &[u8]) -> Check {
    turn_on(ctx).await?;
    let voice = ctx.voice();
    remote_sleep(ctx).await?;
    let mark = ctx.driver.log_len();
    ctx.client.clear_events();
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    // Like a Remote that wakes on the voice key: no wait for the connect.
    let session: u32 = 4711;
    ctx.client
        .command_ok(
            "voice_assistant",
            &voice,
            "voice_start",
            json!({
                "session_id": session,
                "audio_cfg": { "channels": 1, "sample_rate": 16000, "sample_format": "I16" },
            }),
        )
        .await?;
    // Like the Remote: the audio starts after the ready event.
    // The driver waits for the connect (up to 10 s), then starts Siri.
    let ready = match next_voice_event(ctx, session, 15).await? {
        Some(Ok(true)) => start.elapsed().as_millis(),
        found => {
            // End the session on the driver, so it does not wait for audio.
            ctx.client
                .send_binary(voice_message(&Voice::End, session))
                .await?;
            return Err(Fail::new(match found {
                Some(Err(message)) => format!("assistant error before ready: {message}"),
                Some(Ok(_)) => "assistant_event finished before ready".to_owned(),
                None => "no assistant_event ready in 15 s".to_owned(),
            }));
        }
    };
    // At the speed of speech.
    ctx.client
        .send_binary(voice_message(&Voice::Begin, session))
        .await?;
    let ms = (VOICE_CHUNK / BYTES_PER_MS) as u64;
    let mut sent = 0;
    // The Apple TV can end Siri before all audio is sent: then stop, like the Remote.
    let mut early = None;
    for chunk in pcm.chunks(VOICE_CHUNK) {
        ctx.client
            .send_binary(voice_message(&Voice::Data(chunk), session))
            .await?;
        sent += chunk.len();
        ctx.client.listen_ms(ms).await?;
        while let Some(event) = ctx.client.pop_assistant() {
            if let Some(found) = voice_event(&event, session) {
                early = Some(found);
            }
        }
        if early.is_some() {
            break;
        }
    }
    // A lost WebSocket returns at once.
    ctx.client
        .send_binary(voice_message(&Voice::End, session))
        .await?;
    let audio_end = Instant::now();
    let audio_ms = |bytes: usize| bytes / BYTES_PER_MS;
    let sent_note = if sent < pcm.len() {
        format!(
            "Siri ended the session after {} ms of {} ms of audio",
            audio_ms(sent),
            audio_ms(pcm.len())
        )
    } else {
        format!("all {} ms of audio sent", audio_ms(pcm.len()))
    };
    let voice = voice_note(ctx);
    let result = match siri_result(ctx, session, mark, ready, sent, early).await {
        Ok(note) => {
            let note = format!("{note}, the automatic checks passed");
            siri_answer(ctx, &note, audio_end).await
        }
        Err(mut e) => {
            if sent > 0 {
                // The check fails already.
                let _ = siri_late_answer(ctx, audio_end).await;
                e.msg.push_str(&close_siri(ctx).await);
            }
            Err(e)
        }
    };
    result
        .map(|note| format!("{note}, {sent_note}, {voice}"))
        .map_err(|e| Fail {
            msg: format!("{}; {sent_note}, {voice}", e.msg),
            skip: e.skip,
        })
}

/// The automatic checks of [`siri_session`] after the audio: the end event and the log. `sent`
/// is the number of audio bytes sent. `early` is an end event that came while the audio was
/// sent.
async fn siri_result(
    ctx: &mut Ctx,
    session: u32,
    mark: LogMark,
    ready: u128,
    sent: usize,
    early: Option<Result<bool, String>>,
) -> Check {
    let voice = ctx.voice();
    // Siri answers after the audio. The driver sends finished or error at the end.
    let event = match early {
        Some(event) => Some(event),
        None => next_voice_event(ctx, session, 30).await?,
    };
    let end = match event {
        Some(Ok(false)) => Ok(()),
        Some(Err(message)) => Err(message),
        Some(Ok(true)) => Err("a second ready event".to_owned()),
        None => return Err(Fail::new("no assistant_event finished or error in 30 s")),
    };
    // The voice_end line can come a short time after the end event.
    let end_line = format!("=> voice_end({session}): ");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut log = driver_log(&ctx.driver, mark);
    while !log.contains(&end_line) && Instant::now() < deadline {
        ctx.client.listen_ms(100).await?;
        log = driver_log(&ctx.driver, mark);
    }
    let failed = log.lines_with(&["Siri failed"]);
    ensure!(failed.is_empty(), "{}", failed.join(" | "));
    if let Err(message) = end {
        return Err(Fail::new(format!("assistant error: {message}")));
    }
    ensure!(
        log.contains(&format!("=> voice_begin({session})")),
        "no voice_begin line in the log"
    );
    let bytes = log
        .entries()
        .find_map(|l| {
            let rest = l.message.split_once(&end_line)?.1;
            let (_, rest) = rest.split_once(" chunks, ")?;
            rest.split_once(" bytes")?.0.parse::<u64>().ok()
        })
        .ok_or_else(|| Fail::new("no voice_end line in the log 2 s after the end"))?;
    // The driver counts each chunk, also after the session ended. Fewer bytes means a lost
    // chunk.
    ensure!(
        bytes == sent as u64,
        "voice_end with {bytes} bytes, {sent} sent"
    );
    ensure!(
        log.contains("mode Auto"),
        "the Siri session did not use the Auto mode"
    );
    ctx.client.refresh_states().await?;
    let state = ctx.client.text(&voice, "state");
    ensure!(
        !ctx.client.was_unavailable(&voice) && state != "UNAVAILABLE",
        "the voice entity was UNAVAILABLE"
    );
    Ok(format!(
        "ready after {ready} ms, {bytes} bytes received by the driver"
    ))
}

/// Wait until [`SIRI_ANSWER_MS`] after `audio_end`, when the owner cannot answer or a check
/// failed after the audio, so the next step does not close a connection before Siri answers.
/// The WebSocket is read meanwhile.
/// On a WebSocket error it still waits, then returns the error.
async fn siri_late_answer(ctx: &mut Ctx, audio_end: Instant) -> Result<(), Fail> {
    let end = audio_end + Duration::from_millis(SIRI_ANSWER_MS);
    let ms = end.saturating_duration_since(Instant::now()).as_millis() as u64;
    let result = ctx.client.listen_ms(ms).await;
    if result.is_err() {
        tokio::time::sleep_until(end.into()).await;
    }
    result
}

/// The next assistant event of `session` within `secs`: `Ok(true)` for ready, `Ok(false)` for
/// finished, `Err` with the message for error. Other events are skipped.
async fn next_voice_event(
    ctx: &mut Ctx,
    session: u32,
    secs: u64,
) -> Result<Option<Result<bool, String>>, Fail> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let Some(event) = ctx.client.next_assistant(left.as_secs().max(1)).await? else {
            return Ok(None);
        };
        if let Some(found) = voice_event(&event, session) {
            return Ok(Some(found));
        }
    }
}

/// An assistant event of `session`: `Ok(true)` for ready, `Ok(false)` for finished, `Err` with
/// the message for error. `None` for other events.
fn voice_event(event: &Value, session: u32) -> Option<Result<bool, String>> {
    if event["session_id"] != session {
        return None;
    }
    match event["type"].as_str()? {
        "ready" => Some(Ok(true)),
        "finished" => Some(Ok(false)),
        "error" => {
            let message = event["data"]["message"].as_str().unwrap_or_default();
            Some(Err(message.to_owned()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- reconfiguration

pub async fn abort_setup(ctx: &mut Ctx) -> Check {
    start_setup(ctx, true).await?;
    // The configured devices keep working during a setup flow.
    press(ctx, "HOME").await?;
    abort(ctx).await?;
    let state = wait_available(ctx, 10).await?;
    press(ctx, "HOME").await?;
    Ok(format!(
        "state {state}, commands work during the setup and after the abort"
    ))
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

/// Number of apps in the media player source list.
fn app_count(ctx: &Ctx) -> usize {
    ctx.client
        .attr(&ctx.mp(), "source_list")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// Number of apps in the cache file of the driver for this device. `None` if the file is
/// missing or not valid.
fn cached_apps(ctx: &Ctx) -> Option<usize> {
    let file = ctx.driver.dir.join(format!("cache_{}.json", ctx.key));
    let text = std::fs::read_to_string(file).ok()?;
    let cache: Value = serde_json::from_str(&text).ok()?;
    cache.get("apps")?.as_array().map(Vec::len)
}

/// The driver answers the pings of the Remote's client. The client pings every second and closes
/// after 2 missed answers.
pub async fn ws_keepalive(ctx: &mut Ctx) -> Check {
    let config = WebSocketConfig::builder()
        .ping_interval(Duration::from_secs(1))
        .max_missed_pongs(2u32)
        .max_reconnect_attempts(0u32)
        .build()?;
    let mut client = Client::connect_with(ctx.driver.port, config).await?;
    client.listen(5).await?;
    ensure!(client.is_connected().await, "not connected after 5 s");
    Ok("5 s with a ping every second".into())
}

/// A Remote that sends nothing stays connected: the driver pings every 20 s and closes after 40 s
/// without a message. The client sends no pings, so only its answers to the driver keep the
/// connection.
pub async fn ws_idle(ctx: &mut Ctx) -> Check {
    let config = WebSocketConfig::builder()
        .ping_enabled(false)
        .max_reconnect_attempts(0u32)
        .build()?;
    let mut client = Client::connect_with(ctx.driver.port, config).await?;
    client.listen(45).await?;
    ensure!(client.is_connected().await, "not connected after 45 s");
    Ok("45 s without messages".into())
}

/// A Remote that closes the connection. The driver must read the close frame and end the session.
/// This client waits until the driver closes the connection. The client of older Remote firmware
/// closes the socket right after its close frame; the driver test
/// `close_frame_before_end_of_connection_is_read` covers that case.
pub async fn ws_close(ctx: &mut Ctx) -> Check {
    const NEW: &str = "New WebSocket connection from: ";
    let mark = ctx.driver.log_len();
    let client = Client::connect(ctx.driver.port).await?;
    // The driver logs lines of a connection with its peer address as `[id]`.
    let log = driver_log(&ctx.driver, mark);
    let Some(id) = log.entries().find_map(|l| {
        l.message
            .split_once(NEW)
            .map(|(_, id)| format!("[{}]", id.trim()))
    }) else {
        return Err(Fail::new("the driver logged no new connection"));
    };
    let stopped = format!("{id} stopped");
    client.close().await;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let log = driver_log(&ctx.driver, mark);
        let lines: Vec<&str> = log
            .entries()
            .map(|l| l.message)
            .filter(|m| m.contains(&id))
            .collect();
        if lines.iter().any(|m| m.contains(&stopped)) {
            if lines.iter().any(|m| m.contains("Remote closed connection")) {
                return Ok("the driver read the close frame".into());
            }
            // The connection logs why it closes in its last line before `stopped`. The controller
            // logs "Remote disconnected" on another thread, so that line can come between them.
            let how = lines
                .into_iter()
                .take_while(|l| !l.contains(&stopped))
                .filter(|l| !l.contains("Remote disconnected"))
                .last()
                .map_or("no close line", |l| {
                    l.split_once(&id).map_or(l, |(_, rest)| rest.trim())
                });
            return Err(Fail::new(format!(
                "the session ended without the close frame: {how}"
            )));
        }
        ensure!(
            Instant::now() < deadline,
            "the driver did not end the session in 3 s"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// The Remote's client connects again after the driver restarts, like a Remote after a driver
/// update.
pub async fn ws_driver_restart(ctx: &mut Ctx) -> Check {
    let config = WebSocketConfig::builder()
        .initial_backoff(Duration::from_millis(500))
        .build()?;
    let mut client = Client::connect_with(ctx.driver.port, config).await?;
    let stopped = Instant::now();
    ctx.driver.stop().await;
    let reason = client.wait_closed(5).await;
    // Start the driver also when the check fails, so the next steps have a driver.
    restart(ctx).await?;
    ctx.client.forget_states();
    subscribe_all(ctx).await?;
    let result = async {
        let Some(reason) = reason else {
            return Err(Fail::new("the client saw no close in 5 s"));
        };
        let connected = client.wait_connected(10).await?;
        client.send_event("connect").await?;
        let states = client.request_ok("get_entity_states", Value::Null).await?;
        ensure!(
            states.as_array().is_some_and(|s| !s.is_empty()),
            "no entity states after the reconnect: {states}"
        );
        Ok((connected.duration_since(stopped), reason))
    }
    .await;
    // The next step stops the driver again: let it connect to the Apple TV first. The check error
    // is reported before an error of this wait.
    let ready = wait_available(ctx, 30).await;
    let (took, reason) = result?;
    ready?;
    Ok(format!(
        "close: {reason:?}, connected again {} ms after the driver stop",
        took.as_millis()
    ))
}

pub async fn driver_restart(ctx: &mut Ctx) -> Check {
    let ids = entity_ids(ctx).await?;
    // Wait for the app list, then for the driver to save it to the cache file before the stop.
    // The list can be empty after a restore.
    let deadline = Instant::now() + Duration::from_secs(10);
    while app_count(ctx) == 0 && Instant::now() < deadline {
        ctx.client.listen_ms(200).await?;
    }
    let mut before = app_count(ctx);
    if before > 0 {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut cached = cached_apps(ctx);
        while cached != Some(before) {
            ensure!(
                Instant::now() < deadline,
                "the driver did not save the cache: cache_{}.json after 10 s: {}, {before} apps \
                 expected",
                ctx.key,
                cached.map_or_else(
                    || "missing or not valid".to_owned(),
                    |n| format!("{n} apps")
                )
            );
            ctx.client.listen_ms(200).await?;
            // The list can still change while the driver saves it.
            before = app_count(ctx);
            cached = cached_apps(ctx);
        }
    }
    ctx.driver.stop().await;
    let start = Instant::now();
    restart(ctx).await?;
    let after = entity_ids(ctx).await?;
    ensure!(after == ids, "entities differ after the restart");
    ctx.client.forget_states();
    subscribe_all(ctx).await?;
    // The first states come from the cache, before the Apple TV answers.
    let mp = ctx.mp();
    if ctx.client.attr(&mp, "state").is_none() {
        ctx.client.wait_attr(&mp, "state", 10, |_| true).await?;
    }
    let cached = app_count(ctx);
    let state = wait_available(ctx, 30).await?;
    let took = start.elapsed().as_millis();
    let apps = if before == 0 {
        "apps not tested: the Apple TV gave no app list".to_owned()
    } else {
        let after = cached;
        ensure!(
            after == before,
            "{after} apps from the cache, {before} before the restart"
        );
        format!("{after} apps from the cache")
    };
    let mut fresh = "";
    if ctx.oracle.is_some()
        && let Err(e) = verify(ctx, 10).await
    {
        // The second connection can keep an old paused item that a new connection does not
        // get. Compare once more with a new second connection.
        let old_item = mismatches(ctx).await.is_ok_and(|diff| {
            !diff.is_empty()
                && diff
                    .iter()
                    .all(|d| d.starts_with("source ") || d.starts_with("media_title "))
        }) && ctx
            .oracle
            .as_ref()
            .is_some_and(|o| o.playback_state() != Some(PlaybackState::Playing));
        if !old_item {
            return Err(e);
        }
        reconnect_oracle(ctx)
            .await
            .map_err(|r| Fail::new(format!("{}; second connection not open again: {r}", e.msg)))?;
        verify(ctx, 10)
            .await
            .map_err(|e| Fail::new(format!("{} (with a new second connection)", e.msg)))?;
        fresh = ", second connection opened again: it had an old item";
    }
    Ok(format!("state {state} after {took} ms, {apps}{fresh}"))
}

/// Start the driver again, with or without `RUST_LOG`, and connect a new Remote. Waits until the
/// media player is available.
async fn restart_ready(ctx: &mut Ctx, rust_log: RustLog) -> Result<(), Fail> {
    ctx.driver.launch(rust_log).await?;
    ctx.reconnect().await?;
    ctx.client.forget_states();
    subscribe_all(ctx).await?;
    wait_available(ctx, 30).await.map(|_| ())
}

/// The debug log option of the setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DebugLog {
    On,
    Off,
}

impl DebugLog {
    /// The value in the settings file and in the setup.
    const fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

/// The Remote goes to standby and wakes, so the driver and ratv connect again and log.
async fn wake_again(ctx: &mut Ctx) -> Result<(), Fail> {
    remote_sleep(ctx).await?;
    let mark = ctx.driver.log_len();
    ctx.client.send_event("exit_standby").await?;
    wait_joined(ctx, mark, Instant::now(), 15).await?;
    // Time for the lines after the connect.
    ctx.client.listen(2).await
}

/// Turn the debug log option on or off with "Save settings" in the configuration mode.
async fn save_debug_log(ctx: &mut Ctx, option: DebugLog) -> Result<(), Fail> {
    let key = ctx.key.clone();
    let page = start_setup(ctx, true).await?;
    ensure!(
        field_ids(&page).iter().any(|f| f == "debug_log"),
        "configuration mode page without debug_log: {page}"
    );
    ctx.client
        .answer(
            json!({ "choice": key, "action": "settings", "debug_log": option.is_on().to_string() }),
        )
        .await?;
    ctx.client.setup_done(20).await
}

/// Wait up to 2 s until the settings file has `option` for the debug log. The driver writes the
/// file in another thread.
async fn wait_settings(ctx: &Ctx, option: DebugLog) -> Result<(), Fail> {
    let on = option.is_on();
    let settings = ctx.driver.dir.join(SETTINGS_FILE);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let saved = tokio::fs::read_to_string(&settings)
            .await
            .unwrap_or_default();
        let value = serde_json::from_str::<Value>(&saved)
            .ok()
            .and_then(|v| v["debug_log"].as_bool());
        if value == Some(on) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "{SETTINGS_FILE} after 2 s, debug log {on} expected: {saved}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// The settings of `before` with the debug log option set to `option`. The other values stay. A
/// missing file or one that is not a JSON object gives only the option.
fn settings_with_debug_log(before: Option<&[u8]>, option: DebugLog) -> Value {
    let mut settings: Value = before
        .and_then(|s| serde_json::from_slice(s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    settings["debug_log"] = option.is_on().into();
    settings
}

/// Write `before` back to the settings file at `path`. `None` removes the file, because there
/// was none before.
async fn restore_settings(path: &std::path::Path, before: Option<&[u8]>) -> Result<(), Fail> {
    match before {
        Some(bytes) => tokio::fs::write(path, bytes).await,
        None => match tokio::fs::remove_file(path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
    .map_err(|e| Fail::new(format!("{} not set back: {e}", path.display())))
}

/// Start the driver with the debug log option off, turn it on in the setup, then off, then start
/// the driver with the option on. `settings` is the settings file before the step. The driver
/// starts without `RUST_LOG`, because `RUST_LOG` overrides the option.
async fn debug_log_checks(ctx: &mut Ctx, settings: Option<&[u8]>) -> Check {
    let off = settings_with_debug_log(settings, DebugLog::Off);
    ctx.driver.stop().await;
    let path = ctx.driver.dir.join(SETTINGS_FILE);
    tokio::fs::write(&path, off.to_string())
        .await
        .map_err(|e| Fail::new(format!("{}: {e}", path.display())))?;
    let mark = ctx.driver.log_len();
    restart_ready(ctx, RustLog::Unset).await?;
    ctx.client.listen(2).await?;
    let lines = RawLog::since(&ctx.driver, mark).debug_lines();
    ensure!(
        lines == (0, 0),
        "debug lines (driver, ratv) {lines:?} with the option off"
    );

    let mark = ctx.driver.log_len();
    save_debug_log(ctx, DebugLog::On).await?;
    ensure!(
        !driver_log(&ctx.driver, mark).contains("RUST_LOG is used instead"),
        "the driver uses RUST_LOG, but it was started without it"
    );
    wake_again(ctx).await?;
    let (driver, ratv) = log_after(&ctx.driver, mark, "] debug log on")?.debug_lines();
    ensure!(
        driver > 0 && ratv > 0,
        "{driver} driver and {ratv} ratv debug lines after the option was turned on"
    );
    wait_settings(ctx, DebugLog::On).await?;

    let mark = ctx.driver.log_len();
    save_debug_log(ctx, DebugLog::Off).await?;
    log_after(&ctx.driver, mark, "] debug log off")?;
    wait_settings(ctx, DebugLog::Off).await?;
    // A debug line that was written while the option changed does not count.
    let mark = ctx.driver.log_len();
    wake_again(ctx).await?;
    let after = RawLog::since(&ctx.driver, mark).debug_lines();
    ensure!(
        after == (0, 0),
        "debug lines (driver, ratv) {after:?} after the option was turned off"
    );

    save_debug_log(ctx, DebugLog::On).await?;
    wait_settings(ctx, DebugLog::On).await?;
    ctx.driver.stop().await;
    let mark = ctx.driver.log_len();
    restart_ready(ctx, RustLog::Unset).await?;
    ensure!(
        driver_log(&ctx.driver, mark).contains("Debug log is on"),
        "no 'Debug log is on' line after a start with the option on"
    );
    ensure!(
        RawLog::since(&ctx.driver, mark).debug_lines().0 > 0,
        "no driver debug lines after a start with the option on"
    );
    Ok(format!(
        "option on ({driver} driver, {ratv} ratv debug lines), off, and on after a start"
    ))
}

/// The debug log option of the setup. The driver of the other checks gets `RUST_LOG`, so the
/// option has no effect there. At the end, on all paths, the settings file is set back and the
/// driver is started again with `RUST_LOG`.
pub async fn debug_log(ctx: &mut Ctx) -> Check {
    let settings = ctx.driver.dir.join(SETTINGS_FILE);
    let before = match tokio::fs::read(&settings).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Fail::new(format!("{}: {e}", settings.display()))),
    };
    let result = debug_log_checks(ctx, before.as_deref()).await;
    ctx.driver.stop().await;
    let file = restore_settings(&settings, before.as_deref()).await;
    // Start the driver also when the check fails, so the next steps have a driver.
    let started = restart_ready(ctx, RustLog::Set).await;
    // The check error comes first, then the errors from setting things back.
    let after: Vec<String> = [file.err(), started.err()]
        .into_iter()
        .flatten()
        .map(|e| e.msg)
        .collect();
    match result {
        Ok(note) if after.is_empty() => Ok(note),
        Ok(_) => Err(Fail::new(after.join("; "))),
        Err(mut e) => {
            for msg in after {
                e.msg.push_str("; ");
                e.msg.push_str(&msg);
            }
            Err(e)
        }
    }
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
        let mode = tokio::fs::metadata(ctx.driver.config_file())
            .await?
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
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    ctx.driver.stop().await;
    tokio::fs::write(ctx.driver.config_file(), "[{\"broken\": ").await?;
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
        let _ = tokio::fs::remove_file(file).await;
    }
    tokio::fs::write(ctx.driver.config_file(), good).await?;
    restart(ctx).await?;
    ctx.client.forget_states();
    subscribe_all(ctx).await?;
    // The check error is reported before an error of this wait.
    let ready = wait_available(ctx, 30).await;
    let note = result?;
    ready?;
    Ok(note)
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

/// Close the second connection and open a new one, then wait 3 s for its state.
async fn reconnect_oracle(ctx: &mut Ctx) -> Result<(), Fail> {
    let address = ctx.opt.address.ok_or_else(|| Fail::new("no address"))?;
    if let Some(old) = ctx.oracle.take() {
        old.close().await;
    }
    ctx.oracle = Some(Oracle::connect(address, &ctx.opt.credentials).await?);
    // Time for the new connection to get the state of the Apple TV, like in `states`.
    ctx.client.listen(3).await
}

/// Driver log lines when the connection to the Apple TV closes.
/// The `]` before "connection closed" leaves out "Home Screen clear ended: connection closed".
const CLOSED: [&str; 3] = ["] connection closed", "Companion closed", "Mrp closed"];

/// The owner restarts the Apple TV. The driver must connect again. Only with `--only`.
pub async fn atv_restart(ctx: &mut Ctx) -> Check {
    turn_on(ctx).await?;
    let mark = ctx.driver.log_len();
    let text = "Restart the Apple TV: Settings > System > Restart.";
    let keys = Ask::Action {
        done: "the restart started",
        skip: SKIP_CHECK,
    };
    match ask(ctx, keys, text).await? {
        Answer::Done => {}
        Answer::NoTerminal => return Err(Fail::skip(NO_TERMINAL)),
        Answer::NoInput => return Err(Fail::skip("no restart (no input)")),
        Answer::Skip | Answer::Yes | Answer::No => return Err(Fail::skip("no restart")),
    }
    let start = Instant::now();
    let deadline = start + Duration::from_secs(300);
    while !driver_log(&ctx.driver, mark).has_any(&CLOSED) {
        ensure!(Instant::now() < deadline, "no closed line in 300 s");
        ctx.client.listen_ms(200).await?;
    }
    let closed_ms = start.elapsed().as_millis();
    let joined_ms = loop {
        // Only a connect after the closed line counts.
        let log = driver_log(&ctx.driver, mark);
        if log
            .first_with(&CLOSED)
            .is_some_and(|at| joined(&log.lines_from(at)))
        {
            break start.elapsed().as_millis();
        }
        ensure!(
            Instant::now() < deadline,
            "closed after {closed_ms} ms, no full connect in 300 s"
        );
        ctx.client.listen_ms(200).await?;
    };
    ctx.client.refresh_states().await?;
    let left = deadline.saturating_duration_since(Instant::now()).as_secs();
    let state = wait_available(ctx, left.max(10)).await?;
    let available = start.elapsed().as_millis();
    // The second connection closed with the restart.
    let oracle = match reconnect_oracle(ctx).await {
        Ok(()) => "",
        Err(_) => ", second connection not open again",
    };
    Ok(format!(
        "closed after {closed_ms} ms, connected after {joined_ms} ms, {state} after \
         {available} ms{oracle}"
    ))
}

/// Bundle id of YouTube.
const YOUTUBE: &str = "com.google.ios.youtube";

/// Time from the start of `select_source` in which `source` must stay the opened app. It ends
/// before the driver's `LAUNCH_GRACE` (5 s): after that, a screen event clears the opened app on
/// purpose.
const LAUNCH_WINDOW_MS: u64 = 4500;

/// A video in a third-party app (YouTube) shows its app and title on the driver. The owner opens
/// the app, so the driver must get the app from the Apple TV, not from its own launch. Then, with
/// the video paused, `select_source` opens Settings and `source` must stay Settings for
/// [`LAUNCH_WINDOW_MS`]. Needs a person, so only with `--only third_party`.
pub async fn third_party(ctx: &mut Ctx) -> Check {
    if !terminal() {
        return Err(Fail::skip(NO_TERMINAL));
    }
    turn_on(ctx).await?;
    let o = oracle(ctx)?;
    if !o.has_mrp() {
        return Err(Fail::skip("no MRP on the second connection"));
    }
    let youtube = o
        .app_name(YOUTUBE)
        .map(str::to_owned)
        .ok_or_else(|| Fail::skip("YouTube is not installed"))?;
    let settings = o
        .app_name(SETTINGS)
        .map(str::to_owned)
        .ok_or_else(|| Fail::new("no Settings app"))?;
    let mp = ctx.mp();
    ensure!(
        has_value(ctx, &mp, "source_list", &youtube),
        "{youtube} is not in the source list"
    );
    let text = format!(
        "On the Apple TV, open {youtube}, start a video and wait until the video plays, after \
         any ads."
    );
    let keys = Ask::Action {
        done: "it plays",
        skip: SKIP_CHECK,
    };
    match ask(ctx, keys, &text).await? {
        Answer::Done => {}
        Answer::NoTerminal => return Err(Fail::skip(NO_TERMINAL)),
        Answer::NoInput => return Err(Fail::skip("no video started (no input)")),
        Answer::Skip | Answer::Yes | Answer::No => return Err(Fail::skip("no video started")),
    }
    // The second connection first: without app and title there, the driver cannot show them.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let o = oracle(ctx)?;
        if o.active_app() == Some(youtube.as_str()) && o.missing(Content::Playing).is_none() {
            break;
        }
        if Instant::now() >= deadline {
            let now = o.now_text().unwrap_or_else(|| "nothing".into());
            let app = o.app_text().unwrap_or_else(|| "none".into());
            return Err(Fail::new(format!(
                "the second connection shows {now} (app {app}) after 30 s, not a {youtube} item \
                 that plays and has a title"
            )));
        }
        ctx.client.listen_ms(500).await?;
    }
    ctx.client.wait_text(&mp, "state", 10, "PLAYING").await?;
    ctx.client.wait_text(&mp, "source", 10, &youtube).await?;
    let title = ctx
        .client
        .wait_attr(&mp, "media_title", 10, |v| {
            v.as_str().is_some_and(|t| !t.is_empty())
        })
        .await?;
    let title = title.as_str().unwrap_or_default().to_owned();
    mp_command(ctx, "PAUSE", json!({})).await?;
    ctx.client.wait_text(&mp, "state", 5, "PAUSED").await?;
    wait_playback(ctx, PlaybackState::Paused).await?;
    // The paused item should stay in now playing. An MRP update must not replace the opened app.
    let opened = Instant::now();
    let end = opened + Duration::from_millis(LAUNCH_WINDOW_MS);
    mp_command(ctx, "select_source", json!({ "source": settings })).await?;
    let shown_by = opened + Duration::from_secs(10);
    while ctx.client.attr(&mp, "source").and_then(Value::as_str) != Some(settings.as_str()) {
        ensure!(
            Instant::now() < shown_by,
            "{settings} not shown in 10 s, source {:?}",
            ctx.client.text(&mp, "source")
        );
        ctx.client.listen_ms(100).await?;
    }
    let shown = opened.elapsed().as_millis();
    if Instant::now() >= end {
        return Err(Fail::skip(format!(
            "{settings} shown after {shown} ms, no time left to check the source"
        )));
    }
    while Instant::now() < end {
        ctx.client.listen_ms(200).await?;
        if ctx.client.attr(&mp, "source").and_then(Value::as_str) != Some(settings.as_str()) {
            let source = ctx.client.text(&mp, "source");
            let o = oracle(ctx)?;
            let now = o.now_text().unwrap_or_else(|| "nothing".into());
            let app = o.app_text().unwrap_or_else(|| "none".into());
            return Err(Fail::new(format!(
                "source changed from {settings} to {source:?} after {} ms with {youtube} \
                 paused, the second connection shows {now} (app {app})",
                opened.elapsed().as_millis()
            )));
        }
    }
    // Without the paused item, nothing from YouTube can update now playing in the window, so the
    // check proves less.
    let item = match oracle(ctx)?.has_item() {
        Some(true) => "the paused item stayed",
        Some(false) => "the paused item is gone",
        None => "item unknown: no MRP on the second connection",
    };
    press(ctx, "HOME").await?;
    Ok(format!(
        "{youtube} shows {title:?} on the driver, {settings} shown after {shown} ms and stayed \
         until {LAUNCH_WINDOW_MS} ms, {item}"
    ))
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
    let text = RawLog::whole(&ctx.driver);
    // After a failed setup the driver may have no config, so also use the source file.
    let mut credentials = Vec::new();
    for path in [ctx.driver.config_file(), ctx.opt.config.clone()] {
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Fail::new(format!("{}: {e}", path.display()))),
        };
        let config: Value = serde_json::from_str(&text)
            .map_err(|e| Fail::new(format!("{}: {e}", path.display())))?;
        credentials.extend(
            config
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["credentials"].as_str().map(str::to_owned)),
        );
    }
    ensure!(
        !credentials.is_empty(),
        "no credentials to search for in the log"
    );
    // A part of the secret is enough to find it.
    ensure!(
        !credentials
            .iter()
            .any(|c| c.len() > 24 && text.contains_anywhere(&c[c.len() - 24..])),
        "the log contains the credentials"
    );
    ensure!(!text.contains_anywhere("panicked"), "the driver panicked");
    // The broken file check causes one ERROR line.
    let errors: Vec<&str> = text
        .level_lines("ERROR")
        .into_iter()
        .filter(|l| !l.contains("config.json.bad-"))
        .collect();
    let warnings = text.level_lines("WARN");
    for line in errors.iter().chain(&warnings) {
        println!("      {line}");
    }
    ensure!(errors.is_empty(), "{} ERROR lines in the log", errors.len());
    Ok(format!(
        "{} warnings, no errors, no credentials. Log: {}",
        warnings.len(),
        ctx.driver.log_display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::log::test_line as line;

    fn log_of(raw: &str) -> DriverLog {
        DriverLog::from_raw(raw)
    }

    fn quick_wake_of(raw: &str) -> Result<(), String> {
        after_quick_wake(&log_of(raw))
    }

    fn joined_of(raw: &str) -> bool {
        joined(&log_of(raw))
    }

    fn clear_ended_of(raw: &str) -> bool {
        clear_ended(&log_of(raw))
    }

    #[test]
    fn clear_ended_uses_the_last_line() {
        let clear = line(HOME_CLEAR);
        let left = line(HOME_LEFT);
        let closed = line(HOME_CLOSED);
        assert!(!clear_ended_of(""));
        assert!(!clear_ended_of(&clear));
        assert!(clear_ended_of(&left));
        assert!(clear_ended_of(&closed));
        assert!(clear_ended_of(&format!("{clear}{left}")));
        assert!(!clear_ended_of(&format!("{left}{clear}")));
        assert!(clear_ended_of(&format!("{left}{clear}{closed}")));
        assert!(!clear_ended_of(&format!("{closed}{clear}")));
        assert!(!clear_ended_of(&format!("{clear}{closed}{clear}")));
    }

    #[test]
    fn voice_messages() {
        let begin = [
            0x0a, 0x0d, 0x08, 0x07, 0x12, 0x09, 0x08, 0x01, 0x10, 0x80, 0x7d, 0x18, 0x02, 0x20,
            0x01,
        ];
        assert_eq!(begin.as_slice(), voice_message(&Voice::Begin, 7));
        assert_eq!(
            [0x12, 0x06, 0x08, 0x07, 0x12, 0x02, 0x01, 0x02].as_slice(),
            voice_message(&Voice::Data(&[1, 2]), 7)
        );
        assert_eq!(
            [0x1a, 0x02, 0x08, 0x07].as_slice(),
            voice_message(&Voice::End, 7)
        );
        // A session id of more than 7 bits needs two bytes.
        assert_eq!(
            [0x1a, 0x03, 0x08, 0xe7, 0x24].as_slice(),
            voice_message(&Voice::End, 4711)
        );
    }

    /// A WAV file with a `JUNK` chunk of odd size before `fmt `, like `say` writes one.
    fn wav(channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut w = b"RIFF\0\0\0\0WAVE".to_vec();
        w.extend_from_slice(b"JUNK\x03\0\0\0abc\0");
        w.extend_from_slice(b"fmt \x10\0\0\0");
        w.extend_from_slice(&1_u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 2).to_le_bytes());
        w.extend_from_slice(&2_u16.to_le_bytes());
        w.extend_from_slice(&16_u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        w.extend_from_slice(data);
        w
    }

    #[test]
    fn quiet_trimmed() {
        let pcm = |s: &[i16]| s.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let block = |v: i16| vec![v; QUIET_BLOCK];
        let blocks = |v: i16, n: usize| vec![v; n * QUIET_BLOCK];
        // Room noise up to 450, below 1/16 of the peak: quiet.
        let mut samples = [blocks(300, 5), block(-450)].concat();
        let lead = samples.len();
        // Speech of 4 loud blocks, one with quiet samples inside.
        samples.extend([block(9000), block(-3000), block(10), block(i16::MIN)].concat());
        samples[lead + 2 * QUIET_BLOCK] = 5000;
        let speech_end = samples.len();
        // A quiet tail, a click over a block border, then quiet again.
        samples.extend(blocks(-400, 50));
        let click = samples.len() - QUIET_BLOCK / 2;
        samples.extend(blocks(0, 3));
        samples[click..click + QUIET_BLOCK].fill(20_000);
        let audio = pcm(&samples);
        assert_eq!(trim_quiet(&audio), &audio[lead * 2..speech_end * 2]);
        // 2 loud blocks are not speech.
        assert!(trim_quiet(&pcm(&blocks(3000, 2))).is_empty());
        assert!(trim_quiet(&pcm(&[1, -2, 499])).is_empty());
        assert!(trim_quiet(&[]).is_empty());
    }

    #[test]
    fn wav_samples() {
        assert_eq!(
            Ok(vec![1, 2, 3, 4]),
            wav_pcm(&wav(1, 16_000, &[1, 2, 3, 4])).map_err(|e| e.msg)
        );
        assert!(wav_pcm(&wav(2, 16_000, &[1, 2])).is_err(), "stereo");
        assert!(wav_pcm(&wav(1, 44_100, &[1, 2])).is_err(), "other rate");
        assert!(wav_pcm(b"RIFF").is_err(), "short");
        let mut cut = wav(1, 16_000, &[1, 2, 3, 4]);
        cut.truncate(cut.len() - 1);
        assert!(wav_pcm(&cut).is_err(), "cut data");
        // `wav` has a 12-byte JUNK chunk, so the format tag is at 32.
        let mut float = wav(1, 16_000, &[1, 2]);
        float[32..34].copy_from_slice(&3_u16.to_le_bytes());
        assert!(wav_pcm(&float).is_err(), "not PCM");
    }

    #[test]
    fn full_connect_lines() {
        assert!(joined_of(
            "[t INFO  uc_intg_appletv::device::actor] [ATV] connected in 812 ms"
        ));
        assert!(joined_of(
            "[t INFO  uc_intg_appletv] [ATV] MRP connected in 1200 ms"
        ));
        assert!(!joined_of(
            "[t INFO  uc_intg_appletv] [ATV] Companion connected in 300 ms (mDNS), MRP next"
        ));
        assert!(
            !joined_of("[t DEBUG ratv] [ATV] connected in 80 ms"),
            "ratv line"
        );
        assert!(
            !joined_of("INFO x] [ATV] connected in 812 ms"),
            "no line start"
        );
        let both = "[t INFO  uc_intg_appletv] [ATV] Companion and MRP connected in 50 ms";
        assert!(joined_of(both), "MRP after Companion in one line");
        assert_eq!(0, log_of(both).count(&CONNECT_LINES));
    }

    #[test]
    fn quick_gaps_cover_the_connect() {
        let awake =
            |range: std::ops::Range<u64>| QUICK_GAPS_MS.iter().any(|g| range.contains(&g.0));
        assert!(awake(0..30), "a standby before Companion is open");
        assert!(
            awake(30..100),
            "a standby while Companion opens or MRP joins"
        );
        assert!(awake(300..1000), "a standby after the full connect");
        assert!(
            QUICK_GAPS_MS.iter().any(|&(a, s)| a >= 100 && s == 0),
            "a wake while the old connection closes"
        );
    }

    #[test]
    fn after_quick_wake_needs_one_connect() {
        let full = line("[ATV] connected in 80 ms (known)");
        let full = full.as_str();
        assert_eq!(Ok(()), quick_wake_of(full));
        let joined = format!(
            "{}{}",
            line("[ATV] Companion connected in 30 ms (known), MRP next"),
            line("[ATV] MRP connected in 60 ms")
        );
        let joined = joined.as_str();
        assert_eq!(Ok(()), quick_wake_of(joined), "Companion, then MRP");
        let added = line("[ATV] Companion connected in 40 ms");
        assert_eq!(0, log_of(&added).count(&CONNECT_LINES), "added later");
        let mrp_first = format!("{}{added}", line("[ATV] connected in 50 ms (known)"));
        assert_eq!(
            Ok(()),
            quick_wake_of(&mrp_first),
            "Companion added after MRP"
        );
        assert_eq!(
            1,
            log_of(&format!("{joined}{added}")).count(&CONNECT_LINES),
            "Companion first"
        );
        assert_eq!(
            Err("0 connects after the last exit_standby, expected 1: ".to_owned()),
            quick_wake_of("")
        );
        let two = format!("{joined}{full}");
        assert_eq!(
            Err(format!(
                "2 connects after the last exit_standby, expected 1: {} | {}",
                line("[ATV] Companion connected in 30 ms (known), MRP next").trim_end(),
                full.trim_end()
            )),
            quick_wake_of(&two)
        );
        let warn = |message: &str| line(message).replacen("INFO ", "WARN ", 1);
        let other = format!(
            "{full}[t DEBUG ratv::apple_tv] connection failed protocol=mrp\n\
             [t DEBUG ratv::mrp] connection closed\n\
             [t WARN  ratv::mrp] connection failed\n\
             [t ERROR ratv::apple_tv] broken\n{}",
            line("[ATV] not found on the network")
        );
        assert_eq!(
            Ok(()),
            quick_wake_of(&other),
            "ratv lines and driver INFO lines do not count"
        );
        for bad in [
            warn("[ATV] MRP did not connect: timeout. No Siri"),
            warn("[ATV] connect failed: timeout"),
            warn("[ATV] not connected: Mrp. No Siri"),
            line("[ATV] broken").replacen("INFO ", "ERROR", 1),
            line("[ATV] connection closed"),
            line("[ATV] Companion closed. No apps"),
        ] {
            let log = format!("{full}{bad}");
            assert_eq!(Err(bad.trim_end().to_owned()), quick_wake_of(&log), "{bad}");
        }
        // A WARN line about a closed connection is shown once.
        let both = warn("[ATV] connection closed: reset");
        assert_eq!(
            Err(both.trim_end().to_owned()),
            quick_wake_of(&format!("{full}{both}"))
        );
    }

    /// A new empty folder in the temp folder for one test.
    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remote-sim-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test folder");
        dir
    }

    #[tokio::test]
    async fn settings_are_set_back() {
        let dir = test_dir("settings");
        let path = dir.join(SETTINGS_FILE);
        std::fs::write(&path, r#"{"debug_log":false}"#).expect("write");
        let before = br#"{"debug_log":true,"siri_mode":"auto"}"#;
        assert!(restore_settings(&path, Some(before)).await.is_ok());
        assert_eq!(before.as_slice(), std::fs::read(&path).expect("read"));
        assert!(restore_settings(&path, None).await.is_ok());
        assert!(!path.exists(), "a file that did not exist is removed");
        assert!(restore_settings(&path, None).await.is_ok(), "no file");
        // A folder at the path cannot be written or removed as a file.
        std::fs::create_dir(&path).expect("folder");
        for before in [Some(before.as_slice()), None] {
            let error = restore_settings(&path, before).await.expect_err("folder");
            assert!(error.msg.contains("not set back"), "{}", error.msg);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_keep_other_values() {
        let before = br#"{"debug_log":true,"siri_mode":"mrp"}"#;
        assert_eq!(
            json!({ "debug_log": false, "siri_mode": "mrp" }),
            settings_with_debug_log(Some(before), DebugLog::Off)
        );
        assert_eq!(
            json!({ "debug_log": true }),
            settings_with_debug_log(Some(b"{broken"), DebugLog::On)
        );
        assert_eq!(
            json!({ "debug_log": false }),
            settings_with_debug_log(Some(b"[1]"), DebugLog::Off)
        );
        assert_eq!(
            json!({ "debug_log": false }),
            settings_with_debug_log(None, DebugLog::Off)
        );
    }

    #[test]
    fn closed_lines_count_after_the_mark() {
        let dir = test_dir("closed");
        let driver = Driver::new(std::path::Path::new("none"), &dir, 0);
        let start = LogMark::at(0);
        assert!(closed_lines(&driver, start).is_empty(), "no log");
        let first = line("[A] connection closed");
        driver.append_test_log(&first);
        let mark = driver.log_len();
        assert_eq!(first.trim_end(), closed_lines(&driver, start));
        assert!(closed_lines(&driver, mark).is_empty(), "an earlier line");
        let mut log = String::new();
        for message in [
            "[A] connected in 20 ms",
            "[A] connection closed: reset",
            "[A] Companion closed. x",
            "[A] Mrp closed: y. z",
        ] {
            log.push_str(&line(message));
        }
        log.push_str("[t DEBUG ratv::mrp] connection closed\n");
        // A driver line about the Home Screen, not about the connection.
        log.push_str(&line(HOME_CLOSED).replacen("INFO ", "DEBUG", 1));
        driver.append_test_log(&log);
        let after = [
            line("[A] connection closed: reset"),
            line("[A] Companion closed. x"),
            line("[A] Mrp closed: y. z"),
        ]
        .map(|l| l.trim_end().to_owned())
        .join(" | ");
        assert_eq!(after, closed_lines(&driver, mark));
        assert_eq!(
            format!("{} | {after}", first.trim_end()),
            closed_lines(&driver, start)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_keys_and_answers() {
        let action = Ask::Action {
            done: "done",
            skip: SKIP_CHECK,
        };
        let question = Ask::Question {
            yes: "yes",
            no: "no",
            skip: "do not wait",
        };
        assert_eq!(
            "Enter = done, s + Enter = skip this check",
            keys_text(action)
        );
        assert_eq!(
            "y + Enter = yes, n + Enter = no, s + Enter = do not wait",
            keys_text(question)
        );
        assert_eq!(Some(Answer::Done), parse_answer(action, ""));
        assert_eq!(Some(Answer::Skip), parse_answer(action, " S "));
        assert_eq!(None, parse_answer(action, "y"));
        assert_eq!(Some(Answer::Yes), parse_answer(question, "Y"));
        assert_eq!(Some(Answer::No), parse_answer(question, "n"));
        assert_eq!(Some(Answer::Skip), parse_answer(question, "s"));
        assert_eq!(None, parse_answer(question, ""));
    }
}
