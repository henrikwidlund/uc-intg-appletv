// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Test program that acts as the Remote. It starts the driver, sets it up with a real Apple TV
//! and runs the checks over the WebSocket. Some checks ask the owner on the terminal, for example
//! to start the right video. Without a terminal they skip the question. `atv_restart` runs only
//! with `--only atv_restart`.
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
//!   [--only a,b] [--skip a,b] [--no-power] [--port <port>] [--siri-audio <file>]
//!   [--siri-hold]`
//!
//! `--siri-hold` sends 1500 ms of silence after the speech instead of 300 ms, like a user who
//! holds the button past the end of listening. On tvOS 27.2.1 this leaves the dimmed Siri
//! overlay on the Apple TV, which blocks navigation until Siri and then Back twice are pressed.
//! It is for testing that.

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
const STEPS: [&str; 32] = [
    "metadata",
    "setup_input",
    "setup",
    "entities",
    "states",
    "values",
    "remote_ui",
    "standby",
    "remote_wake",
    "remote_commands",
    "simple_commands",
    "app_select",
    "volume",
    "playback",
    "languages",
    "artwork",
    "wake_command",
    "wake_artwork",
    "speed",
    "home_playing",
    "siri_wake",
    "siri_direct",
    "power",
    "abort_setup",
    "configure",
    "backup_restore",
    "driver_restart",
    "config_file",
    "broken_config",
    "atv_restart",
    "restore_power",
    "log",
];
/// Steps that set up the driver and connect it. The other checks need them.
const BASE_STEPS: [&str; 3] = ["setup", "entities", "states"];
/// Steps at the end that set the Apple TV back and check the driver log.
const END_STEPS: [&str; 2] = ["restore_power", "log"];

/// Steps that run only when `--only` names them.
const OPT_IN_STEPS: [&str; 1] = ["atv_restart"];

/// A step runs if `--only` is empty or names it, or if it is in [`BASE_STEPS`] or
/// [`END_STEPS`]. A step of [`OPT_IN_STEPS`] runs only if `--only` names it. `--skip` wins.
fn selected(only: &[String], skip: &[String], name: &str) -> bool {
    let named = only.iter().any(|n| n == name);
    let runs = if OPT_IN_STEPS.contains(&name) {
        named
    } else {
        only.is_empty() || BASE_STEPS.contains(&name) || END_STEPS.contains(&name) || named
    };
    runs && !skip.iter().any(|n| n == name)
}

const USAGE: &str = "Usage:
  remote-sim pair <address> [--credentials <path>]
  remote-sim pair-driver <address> [--config <path>] [--driver <path>] [--port <port>]
  remote-sim <address> [--config <path>] [--driver <path>] [--credentials <path>]
             [--only a,b] [--skip a,b] [--no-power] [--port <port>]
             [--siri-audio <file>] [--siri-hold]";

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
    /// Recorded Siri question, in a format that `afconvert` reads. Else `say` makes it.
    pub siri_audio: Option<PathBuf>,
    /// 1500 ms of silence after the speech instead of 300 ms. On tvOS 27.2.1 this leaves the
    /// dimmed Siri overlay on the Apple TV, which blocks navigation until Siri and then Back
    /// twice are pressed. It is for testing that.
    pub siri_hold: bool,
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
    /// Questions to the owner in the current step and the answers, for the report.
    pub answers: Vec<String>,
    /// Speech for the Siri checks, 16 kHz mono 16 bit PCM with the silence. Made once.
    pub siri_pcm: Option<Vec<u8>>,
    /// Lines of the terminal, from [`checks::stdin_lines`].
    pub stdin: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    /// The least content that was not started: the owner skipped, or there was no terminal to
    /// ask. With the reason. Checks that need it or more skip without asking.
    pub declined: Option<(oracle::Content, String)>,
    /// The app that last had the state Playing (or a paused movie that a check accepted), seen when
    /// the second connection started or when a check found its content. A check opens it again
    /// when nothing plays or the item has no title.
    pub played_app: Option<String>,
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
        let mut line = if note.is_empty() {
            format!("{tag}  {name} ({ms} ms)")
        } else {
            format!("{tag}  {name} ({ms} ms): {note}")
        };
        if !self.answers.is_empty() {
            line.push_str(" [owner: ");
            line.push_str(&self.answers.join("; "));
            line.push(']');
            self.answers.clear();
        }
        println!("{line}");
        self.report.push(line);
        tag != "FAIL"
    }
}

