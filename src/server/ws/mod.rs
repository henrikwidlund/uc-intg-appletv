// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket server for the Remote Two integration API.

use crate::configuration::{ENV_API_MSG_TRACING, HeartbeatSettings};
use crate::controller::{Controller, R2AudioChunkMsg, driver_version};
use actix::{Addr, Recipient};
use actix_web::{Error, HttpRequest, HttpResponse, get, web};
use bytestring::ByteString;
use log::debug;
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;
use uc_api::intg::IntegrationDriverUpdate;
use uc_api::ws::WsMessage;

mod connection;
mod events;
mod requests;
mod responses;

/// WebSocket connection of one Remote.
struct WsConn {
    /// Unique connection identifier, used for logging and to route messages in the [`Controller`].
    id: Arc<str>,
    /// Local address that the connection arrived on.
    local_ip: Option<IpAddr>,
    /// Time of the last client activity.
    hb: Instant,
    /// [`Controller`] address for sending events and requests.
    controller_addr: Addr<Controller>,
    heartbeat: HeartbeatSettings,
    /// Log every incoming message. PINs and backup data are hidden.
    msg_tracing_in: bool,
    /// Log every outgoing message. PINs and backup data are hidden.
    msg_tracing_out: bool,
    /// Audio received in the last voice session.
    voice: VoiceCount,
    /// Receives the voice audio chunks.
    audio: Recipient<R2AudioChunkMsg>,
    /// The `authentication` message, sent first.
    authentication: ByteString,
}

/// The `authentication` message with the driver version. It is the same for each connection.
#[derive(Clone)]
pub struct Authentication(pub ByteString);

impl Authentication {
    pub fn new(metadata: &IntegrationDriverUpdate) -> Self {
        let msg = WsMessage::response(0, "authentication", driver_version(metadata));
        let text = serde_json::to_string(&msg).unwrap_or_else(|_| {
            r#"{"kind":"resp","req_id":0,"code":200,"msg":"authentication"}"#.into()
        });
        Self(text.into())
    }
}

/// Audio received from the Remote in one voice session.
#[derive(Debug, Default, Clone, Copy)]
struct VoiceCount {
    session_id: u32,
    chunks: u32,
    bytes: u64,
    /// The audio format is not supported. The audio of this session is dropped.
    rejected: bool,
}

impl WsConn {
    fn new(
        client_id: String,
        local_ip: Option<IpAddr>,
        controller_addr: Addr<Controller>,
        heartbeat: HeartbeatSettings,
        authentication: ByteString,
    ) -> Self {
        let msg_tracing = env::var(ENV_API_MSG_TRACING).unwrap_or_default();
        Self {
            authentication,
            id: client_id.into(),
            local_ip,
            hb: Instant::now(),
            heartbeat,
            msg_tracing_in: msg_tracing == "all" || msg_tracing == "in",
            msg_tracing_out: msg_tracing == "all" || msg_tracing == "out",
            voice: VoiceCount::default(),
            audio: controller_addr.clone().recipient(),
            controller_addr,
        }
    }
}

/// Local address that a connection arrived on.
#[derive(Clone, Copy)]
struct LocalAddress(IpAddr);

/// Save the local address of a new connection for [`ws_index`]. The OS sets it, so a client
/// cannot change it.
pub fn save_local_address(conn: &dyn std::any::Any, data: &mut actix_web::dev::Extensions) {
    if let Some(tcp) = conn.downcast_ref::<actix_web::rt::net::TcpStream>()
        && let Ok(addr) = tcp.local_addr()
    {
        data.insert(LocalAddress(addr.ip()));
    }
}

/// HTTP endpoint for the WebSocket upgrade.
#[get("/")]
pub async fn ws_index(
    request: HttpRequest,
    stream: web::Payload,
    heartbeat: web::Data<HeartbeatSettings>,
    controller: web::Data<Addr<Controller>>,
    authentication: web::Data<Authentication>,
) -> Result<HttpResponse, Error> {
    // Use peer IP:port as unique client identifier.
    let client_id = request
        .peer_addr()
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), |addr| addr.to_string());
    debug!("New WebSocket connection from: {client_id}");

    let (resp, session, msg_stream) = actix_ws::handle(&request, stream)?;

    // Remote messages are small. The default frame size of 64 KiB is enough, but voice data can
    // come in bigger chunks.
    let stream = msg_stream
        .max_frame_size(128 * 1024)
        .aggregate_continuations();

    let local_ip = request.conn_data::<LocalAddress>().map(|a| a.0);
    let conn = WsConn::new(
        client_id,
        local_ip,
        controller.get_ref().clone(),
        *heartbeat.get_ref(),
        authentication.0.clone(),
    );

    actix_web::rt::spawn(async move {
        conn.run(session, stream).await;
    });

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::Authentication;
    use serde_json::{Value, json};

    #[test]
    fn authentication_has_driver_version() {
        let metadata =
            serde_json::from_value(json!({ "name": { "en": "Apple TV" } })).expect("metadata");
        let text = Authentication::new(&metadata).0;
        let msg: Value = serde_json::from_str(&text).expect("json");
        assert_eq!("resp", msg["kind"]);
        assert_eq!(0, msg["req_id"]);
        assert_eq!(200, msg["code"]);
        assert_eq!("authentication", msg["msg"]);
        assert_eq!("Apple TV", msg["msg_data"]["name"]);
        assert_eq!(crate::APP_VERSION, msg["msg_data"]["version"]["driver"]);
    }
}
