// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket server for the Remote Two integration API.

use crate::configuration::{ENV_API_MSG_TRACING, HeartbeatSettings};
use crate::controller::Controller;
use actix::Addr;
use actix_web::{Error, HttpRequest, HttpResponse, get, web};
use log::debug;
use std::env;
use std::sync::Arc;
use std::time::Instant;

mod connection;
mod events;
mod requests;
mod responses;

/// WebSocket connection of one Remote.
struct WsConn {
    /// Unique connection identifier, used for logging and to route messages in the [`Controller`].
    id: Arc<str>,
    /// Time of the last client activity.
    hb: Instant,
    /// [`Controller`] address for sending events and requests.
    controller_addr: Addr<Controller>,
    heartbeat: HeartbeatSettings,
    /// Log every incoming message. Messages can contain PINs!
    msg_tracing_in: bool,
    /// Log every outgoing message.
    msg_tracing_out: bool,
    /// Audio received in the last voice session.
    voice: VoiceCount,
}

/// Audio received from the Remote in one voice session.
#[derive(Debug, Default, Clone, Copy)]
struct VoiceCount {
    session_id: u32,
    chunks: u32,
    bytes: u64,
}

impl WsConn {
    fn new(
        client_id: String,
        controller_addr: Addr<Controller>,
        heartbeat: HeartbeatSettings,
    ) -> Self {
        let msg_tracing = env::var(ENV_API_MSG_TRACING).unwrap_or_default();
        Self {
            id: client_id.into(),
            hb: Instant::now(),
            controller_addr,
            heartbeat,
            msg_tracing_in: msg_tracing == "all" || msg_tracing == "in",
            msg_tracing_out: msg_tracing == "all" || msg_tracing == "out",
            voice: VoiceCount::default(),
        }
    }
}

/// HTTP endpoint for the WebSocket upgrade.
#[get("/")]
pub async fn ws_index(
    request: HttpRequest,
    stream: web::Payload,
    heartbeat: web::Data<HeartbeatSettings>,
    controller: web::Data<Addr<Controller>>,
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

    let conn = WsConn::new(
        client_id,
        controller.get_ref().clone(),
        *heartbeat.get_ref(),
    );

    actix_web::rt::spawn(async move {
        conn.run(session, stream).await;
    });

    Ok(resp)
}