fn parse() -> Result<Options, String> {
    parse_from(std::env::args().skip(1))
}

/// Parses the arguments without the program name.
fn parse_from(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut args = args.peekable();
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
        siri_audio: None,
        siri_hold: false,
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
            "--siri-hold" => opt.siri_hold = true,
            "--siri-audio" => {
                opt.siri_audio = Some(args.next().ok_or("--siri-audio needs a file")?.into());
            }
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
    if let Some(text) = old_driver(&opt.bin) {
        println!("{text}");
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
        answers: Vec::new(),
        declined: None,
        played_app: None,
        siri_pcm: None,
        // Started at once on a terminal, so lines typed before a prompt are dropped. A run in the
        // background with a terminal stdin stops; use `< /dev/null` there.
        stdin: checks::terminal().then(checks::stdin_lines),
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
    // Exit with 1 if a check failed.
    std::process::exit(i32::from(ctx.failed > 0));
}

/// A warning if the driver binary is older than a file in `src` or `resources`, or than
/// `Cargo.toml`, `Cargo.lock` or `build.rs` of its repo. The repo is the folder above
/// `target/<profile>/` with a `Cargo.toml`. `None` if it is not older or not in a repo.
fn old_driver(bin: &std::path::Path) -> Option<String> {
    let built = std::fs::metadata(bin).ok()?.modified().ok()?;
    let profile = bin.parent()?.file_name()?.to_str()?;
    let root = bin.ancestors().nth(3)?;
    let root = if root.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        root
    };
    if !root.join("Cargo.toml").is_file() {
        return None;
    }
    let build = match profile {
        "debug" => "cargo build".to_owned(),
        "release" => "cargo build --release".to_owned(),
        other => format!("cargo build --profile {other}"),
    };
    let modified = |f: &str| std::fs::metadata(root.join(f)).ok()?.modified().ok();
    let newest = ["src", "resources"]
        .iter()
        .filter_map(|d| newest_file(&root.join(d)))
        .chain(
            ["Cargo.toml", "Cargo.lock", "build.rs"]
                .into_iter()
                .filter_map(modified),
        )
        .max()?;
    (newest > built).then(|| {
        format!(
            "WARNING: the driver binary {} may be older than the source: run {build} (ignore \
             this if it has nothing to build)",
            bin.display()
        )
    })
}

