// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Actix messages between the WebSocket connections and the [`Controller`](super::Controller).

use crate::errors::ServiceError;
use crate::util::DeserializeMsgData;
use actix::prelude::{Message, Recipient};
use bytes::Bytes;
use bytestring::ByteString;
use std::sync::Arc;
use uc_api::intg::ws::{R2Event, R2Request, R2Response};
use uc_api::ws::WsMessage;

/// Sample rate of the voice audio stream. Siri needs 16 kHz.
pub const VOICE_SAMPLE_RATE: u32 = 16_000;

/// Send a WebSocket message to a Remote. Best effort only.
#[derive(Message)]
#[rtype(result = "()")]
pub enum SendWsMessage {
    Msg(WsMessage),
    /// Serialized already. Cheap to clone for more than one Remote.
    Text(ByteString),
}

impl From<WsMessage> for SendWsMessage {
    fn from(msg: WsMessage) -> Self {
        Self::Msg(msg)
    }
}

/// A Remote connected.
#[derive(Message)]
#[rtype(result = "()")]
pub struct NewR2Session {
    /// Address of the WebSocket session to send messages to.
    pub addr: Recipient<SendWsMessage>,
    /// Unique identifier of the WebSocket connection.
    pub id: Arc<str>,
}

/// A Remote disconnected.
#[derive(Message)]
#[rtype(result = "()")]
pub struct R2SessionDisconnect {
    /// Unique identifier of the WebSocket connection.
    pub id: Arc<str>,
}

/// Request message from a Remote.
///
/// The controller returns the response, or `None` if it sends the response later.
#[derive(Debug, Message)]
#[rtype(result = "Result<Option<WsMessage>, ServiceError>")]
pub struct R2RequestMsg {
    pub ws_id: Arc<str>,
    pub req_id: u32,
    pub request: R2Request,
    pub msg_data: Option<serde_json::Value>,
}

impl From<R2RequestMsg> for Option<serde_json::Value> {
    fn from(value: R2RequestMsg) -> Self {
        value.msg_data
    }
}

impl DeserializeMsgData for R2RequestMsg {}

/// Response message from a Remote.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct R2ResponseMsg {
    pub ws_id: Arc<str>,
    pub msg: R2Response,
}

/// Event message from a Remote.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct R2EventMsg {
    pub ws_id: Arc<str>,
    pub event: R2Event,
}

/// Voice audio chunk from a Remote. Empty data ends the audio stream.
#[derive(Message)]
#[rtype(result = "()")]
pub struct R2AudioChunkMsg {
    /// Remote audio session id.
    pub session_id: u32,
    /// PCM samples: 16 bit signed, little endian, mono.
    pub data: Bytes,
}

impl R2AudioChunkMsg {
    /// End of the audio stream.
    pub const fn end(session_id: u32) -> Self {
        Self {
            session_id,
            data: Bytes::new(),
        }
    }
}
