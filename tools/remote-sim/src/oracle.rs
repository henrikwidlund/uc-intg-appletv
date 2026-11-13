// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! A second connection to the Apple TV. It gives the values that the Apple TV has, to compare
//! them with the values that the driver shows.
//!
//! It uses its own pairing. With the credentials of the driver, the Apple TV closes the Companion
//! connection of the driver.

use crate::Fail;
use ratv::companion::{App, AttentionState};
use ratv::discovery::{MDNS_PORT, ScanOptions, scan};
use ratv::mrp::{NowPlaying, Volume};
use ratv::{AppleTv, ClientOptions, Credentials, Endpoints, ExposeSecret, PairOptions};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Client name shown on the Apple TV.
const NAME: &str = "remote-sim";

pub struct Oracle {
    tv: AppleTv,
    pub apps: Vec<App>,
}

async fn endpoints(address: IpAddr) -> Result<Endpoints, Fail> {
    let mut options = ScanOptions::default();
    options.timeout = Duration::from_secs(3);
    options.targets = vec![SocketAddr::new(address, MDNS_PORT)];
    let devices = scan(&options).await?;
    let device = devices
        .iter()
        .find(|d| d.addresses.contains(&address))
        .ok_or_else(|| Fail::new(format!("no Apple TV answers at {address}")))?;
    Ok(Endpoints::from(device))
}

/// Pair the test program with the Apple TV. The PIN is read from the terminal.
pub async fn pair(address: IpAddr, file: &Path) -> Result<(), Fail> {
    let ep = endpoints(address).await?;
    let mut options = PairOptions::default();
    options.name = NAME.into();
    let prompt = ratv::pair(&ep, &options).await?;
    println!("Type the PIN shown on the Apple TV and press Enter:");
    let mut pin = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut pin)
        .await?;
    let credentials = prompt.finish(pin.trim()).await?;
    write_private(file, credentials.to_secret_string().expose_secret())?;
    println!("Saved the pairing to {}", file.display());
    Ok(())
}

/// Write a file that only the owner can read.
fn write_private(file: &Path, text: &str) -> Result<(), Fail> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(file)?.write_all(text.as_bytes())?;
    Ok(())
}

impl Oracle {
    pub async fn connect(address: IpAddr, file: &Path) -> Result<Self, Fail> {
        let text = std::fs::read_to_string(file).map_err(|e| {
            Fail::new(format!(
                "no pairing in {} ({e}), run `remote-sim pair <address>` first",
                file.display()
            ))
        })?;
        let credentials: Credentials = text
            .trim()
            .parse()
            .map_err(|_| Fail::new(format!("invalid pairing in {}", file.display())))?;
        let ep = endpoints(address).await?;
        let mut options = ClientOptions::default();
        options.name = NAME.into();
        let tv = AppleTv::connect(&ep, &credentials, &options).await?;
        ensure_protocols(&tv)?;
        let apps = tv.apps().await?;
        Ok(Self { tv, apps })
    }

    pub async fn close(&self) {
        let _ = self.tv.close().await;
    }

    pub async fn power(&self) -> Result<AttentionState, Fail> {
        let companion = self
            .tv
            .companion()
            .ok_or_else(|| Fail::new("no Companion connection"))?;
        Ok(companion.power_state().await?)
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        self.tv.now_playing().map(|rx| rx.borrow().clone())
    }

    pub fn volume(&self) -> Option<Volume> {
        self.tv.volume().map(|rx| rx.borrow().clone())
    }

    /// Name of the app with this bundle id.
    pub fn app_name(&self, bundle_id: &str) -> Option<&str> {
        self.apps
            .iter()
            .find(|a| a.bundle_id == bundle_id)
            .map(|a| a.name.as_str())
    }
}

fn ensure_protocols(tv: &AppleTv) -> Result<(), Fail> {
    match (tv.companion().is_some(), tv.mrp().is_some()) {
        (true, true) => Ok(()),
        (false, _) => Err(Fail::new("the second connection has no Companion")),
        (_, false) => Err(Fail::new("the second connection has no MRP")),
    }
}

/// The Apple TV is on.
pub const fn is_awake(state: AttentionState) -> bool {
    !matches!(state, AttentionState::Asleep | AttentionState::Unknown)
}