/// Time of the newest file in `dir` and its subfolders.
fn newest_file(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let time = if kind.is_dir() {
            newest_file(&entry.path())
        } else {
            entry.metadata().ok().and_then(|m| m.modified().ok())
        };
        newest = newest.max(time);
    }
    newest
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
    ctx.step("remote_commands", checks::remote_commands).await;
    ctx.step("simple_commands", checks::simple_commands).await;
    ctx.step("app_select", checks::app_select).await;
    ctx.step("volume", checks::volume).await;
    ctx.step("playback", checks::playback).await;
    ctx.step("languages", checks::languages).await;
    ctx.step("artwork", checks::artwork).await;
    ctx.step("wake_command", checks::wake_command).await;
    ctx.step("wake_artwork", checks::wake_artwork).await;
    ctx.step("speed", checks::speed).await;
    ctx.step("home_playing", checks::home_playing).await;
    // Siri can cover the video, so it runs after the other checks that need playback.
    ctx.step("siri_wake", checks::siri_wake).await;
    // The same audio through the second connection, without the driver, to compare.
    ctx.step("siri_direct", checks::siri_direct).await;
    // After the checks that need playback: power stops the video.
    ctx.step("power", checks::power).await;
    ctx.step("abort_setup", checks::abort_setup).await;
    ctx.step("configure", checks::configure).await;
    ctx.step("backup_restore", checks::backup_restore).await;
    ctx.step("driver_restart", checks::driver_restart).await;
    ctx.step("config_file", checks::config_file).await;
    ctx.step("broken_config", checks::broken_config).await;
    // Needs a person, so only with `--only atv_restart`.
    ctx.step("atv_restart", checks::atv_restart).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> impl Iterator<Item = String> {
        line.split_whitespace().map(String::from)
    }

    #[test]
    fn siri_audio_option() {
        let opt = parse_from(args("10.0.0.1 --siri-audio question.m4a")).unwrap();
        assert_eq!(opt.siri_audio, Some(PathBuf::from("question.m4a")));
        assert!(!opt.siri_hold);
        let opt = parse_from(args("10.0.0.1")).unwrap();
        assert_eq!(opt.siri_audio, None);
        assert!(!opt.siri_hold);
        assert!(parse_from(args("10.0.0.1 --siri-hold")).unwrap().siri_hold);
        assert!(parse_from(args("10.0.0.1 --siri-audio")).is_err());
    }

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
        assert!(
            !selected(&[], &[], "atv_restart"),
            "opt-in step without --only"
        );
        assert!(!selected(&only, &[], "atv_restart"));
        let restart = names(&["atv_restart"]);
        assert!(selected(&restart, &[], "atv_restart"));
        assert!(!selected(&restart, &restart, "atv_restart"), "--skip wins");
        for name in OPT_IN_STEPS {
            assert!(STEPS.contains(&name), "{name}");
        }
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
    fn old_driver_binary() {
        use std::time::{Duration, SystemTime};
        let root = std::env::temp_dir().join(format!("remote-sim-old-{}", std::process::id()));
        let bin = root.join("target/release/driver");
        std::fs::create_dir_all(bin.parent().expect("parent")).expect("target");
        std::fs::create_dir_all(root.join("src/sub")).expect("src");
        std::fs::write(root.join("Cargo.toml"), "").expect("Cargo.toml");
        std::fs::write(root.join("src/sub/a.rs"), "").expect("source");
        let set = |time: SystemTime| {
            std::fs::File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&bin)
                .and_then(|f| f.set_modified(time))
                .expect("set time");
        };
        let now = SystemTime::now();
        set(now - Duration::from_secs(100));
        let warning = old_driver(&bin).expect("older than the source");
        assert!(
            warning.contains("run cargo build --release (ignore"),
            "{warning}"
        );
        std::fs::remove_file(root.join("Cargo.toml")).expect("remove Cargo.toml");
        assert!(old_driver(&bin).is_none(), "not in a repo");
        std::fs::write(root.join("Cargo.toml"), "").expect("Cargo.toml");
        set(now + Duration::from_secs(100));
        assert!(old_driver(&bin).is_none(), "newer than the source");
        // Only Cargo.lock is newer, as after a dependency update.
        let age = |path: &std::path::Path, time: SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|f| f.set_modified(time))
                .expect("set time");
        };
        age(&root.join("src/sub/a.rs"), now - Duration::from_secs(300));
        age(&root.join("Cargo.toml"), now - Duration::from_secs(300));
        set(now - Duration::from_secs(100));
        assert!(old_driver(&bin).is_none(), "older source");
        std::fs::write(root.join("Cargo.lock"), "").expect("Cargo.lock");
        assert!(old_driver(&bin).is_some(), "newer Cargo.lock");
        assert!(old_driver(&root.join("missing")).is_none(), "no binary");
        let _ = std::fs::remove_dir_all(&root);
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
