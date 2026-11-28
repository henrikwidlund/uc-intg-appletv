// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The driver as a child process. mDNS is off, so the driver needs no root rights.

pub mod log;

use crate::Fail;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

/// A byte position in the driver log. Outside the driver module, only [`Driver::log_len`] and
/// [`Driver::log_start`] make one, so a line index cannot be used as a mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogMark(u64);

impl LogMark {
    /// The start of the log.
    const START: Self = Self(0);

    /// A mark at byte `at`.
    #[cfg(test)]
    pub const fn at(at: u64) -> Self {
        Self(at)
    }
}

/// Whether the driver gets [`log_filter`] as `RUST_LOG`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RustLog {
    Set,
    /// The debug log option of the setup sets the log filter.
    Unset,
}

pub struct Driver {
    bin: PathBuf,
    pub dir: PathBuf,
    pub port: u16,
    /// Only the [`log`] module reads this file.
    log: PathBuf,
    child: Option<Child>,
    /// Log size when the driver was last started.
    log_start: LogMark,
}

impl Driver {
    pub fn new(bin: &Path, dir: &Path, port: u16) -> Self {
        Self {
            bin: bin.to_owned(),
            dir: dir.to_owned(),
            port,
            log: dir.join("driver.log"),
            child: None,
            log_start: LogMark::START,
        }
    }

    /// Start the driver with [`log_filter`] as `RUST_LOG`.
    pub async fn start(&mut self) -> Result<(), Fail> {
        self.launch(RustLog::Set).await
    }

    /// Start the driver, with or without [`log_filter`] as `RUST_LOG`.
    pub async fn launch(&mut self, rust_log: RustLog) -> Result<(), Fail> {
        self.stop().await;
        // Another process on the port would look like a started driver.
        check_port_free(self.port).await?;
        // The child process needs a file of the standard library.
        let log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .await?
            .into_std()
            .await;
        self.log_start = self.log_len();
        let mut command = Command::new(&self.bin);
        command
            .env("UC_CONFIG_HOME", &self.dir)
            .env("UC_INTEGRATION_INTERFACE", "127.0.0.1")
            .env("UC_INTEGRATION_HTTP_PORT", self.port.to_string())
            .env("UC_DISABLE_MDNS_PUBLISH", "true");
        match rust_log {
            RustLog::Set => command.env("RUST_LOG", log_filter()),
            RustLog::Unset => command.env_remove("RUST_LOG"),
        };
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Fail::new(format!("cannot start {}: {e}", self.bin.display())))?;
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // The process check comes first: a port that answers counts only while the driver runs.
            if let Some(status) = self
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten())
            {
                return Err(Fail::new(format!("the driver ended at start: {status}")));
            }
            if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Fail::new("the driver does not listen after 10 s"));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }

    pub fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| c.try_wait().ok().flatten().is_none())
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// Log size when the driver was last started: a mark for [`log::driver_log`].
    pub const fn log_start(&self) -> LogMark {
        self.log_start
    }

    /// Current size of the log: a mark for [`log::driver_log`].
    pub fn log_len(&self) -> LogMark {
        LogMark(std::fs::metadata(&self.log).map_or(0, |m| m.len()))
    }

    /// The path of the log file, to show to the user.
    pub fn log_display(&self) -> std::path::Display<'_> {
        self.log.display()
    }

    /// Add `text` to the end of the log.
    #[cfg(test)]
    pub fn append_test_log(&self, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .expect("write the test log");
    }
}

/// Log filter of the driver. Checks read its info and debug lines. A `RUST_LOG` of the caller
/// is added after the default, for example `ratv::mrp=trace`. A level in it for the same target,
/// or a global level, replaces the one in the default.
fn log_filter() -> String {
    const DEFAULT: &str = "info,uc_intg_appletv=debug";
    match std::env::var("RUST_LOG") {
        Ok(caller) if !caller.is_empty() => format!("{DEFAULT},{caller}"),
        _ => DEFAULT.to_owned(),
    }
}

/// Fails if a process listens on `port` on 127.0.0.1.
pub async fn check_port_free(port: u16) -> Result<(), Fail> {
    if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
        return Err(Fail::new(format!(
            "port {port} is used by another process. Stop it or use --port"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_used_port_is_not_taken_for_a_started_driver() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let dir = std::env::temp_dir().join(format!("remote-sim-port-test-{}", std::process::id()));
        let mut driver = Driver::new(Path::new("none"), &dir, port);
        let error = driver.start().await.expect_err("port used");
        assert_eq!(
            format!("port {port} is used by another process. Stop it or use --port"),
            error.msg
        );
        assert!(driver.child.is_none(), "no driver started");
    }

    /// A port that was free a moment ago.
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        listener.local_addr().expect("address").port()
    }

    #[tokio::test]
    async fn a_free_port_passes_and_a_missing_binary_fails() {
        let port = free_port().await;
        assert!(check_port_free(port).await.is_ok());
        let dir = std::env::temp_dir().join(format!("remote-sim-bin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test folder");
        let bin = dir.join("missing-driver");
        let mut driver = Driver::new(&bin, &dir, port);
        let error = driver.launch(RustLog::Unset).await.expect_err("no binary");
        assert!(
            error
                .msg
                .starts_with(&format!("cannot start {}: ", bin.display())),
            "{}",
            error.msg
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_driver_that_ends_at_start_fails() {
        let port = free_port().await;
        let dir = std::env::temp_dir().join(format!("remote-sim-end-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test folder");
        let mut driver = Driver::new(Path::new("/usr/bin/true"), &dir, port);
        let error = driver.start().await.expect_err("ended");
        assert!(
            error.msg.starts_with("the driver ended at start: "),
            "{}",
            error.msg
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
