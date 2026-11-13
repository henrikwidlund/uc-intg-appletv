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
//! - `remote-sim <address> [--config <path>] [--driver <path>] [--only a,b] [--skip a,b]
//!   [--no-power] [--port <port>]`

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

const USAGE: &str = "Usage:
  remote-sim pair <address> [--credentials <path>]
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
        (self.opt.only.is_empty() || self.opt.only.iter().any(|n| n == name))
            && !self.opt.skip.iter().any(|n| n == name)
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
        address: None,
        config: PathBuf::from("config.json"),
        bin: PathBuf::from("target/release/uc-intg-appletv"),
        credentials: PathBuf::from("remote-sim-credentials"),
        only: Vec::new(),
        skip: Vec::new(),
        no_power: false,
        port: 19190,
    };
    if args.peek().is_some_and(|a| a == "pair") {
        opt.pair = true;
        args.next();
    }
    let list = |v: Option<String>| -> Vec<String> {
        v.unwrap_or_default()
            .split(',')
            .map(str::to_owned)
            .collect()
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => opt.config = args.next().ok_or("--config needs a path")?.into(),
            "--driver" => opt.bin = args.next().ok_or("--driver needs a path")?.into(),
            "--credentials" => {
                opt.credentials = args.next().ok_or("--credentials needs a path")?.into();
            }
            "--only" => opt.only = list(args.next()),
            "--skip" => opt.skip = list(args.next()),
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
    if let Err(e) = std::fs::create_dir_all(&dir) {
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
    if std::fs::write(&file, ctx.report.join("\n") + "\n").is_ok() {
        println!("Results: {file}");
    }
    std::process::exit(i32::from(ctx.failed > 0));
}

async fn run(ctx: &mut Ctx) {
    ctx.step("metadata", checks::metadata).await;
    ctx.step("setup_input", checks::setup_input).await;
    // A failed setup or connect makes all other checks fail, so the run ends there.
    if !ctx.step("setup", checks::setup_restore).await
        || !ctx.step("entities", checks::entities).await
        || !ctx.step("states", checks::states).await
    {
        return;
    }
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
    ctx.step("restore_power", checks::restore_power).await;
    ctx.step("log", checks::log).await;
}
