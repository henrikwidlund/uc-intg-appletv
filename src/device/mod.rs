// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Apple TV devices: one actor per device, discovery and state.

mod actor;
mod browse;
mod commands;
pub mod discovery;
pub mod state;

pub use actor::{AppleTvActor, DeviceCache};

use crate::config_store::{ConfigStore, DeviceConfig, SiriMode};
use crate::entities::EntityKind;
use crate::errors::ServiceError;
use crate::server::ArtworkStore;
use actix::Message;
use bytes::Bytes;
use serde_json::{Map, Value};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use uc_api::intg::AssistantEvent;
use uc_api::intg::ws::BrowseMediaResponseMsgData;
use uc_api::model::Paging;

/// Shared settings of all device actors.
#[derive(Debug)]
pub struct DeviceContext {
    /// Name shown on the Apple TV.
    pub client_name: String,
    /// Address of the HTTP server for the artwork route. `None` if it listens on all interfaces.
    pub http_address: Option<IpAddr>,
    /// The HTTP server listens on `::`, so it can also be reached over IPv6.
    pub http_ipv6: bool,
    pub http_port: u16,
    pub store: ConfigStore,
    pub artwork: ArtworkStore,
    /// [`SiriMode`] as `u8`. The setup can change it.
    pub siri_mode: AtomicU8,
    /// Local address of the last Remote connection, for artwork URLs. The Remote can reach the
    /// driver there.
    pub(crate) remote_ip: Mutex<Option<IpAddr>>,
}

impl DeviceContext {
    pub fn siri_mode(&self) -> SiriMode {
        SiriMode::from_u8(self.siri_mode.load(Ordering::Relaxed))
    }

    pub fn set_siri_mode(&self, mode: SiriMode) {
        self.siri_mode.store(mode as u8, Ordering::Relaxed);
    }

    pub fn remote_ip(&self) -> Option<IpAddr> {
        *self
            .remote_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Store the address of a new Remote connection. A loopback address does not replace an
    /// address that is not loopback: otherwise a program on the same host would break the
    /// artwork of a Remote on the network. Returns `true` if the stored address changed.
    pub fn set_remote_ip(&self, ip: IpAddr) -> bool {
        // An IPv4 address can come as an IPv6 address on a `::` listener.
        let ip = ip.to_canonical();
        let mut stored = self
            .remote_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let replace = replaces_remote_ip(*stored, ip);
        if replace {
            *stored = Some(ip);
        }
        replace
    }
}

/// A new Remote address `new` replaces `old`, unless it is the same, or it is a loopback address
/// and `old` is an address that is not loopback. Both are canonical (see
/// [`IpAddr::to_canonical`]).
fn replaces_remote_ip(old: Option<IpAddr>, new: IpAddr) -> bool {
    old != Some(new) && (!new.is_loopback() || old.is_none_or(|o| o.is_loopback()))
}

/// The pairing does not work with the device at this address: the device rejected the
/// credentials, or another device has the address. Pair again or find the device again.
pub const fn pairing_not_valid(e: &ratv::Error) -> bool {
    e.is_auth() || e.is_other_device()
}

/// Current time in MRP time, for playback positions.
pub fn now() -> f64 {
    ratv::mrp::cocoa_time(std::time::SystemTime::now())
}

/// Connection status of an Apple TV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    Disconnected,
    Connecting,
    Connected,
    /// Pairing is not valid anymore. No reconnect until the next connect request.
    AuthError,
}

/// Event from a device actor to the controller.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub enum DeviceEvent {
    /// Attributes that might have changed, per entity.
    Changes {
        key: Arc<str>,
        changes: Vec<(EntityKind, Map<String, Value>)>,
    },
    /// The configuration changed and must be saved.
    Config {
        key: Arc<str>,
        cfg: DeviceConfig,
        /// Number of [`UpdateConfig`] messages the actor has handled.
        version: u64,
    },
    Status {
        key: Arc<str>,
        status: DeviceStatus,
    },
    Assistant(AssistantEvent),
    /// A command with a [`Reply`] started. Sent before the events of the command.
    Accepted(Reply),
}

/// Connect, and stay connected.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Connect;

/// Disconnect, and stay disconnected until the next `Connect` or command. When connected, waits
/// up to 2 s for a command that is running.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Disconnect;

/// The Siri mode in [`DeviceContext`] changed.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct SiriModeChanged;

/// The Remote address in [`DeviceContext`] changed: build the artwork URL again.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct RemoteAddressChanged;

/// Disconnect and stop the actor.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Stop {
    /// Delete the cache file when the actor stops, after its last save.
    pub remove_cache: bool,
}

/// Save the cache file now.
#[cfg(test)]
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct SaveCacheNow;

/// New configuration of the device.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct UpdateConfig(pub DeviceConfig);

/// Entity command. Answered after the parameters are checked; the command runs later.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct Command {
    pub kind: EntityKind,
    pub cmd_id: String,
    pub params: Option<Map<String, Value>>,
    /// The request to answer with [`DeviceEvent::Accepted`] when the command started. Set for
    /// voice assistant commands: the answer must come before their assistant events.
    pub reply: Option<Reply>,
}

/// Remote request of a command.
#[derive(Debug)]
pub struct Reply {
    pub ws_id: Arc<str>,
    pub req_id: u32,
}

/// Browse the apps and the Up Next queue.
#[derive(Debug, Message)]
#[rtype(result = "Result<BrowseMediaResponseMsgData, ServiceError>")]
pub struct Browse {
    pub media_id: Option<String>,
    pub paging: Paging,
}

/// PCM audio for a Siri session. Empty data ends the session.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct AudioChunk {
    pub session_id: u32,
    pub data: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_does_not_replace_a_network_address() {
        let lan = IpAddr::from([192, 0, 2, 20]);
        let other = IpAddr::from([192, 0, 2, 21]);
        let lo = IpAddr::from([127, 0, 0, 1]);
        let lo6 = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);
        assert!(replaces_remote_ip(None, lo), "first address");
        assert!(replaces_remote_ip(Some(lo), lan));
        assert!(replaces_remote_ip(Some(lan), other));
        assert!(replaces_remote_ip(Some(lo), lo6));
        assert!(!replaces_remote_ip(Some(lan), lan), "same address");
        assert!(!replaces_remote_ip(Some(lan), lo));
        assert!(!replaces_remote_ip(Some(lan), lo6));
    }

    #[test]
    fn mapped_loopback_does_not_replace_a_network_address() {
        let mapped = |ip: std::net::Ipv4Addr| IpAddr::V6(ip.to_ipv6_mapped());
        let lan = std::net::Ipv4Addr::new(192, 0, 2, 20);
        let canonical = |ip: IpAddr| ip.to_canonical();
        assert_eq!(IpAddr::V4(lan), canonical(mapped(lan)));
        let lo = canonical(mapped(std::net::Ipv4Addr::LOCALHOST));
        assert!(
            !replaces_remote_ip(Some(IpAddr::V4(lan)), lo),
            "mapped loopback"
        );
        assert!(!replaces_remote_ip(
            Some(IpAddr::V4(lan)),
            canonical(mapped(lan))
        ));
    }
}
