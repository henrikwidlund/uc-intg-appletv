// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Test program that acts as the Remote. It starts the driver, sets it up with a real Apple TV
//! and runs the checks of the guided manual test that need no person over the WebSocket.
//!
//! The driver pairing comes from a `config.json` of the driver: the setup restores it as a
//! backup. Values are compared with a second connection that has its own pairing, see
//! [`oracle`].
//!
//! Usage:
//! - `remote-sim pair <address>`: pair the second connection one time.
//! - `remote-sim pair-driver <address>`: pair the driver through its setup flow and save its
//!   `config.json` as `--config`. An old file is kept as `<config>.old-<unix time>`.
//! - `remote-sim <address> [--config <path>] [--driver <path>] [--credentials <path>]
//!   [--only a,b] [--skip a,b] [--no-power] [--port <port>]`

mod checks;
mod driver;
mod oracle;
mod ws;

use driver::Driver;
use std::fmt::{self, Display};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Instant;
use ws::Client;

/// All steps in the order of [`run`].
const STEPS: [&str; 25] = [
    "metadata",
    "setup_input",
    "setup",
    "entities",
    "states",
    "values",
    "remote_ui",
    "standby",
    "remote_wake",
    "power",
    "remote_commands",
    "simple_commands",
    "app_select",
    "volume",
    "playback",
    "languages",
    "artwork",
    "abort_setup",
    "configure",
    "backup_restore",
    "driver_restart",
    "config_file",
    "broken_config",
    "restore_power",
    "log",
];
/// Steps that set up the driver and connect it. The other checks need them.
const BASE_STEPS: [&str; 3] = ["setup", "entities", "states"];
/// Steps at the end that set the Apple TV back and check the driver log.
const END_STEPS: [&str; 2] = ["restore_power", "log"];

/// A step runs if `--only` is empty or names it, or if it is in [`BASE_STEPS`] or
/// [`END_STEPS`]. `--skip` wins.
fn selected(only: &[String], skip: &[String], name: &str) -> bool {
    (only.is_empty()
        || BASE_STEPS.contains(&name)
        || END_STEPS.contains(&name)
        || only.iter().any(|n| n == name))
        && !skip.iter().any(|n| n == name)
}

const USAGE: &str = "Usage:
  remote-sim pair <address> [--credentials <path>]
  remote-sim pair-driver <address> [--config <path>] [--driver <path>] [--port <port>]
  remote-sim <address> [--config <path>] [--driver <path>] [--credentials <path>]
             [--only a,b] [--skip a,b] [--no-power] [--port <port>]";

/// A failed or skipped check.
pub struct Fail {
    pub msg: String,
    pub skip: bool,
}

impl Fail {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            skip: false,
        }
    }

    pub fn skip(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            skip: true,
        }
    }
}

impl<E: std::error::Error> From<E> for Fail {
    fn from(e: E) -> Self {
        Self::new(e.to_string())
    }
}

impl Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

/// Result of a check: a note for the report.
pub type Check = Result<String, Fail>;

pub struct Options {
    pub pair: bool,
    pub pair_driver: bool,
    pub address: Option<IpAddr>,
    /// `config.json` of the driver with the pairing to use.
    pub config: PathBuf,
    pub bin: PathBuf,
    /// Pairing of the second connection.
    pub credentials: PathBuf,
    pub only: Vec<String>,
    pub skip: Vec<String>,
    pub no_power: bool,
    pub port: u16,
}

pub struct Ctx {
    pub opt: Options,
    pub client: Client,
    pub driver: Driver,
    /// The second connection to the Apple TV.
    pub oracle: Option<oracle::Oracle>,
    /// Device key in the entity ids.
    pub key: String,
    /// The Apple TV was on at the start.
    pub was_on: bool,
    report: Vec<String>,
    failed: usize,
    skipped: usize,
}

impl Ctx {
    pub fn mp(&self) -> String {
        format!("media_player.{}", self.key)
    }

    pub fn remote(&self) -> String {
        format!("remote.{}", self.key)
    }

    pub fn voice(&self) -> String {
        format!("voice_assistant.{}", self.key)
    }

    pub fn select(&self, kind: &str) -> String {
        format!("select.{}.{kind}", self.key)
    }

    pub fn sensor(&self, kind: &str) -> String {
        format!("sensor.{}.{kind}", self.key)
    }

    /// Connect a new Remote, like a Remote that wakes up.
    pub async fn reconnect(&mut self) -> Result<(), Fail> {
        self.client = Client::connect(self.driver.port).await?;
        self.client.send_event("connect").await
    }

    fn selected(&self, name: &str) -> bool {
        selected(&self.opt.only, &self.opt.skip, name)
    }

    /// Run a check and print the result. Returns false for a failure.
    async fn step(&mut self, name: &str, check: impl AsyncFnOnce(&mut Self) -> Check) -> bool {
        if !self.selected(name) {
            return true;
        }
        let start = Instant::now();
        let result = check(self).await;
        let ms = start.elapsed().as_millis();
        let (tag, note) = match result {
            Ok(note) => ("PASS", note),
            Err(f) if f.skip => {
                self.skipped += 1;
                ("SKIP", f.msg)
            }
            Err(f) => {
                self.failed += 1;
                ("FAIL", f.msg)
            }
        };
        let line = if note.is_empty() {
            format!("{tag}  {name} ({ms} ms)")
        } else {
            format!("{tag}  {name} ({ms} ms): {note}")
        };
        println!("{line}");
        self.report.push(line);
        tag != "FAIL"
    }
}

