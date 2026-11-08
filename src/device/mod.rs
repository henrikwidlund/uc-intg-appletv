// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Apple TV devices: one actor per device, discovery and state.

mod actor;
mod browse;
mod commands;
pub mod discovery;
pub mod state;

pub use actor::AppleTvActor;

use crate::config_store::{ConfigStore, DeviceConfig, SiriTiming};
use crate::entities::EntityKind;
use crate::errors::ServiceError;
use crate::server::ArtworkStore;
use actix::Message;
use bytes::Bytes;
use serde_json::{Map, Value};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
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
    pub http_port: u16,
    pub store: ConfigStore,
    pub artwork: ArtworkStore,
    /// [`SiriTiming`] as `u8`. The setup can change it.
    pub siri_timing: AtomicU8,
}

impl DeviceContext {
    pub fn siri_timing(&self) -> SiriTiming {
        SiriTiming::from_u8(self.siri_timing.load(Ordering::Relaxed))
    }

    pub fn set_siri_timing(&self, timing: SiriTiming) {
        self.siri_timing.store(timing as u8, Ordering::Relaxed);
    }
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
    },
    Status {
        key: Arc<str>,
        status: DeviceStatus,
    },
    Assistant(AssistantEvent),
}

/// Connect, and stay connected.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Connect;

/// Disconnect, and stay disconnected.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Disconnect;

/// Disconnect and stop the actor.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Stop;

/// New configuration of the device.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct UpdateConfig(pub DeviceConfig);

/// Entity command.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct Command {
    pub kind: EntityKind,
    pub cmd_id: String,
    pub params: Option<Map<String, Value>>,
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
