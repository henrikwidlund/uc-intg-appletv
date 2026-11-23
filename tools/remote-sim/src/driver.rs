// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The driver as a child process. mDNS is off, so the driver needs no root rights.

use crate::Fail;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

pub struct Driver {
    bin: PathBuf,
    pub dir: PathBuf,
    pub port: u16,
    pub log: PathBuf,
    child: Option<Child>,
    /// Log size when the driver was last started.
    log_start: usize,
}

impl Driver {
    pub fn new(bin: &Path, dir: &Path, port: u16) -> Self {
        Self {
            bin: bin.to_owned(),
            dir: dir.to_owned(),
            port,
            log: dir.join("driver.log"),
            child: None,
            log_start: 0,
        }
    }

    pub async fn start(&mut self) -> Result<(), Fail> {
        self.stop().await;
        // The child process needs a file of the standard library.
        let log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .await?
            .into_std()
            .await;
        self.log_start = self.log_len();
        let child = Command::new(&self.bin)
            .env("UC_CONFIG_HOME", &self.dir)
            .env("UC_INTEGRATION_INTERFACE", "127.0.0.1")
            .env("UC_INTEGRATION_HTTP_PORT", self.port.to_string())
            .env("UC_DISABLE_MDNS_PUBLISH", "true")
            .env("RUST_LOG", log_filter())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Fail::new(format!("cannot start {}: {e}", self.bin.display())))?;
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", self.port)).await.is_err() {
            if Instant::now() > deadline {
                return Err(Fail::new("the driver does not listen after 10 s"));
            }
            if let Some(status) = self
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten())
            {
                return Err(Fail::new(format!("the driver ended at start: {status}")));
            }
            sleep(Duration::from_millis(50)).await;
        }
        Ok(())
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

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Log size when the driver was last started: a mark for [`Self::log_since`].
    pub const fn log_start(&self) -> usize {
        self.log_start
    }

    /// Current size of the log in bytes: a mark for [`Self::log_since`].
    pub fn log_len(&self) -> usize {
        std::fs::metadata(&self.log).map_or(0, |m| usize::try_from(m.len()).unwrap_or(usize::MAX))
    }

    /// The log text after `mark`.
    pub fn log_since(&self, mark: usize) -> String {
        use std::io::{Read, Seek, SeekFrom};
        let mut text = String::new();
        if let Ok(mut file) = std::fs::File::open(&self.log)
            && file.seek(SeekFrom::Start(mark as u64)).is_ok()
        {
            // A cut UTF-8 character at the mark is replaced, not an error.
            let mut bytes = Vec::new();
            if file.read_to_end(&mut bytes).is_ok() {
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        text
    }
}

/// Log filter of the driver. Checks read its info and debug lines. A `RUST_LOG` of the caller
/// is added after the default, for example `ratv::mrp=trace`. A target in it replaces the same
/// target of the default.
fn log_filter() -> String {
    const DEFAULT: &str = "info,uc_intg_appletv=debug";
    match std::env::var("RUST_LOG") {
        Ok(caller) if !caller.is_empty() => format!("{DEFAULT},{caller}"),
        _ => DEFAULT.to_owned(),
    }
}