fn parse() -> Result<Options, String> {
    let mut args = std::env::args().skip(1).peekable();
    let mut opt = Options {
        pair: false,
        pair_driver: false,
        address: None,
        config: PathBuf::from("config.json"),
        bin: PathBuf::from("target/release/uc-intg-appletv"),
        credentials: PathBuf::from("remote-sim-credentials"),
        only: Vec::new(),
        skip: Vec::new(),
        no_power: false,
        port: 19190,
    };
    match args.peek().map(String::as_str) {
        Some("pair") => opt.pair = true,
        Some("pair-driver") => opt.pair_driver = true,
        _ => {}
    }
    if opt.pair || opt.pair_driver {
        args.next();
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => opt.config = args.next().ok_or("--config needs a path")?.into(),
            "--driver" => opt.bin = args.next().ok_or("--driver needs a path")?.into(),
            "--credentials" => {
                opt.credentials = args.next().ok_or("--credentials needs a path")?.into();
            }
            "--only" => opt.only.extend(step_list("--only", args.next())?),
            "--skip" => opt.skip.extend(step_list("--skip", args.next())?),
            "--no-power" => opt.no_power = true,
            "--port" => {
                opt.port = args
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or("--port needs a number")?;
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => {
                opt.address = Some(
                    other
                        .parse()
                        .map_err(|_| format!("{other} is not an IP address"))?,
                );
            }
        }
    }
    if opt.address.is_none() {
        return Err("the Apple TV address is missing".into());
    }
    Ok(opt)
}

/// Step names of `--only` or `--skip`, separated by commas. Refuses a missing value and unknown
/// or empty names.
fn step_list(flag: &str, value: Option<String>) -> Result<Vec<String>, String> {
    let value = value.ok_or_else(|| format!("{flag} needs step names"))?;
    value
        .split(',')
        .map(|name| {
            if STEPS.contains(&name) {
                Ok(name.to_owned())
            } else {
                Err(format!(
                    "{flag}: unknown step {name:?}. Steps: {}",
                    STEPS.join(", ")
                ))
            }
        })
        .collect()
}

fn exit(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = parse().unwrap_or_else(|e| exit(&format!("{e}\n{USAGE}")));
    let Some(address) = opt.address else {
        exit(USAGE)
    };
    if opt.pair {
        if let Err(e) = oracle::pair(address, &opt.credentials).await {
            exit(&format!("pairing failed: {e}"));
        }
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = std::env::temp_dir().join(format!("remote-sim-{stamp}"));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        exit(&format!("cannot create {}: {e}", dir.display()));
    }
    let mut driver = Driver::new(&opt.bin, &dir, opt.port);
    if let Err(e) = driver.start().await {
        exit(&e.msg);
    }
    let client = Client::connect(opt.port)
        .await
        .unwrap_or_else(|e| exit(&e.msg));
    println!("Driver log and configuration: {}", dir.display());
    let mut ctx = Ctx {
        opt,
        client,
        driver,
        oracle: None,
        key: String::new(),
        was_on: true,
        report: Vec::new(),
        failed: 0,
        skipped: 0,
    };
    if ctx.opt.pair_driver {
        let result = pair_driver(&mut ctx, stamp).await;
        ctx.driver.stop().await;
        if let Err(e) = result {
            exit(&format!("driver pairing failed: {e}"));
        }
        return;
    }
    run(&mut ctx).await;
    if let Some(oracle) = &ctx.oracle {
        oracle.close().await;
    }
    ctx.driver.stop().await;
    let summary = format!("{} checks failed, {} skipped", ctx.failed, ctx.skipped);
    println!("\n{summary}");
    ctx.report.push(String::new());
    ctx.report.push(summary);
    let file = format!("remote-sim-results-{stamp}.md");
    if tokio::fs::write(&file, ctx.report.join("\n") + "\n")
        .await
        .is_ok()
    {
        println!("Results: {file}");
    }
    std::process::exit(i32::from(ctx.failed > 0));
}

