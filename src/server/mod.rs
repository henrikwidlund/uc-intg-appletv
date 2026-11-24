// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Server modules of the integration driver: WebSocket, artwork route and mDNS advertisement.

mod artwork;
#[cfg(feature = "mdns-sd")]
mod mdns;
mod ws;

pub use artwork::{ArtworkStore, artwork_index};
#[cfg(feature = "mdns-sd")]
pub use mdns::publish_service;
pub use ws::{Authentication, save_local_address, ws_index};

/// Fallback if no mDNS library is enabled.
#[cfg(not(feature = "mdns-sd"))]
#[allow(clippy::unnecessary_wraps)]
pub fn publish_service(
    _instance_name: impl Into<String>,
    _service_name: impl AsRef<str>,
    _protocol: impl AsRef<str>,
    _port: u16,
    _txt: Vec<String>,
) -> Result<(), crate::errors::ServiceError> {
    log::warn!("No mDNS library support included: service will not be published!");
    Ok(())
}
