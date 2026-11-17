// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Driver settings from environment variables and the compiled-in driver metadata.

use crate::{APP_VERSION, built_info};
use serde::de::value::{self, StrDeserializer};
use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use uc_api::intg::IntegrationDriverUpdate;

/// Environment variable for the driver setup flow timeout in seconds. `0` disables the timeout.
pub const ENV_SETUP_TIMEOUT: &str = "UC_SETUP_TIMEOUT";
/// Default driver setup flow timeout in seconds.
pub const DEF_SETUP_TIMEOUT_SEC: u64 = 300;
/// Environment variable for the configuration directory. The Remote sets it.
const ENV_CONFIG_HOME: &str = "UC_CONFIG_HOME";
/// Environment variable for the network interface to listen on.
const ENV_INTERFACE: &str = "UC_INTEGRATION_INTERFACE";
/// Environment variable for the HTTP port to listen on.
const ENV_HTTP_PORT: &str = "UC_INTEGRATION_HTTP_PORT";
/// Environment variable to disable mDNS service publishing. Not needed on the Remote.
pub const ENV_DISABLE_MDNS_PUBLISH: &str = "UC_DISABLE_MDNS_PUBLISH";
/// Environment variable for Integration-API message tracing: `all`, `in` or `out`.
///
/// **Attention:** only for debugging. Messages can contain PINs and personal data.
pub const ENV_API_MSG_TRACING: &str = "UC_API_MSG_TRACING";
/// Environment variable for the client name shown on the Apple TV.
const ENV_CLIENT_NAME: &str = "UC_CLIENT_NAME";

const DEF_INTERFACE: &str = "0.0.0.0";
const DEF_HTTP_PORT: u16 = 9090;
const DEF_DRIVER_ID: &str = "appletv_rs";
const DEF_DRIVER_NAME: &str = "Apple TV (Rust)";

/// Compiled-in driver metadata in JSON format.
const DRIVER_METADATA: &str = include_str!("../resources/driver.json");

/// Driver settings.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Interface address to listen on.
    pub interface: String,
    /// HTTP port for the WebSocket server and the artwork route.
    pub http_port: u16,
    /// Directory for the configuration files.
    pub config_home: PathBuf,
    /// Name shown on the Apple TV for pairing and connections.
    pub client_name: String,
    /// WebSocket heartbeat settings.
    pub heartbeat: HeartbeatSettings,
}

/// WebSocket heartbeat settings for sending ping frames.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatSettings {
    /// How often ping frames are sent.
    pub interval: Duration,
    /// Time without client activity before the connection is closed.
    pub timeout: Duration,
}

impl Default for HeartbeatSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(20),
            timeout: Duration::from_secs(40),
        }
    }
}

impl Settings {
    /// Read the settings from the environment.
    pub fn from_env() -> Self {
        let http_port = env::var(ENV_HTTP_PORT)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEF_HTTP_PORT);
        let client_name = env::var(ENV_CLIENT_NAME)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                hostname::get()
                    .ok()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Remote".into());

        Self {
            interface: env::var(ENV_INTERFACE).unwrap_or_else(|_| DEF_INTERFACE.into()),
            http_port,
            config_home: env::var_os(ENV_CONFIG_HOME)
                .map_or_else(|| PathBuf::from("."), PathBuf::from),
            client_name,
            heartbeat: HeartbeatSettings::default(),
        }
    }
}

/// Get the driver metadata from the compiled-in `driver.json`.
pub fn get_driver_metadata() -> Result<IntegrationDriverUpdate, io::Error> {
    let mut driver: IntegrationDriverUpdate =
        serde_json::from_str(DRIVER_METADATA).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid driver.json: {e}"),
            )
        })?;

    if driver.driver_id.is_none() {
        driver.driver_id = Some(DEF_DRIVER_ID.into());
    }
    if driver.name.as_ref().is_none_or(HashMap::is_empty) {
        driver.name = Some(HashMap::from([("en".into(), DEF_DRIVER_NAME.into())]));
    }
    driver.token = None;
    driver.version = Some(APP_VERSION.to_string());
    // Set by the release build.
    if let Some(date) = built_info::RELEASE_DATE {
        // Parsed by the date type of `uc_api`, so the driver does not name it.
        let date = serde::Deserialize::deserialize(StrDeserializer::<value::Error>::new(date))
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid UC_RELEASE_DATE {date}: {e}"),
                )
            })?;
        driver.release_date = Some(date);
    }

    Ok(driver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_metadata_is_valid() {
        let driver = get_driver_metadata().expect("valid driver.json");
        assert_eq!(Some(DEF_DRIVER_ID), driver.driver_id.as_deref());
        assert!(driver.setup_data_schema.is_some());
        if let Some(date) = built_info::RELEASE_DATE {
            let set = driver.release_date.map(|d| d.to_string());
            assert_eq!(Some(date), set.as_deref());
        }
    }
}