/// Pair the driver and save its `config.json` as `--config`.
async fn pair_driver(ctx: &mut Ctx, stamp: u64) -> Result<(), Fail> {
    checks::pair_driver(ctx).await?;
    let source = ctx.driver.config_file();
    let text = tokio::fs::read_to_string(&source).await?;
    let target = &ctx.opt.config;
    // Write the new file first, so a failed write keeps the old pairing in place.
    let mut tmp = target.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // A new file gets mode 600; an old file would keep its mode.
    if let Err(e) = tokio::fs::remove_file(&tmp).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(Fail::new(format!("{}: {e}", tmp.display())));
    }
    oracle::write_private(&tmp, &text).map_err(|e| {
        Fail::new(format!(
            "{}: {e}. The new pairing is in {}",
            tmp.display(),
            source.display()
        ))
    })?;
    // Copy the old file away and replace it in one step, so a file is always in place.
    let kept = |e: std::io::Error, path: &std::path::Path| {
        Fail::new(format!(
            "{}: {e}. The new pairing is in {}",
            path.display(),
            tmp.display()
        ))
    };
    if tokio::fs::try_exists(target)
        .await
        .map_err(|e| kept(e, target))?
    {
        let mut old = target.clone().into_os_string();
        old.push(format!(".old-{stamp}"));
        let old = PathBuf::from(old);
        tokio::fs::copy(target, &old)
            .await
            .map_err(|e| kept(e, &old))?;
        println!("Old pairing kept in {}", old.display());
    }
    tokio::fs::rename(&tmp, target)
        .await
        .map_err(|e| Fail::new(format!("{} -> {}: {e}", tmp.display(), target.display())))?;
    println!("Saved the driver pairing to {}", target.display());
    Ok(())
}

async fn run(ctx: &mut Ctx) {
    ctx.step("metadata", checks::metadata).await;
    ctx.step("setup_input", checks::setup_input).await;
    // A failed setup or connect makes the other checks fail, so only the end steps run then.
    if ctx.step("setup", checks::setup_restore).await
        && ctx.step("entities", checks::entities).await
        && ctx.step("states", checks::states).await
    {
        middle_steps(ctx).await;
    }
    ctx.step("restore_power", checks::restore_power).await;
    ctx.step("log", checks::log).await;
}

/// The checks between the base steps and the end steps.
async fn middle_steps(ctx: &mut Ctx) {
    ctx.step("values", checks::values).await;
    ctx.step("remote_ui", checks::remote_ui).await;
    ctx.step("standby", checks::standby).await;
    ctx.step("remote_wake", checks::remote_wake).await;
    ctx.step("power", checks::power).await;
    ctx.step("remote_commands", checks::remote_commands).await;
    ctx.step("simple_commands", checks::simple_commands).await;
    ctx.step("app_select", checks::app_select).await;
    ctx.step("volume", checks::volume).await;
    ctx.step("playback", checks::playback).await;
    ctx.step("languages", checks::languages).await;
    ctx.step("artwork", checks::artwork).await;
    ctx.step("abort_setup", checks::abort_setup).await;
    ctx.step("configure", checks::configure).await;
    ctx.step("backup_restore", checks::backup_restore).await;
    ctx.step("driver_restart", checks::driver_restart).await;
    ctx.step("config_file", checks::config_file).await;
    ctx.step("broken_config", checks::broken_config).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_selection() {
        let names = |v: &[&str]| v.iter().map(|&n| n.to_owned()).collect::<Vec<_>>();
        assert!(selected(&[], &[], "playback"), "no --only runs all");
        let only = names(&["playback"]);
        for name in [
            "playback",
            "setup",
            "entities",
            "states",
            "restore_power",
            "log",
        ] {
            assert!(selected(&only, &[], name), "{name}");
        }
        assert!(!selected(&only, &[], "languages"));
        assert!(!selected(&only, &names(&["setup"]), "setup"), "--skip wins");
        assert!(!selected(&[], &names(&["log"]), "log"));
    }

    /// Body of the fn that starts with `head`.
    fn body<'a>(src: &'a str, head: &str) -> &'a str {
        let start = src.find(head).unwrap_or_else(|| panic!("{head}"));
        let len = src[start..].find("\n}\n").expect("end of fn");
        &src[start..start + len]
    }

    /// `STEPS` has the steps of `run` and `middle_steps` in the order they run.
    #[test]
    fn steps_match_run() {
        let src = include_str!("main.rs");
        let middle = body(src, "async fn middle_steps(");
        let run = body(src, "async fn run(").replace("middle_steps(ctx).await", middle);
        let mut last = 0;
        for name in STEPS {
            let at = run
                .find(&format!("ctx.step(\"{name}\""))
                .unwrap_or_else(|| panic!("{name} not in run"));
            assert!(at > last, "{name} out of order");
            last = at;
        }
        let in_run = run.matches("ctx.step(\"").count();
        assert_eq!(STEPS.len(), in_run, "a step of run is not in STEPS");
        // The base steps run before the middle steps; the end steps run last.
        let middle_at = run.find("async fn middle_steps(").expect("middle_steps");
        for name in BASE_STEPS {
            let at = run.find(&format!("ctx.step(\"{name}\"")).expect(name);
            assert!(at < middle_at, "{name} is not a base step");
        }
        assert!(STEPS.ends_with(&END_STEPS), "end steps are not last");
    }

    #[test]
    fn step_names() {
        let only = |v: &str| step_list("--only", Some(v.to_owned()));
        assert_eq!(
            Ok(vec!["playback".to_owned(), "log".to_owned()]),
            only("playback,log")
        );
        assert!(step_list("--only", None).is_err(), "missing value");
        for bad in ["", "playbak", "playback,", "a,,b"] {
            assert!(only(bad).is_err(), "{bad:?}");
        }
    }
}
