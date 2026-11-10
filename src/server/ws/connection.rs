// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket connection handling for one Remote.

use crate::controller::{
    NewR2Session, R2AudioChunkMsg, R2SessionDisconnect, SendWsMessage, VOICE_SAMPLE_RATE,
};
use crate::errors::ServiceError;
use crate::server::ws::{VoiceCount, WsConn};
use actix::{Actor, ActorFutureExt, Addr, AsyncContext, Handler, WrapFuture};
use actix_ws::{AggregatedMessage, CloseCode, CloseReason, Session};
use bytes::Bytes;
use log::{debug, error, info, warn};
use prost::Message;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use uc_api::intg::proto::{
    AudioFormat, IntegrationMessage, SampleFormat, integration_message::Message as ProtoMsg,
};
use uc_api::ws::{WsMessage, WsResultMsgData};

/// Longest wait for a free place in the session queue. The queue is full when the Remote does not
/// read.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages that can wait to be sent to one Remote.
const MAILBOX_CAPACITY: usize = 1024;

/// Sends messages from the controller to the WebSocket connection.
///
/// Messages are sent one after another. Events have no id, so the Remote must get them in the
/// order they were created. A send only puts the frame in the session queue, so this is fast.
/// Responses to requests are sent here too.
struct WsSender {
    id: Arc<str>,
    session: Session,
    msg_tracing_out: bool,
}

impl Actor for WsSender {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // A subscribe sends one message per entity: 26 per Apple TV. Changes that do not fit
        // are lost, because the controller sends only what changed.
        ctx.set_mailbox_capacity(MAILBOX_CAPACITY);
    }
}

impl Handler<SendWsMessage> for WsSender {
    type Result = ();

    fn handle(&mut self, msg: SendWsMessage, ctx: &mut Self::Context) {
        let id = Arc::clone(&self.id);
        let msg_tracing_out = self.msg_tracing_out;
        let mut session = self.session.clone();

        // `wait` blocks the mailbox until the message is sent: keeps the order.
        ctx.wait(
            async move {
                let send = async {
                    match msg {
                        SendWsMessage::Msg(m) => {
                            WsConn::send(&id, msg_tracing_out, &m, &mut session).await
                        }
                        SendWsMessage::Text(text) => {
                            if msg_tracing_out {
                                debug!("[{id}] <- {text}");
                            }
                            session.text(text).await.map_err(|e| {
                                error!("[{id}] WebSocket send error: {e}");
                                None
                            })
                        }
                    }
                };
                if timeout(SEND_TIMEOUT, send).await.is_err() {
                    // A lost message leaves an old state on the Remote. After a new connection,
                    // the Remote gets the full state again.
                    warn!("[{id}] Remote does not read, closing the connection");
                    close(session, None).await;
                }
            }
            .into_actor(self)
            .map(|(), _, _| ()),
        );
    }
}

impl WsConn {
    /// Runs the message processing of the WebSocket connection until it is closed.
    ///
    /// - Registers the session with the controller.
    /// - Sends ping frames and closes the connection if the client does not answer.
    /// - Forwards requests, responses and events to the controller.
    pub(crate) async fn run(
        mut self,
        mut session: Session,
        mut stream: actix_ws::AggregatedMessageStream,
    ) {
        use actix_web::rt::time;
        use futures::StreamExt;

        // Only header-based authentication is used, so the authentication message is sent right away.
        let _ = session
            .text(r#"{"kind":"resp","req_id":0,"code":200,"msg":"authentication"}"#)
            .await;

        let sender = WsSender {
            id: Arc::clone(&self.id),
            session: session.clone(),
            msg_tracing_out: self.msg_tracing_out,
        }
        .start();

        if let Err(e) = self
            .controller_addr
            .send(NewR2Session {
                addr: sender.clone().recipient(),
                id: self.id.clone(),
            })
            .await
        {
            error!("Error registering new WebSocket connection: {e}");
            let _ = session
                .close(Some(CloseReason {
                    code: CloseCode::Error,
                    description: Some("internal error".into()),
                }))
                .await;
            return;
        }

        debug!("[{}] started", self.id);

        let mut hb_interval = time::interval(self.heartbeat.interval);
        loop {
            tokio::select! {
                _ = hb_interval.tick() => {
                    if Instant::now().duration_since(self.hb) > self.heartbeat.timeout {
                        info!("[{}] Closing connection due to failed heartbeat", self.id);
                        close(session, Some(CloseReason {
                            code: CloseCode::Away,
                            description: Some("heartbeat timeout".into()),
                        })).await;
                        break;
                    }
                    // Also ends the loop if the session was closed.
                    if !matches!(timeout(SEND_TIMEOUT, session.ping(b"")).await, Ok(Ok(()))) {
                        info!("[{}] Closing connection: Remote does not read", self.id);
                        close(session, None).await;
                        break;
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            if let Err(close_reason) = self.handle_stream_message(msg, &mut session, &sender).await {
                                if close_reason.is_some() {
                                    close(session, close_reason).await;
                                }
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            info!("[{}] Closing WebSocket: {e:?}", self.id);
                            close(session, None).await;
                            break;
                        }
                        None => {
                            debug!("[{}] Message stream ended, closing connection", self.id);
                            close(session, None).await;
                            break;
                        }
                    }
                }
            }
        }

        self.controller_addr.do_send(R2SessionDisconnect {
            id: self.id.clone(),
        });
        debug!("[{}] stopped", self.id);
    }

    /// Process a received WebSocket message.
    ///
    /// Returns `Err` if the connection must be closed.
    async fn handle_stream_message(
        &mut self,
        msg: AggregatedMessage,
        session: &mut Session,
        sender: &Addr<WsSender>,
    ) -> Result<(), Option<CloseReason>> {
        self.hb = Instant::now();
        match msg {
            AggregatedMessage::Text(text) => {
                if self.msg_tracing_in {
                    if is_backup_message(&text) {
                        debug!("[{}] -> <backup data hidden>", self.id);
                    } else {
                        debug!("[{}] -> {text}", self.id);
                    }
                }
                self.handle_text_message(&text, sender)
            }
            AggregatedMessage::Binary(data) => self.handle_binary_message(data),
            AggregatedMessage::Ping(bytes) => {
                if timeout(SEND_TIMEOUT, session.pong(&bytes)).await.is_err() {
                    info!("[{}] Closing connection: Remote does not read", self.id);
                    return Err(None);
                }
                Ok(())
            }
            AggregatedMessage::Pong(_) => Ok(()),
            AggregatedMessage::Close(reason) => {
                info!("[{}] Remote closed connection. Reason: {reason:?}", self.id);
                Err(None)
            }
        }
    }

    fn handle_text_message(
        &self,
        text: &str,
        sender: &Addr<WsSender>,
    ) -> Result<(), Option<CloseReason>> {
        match serde_json::from_str::<WsMessage>(text) {
            Ok(msg) => {
                self.process_intg_api_message(msg, sender);
                Ok(())
            }
            Err(e) => {
                warn!("[{}] Invalid JSON message: {e}", self.id);
                Err(Some(CloseReason {
                    code: CloseCode::Unsupported,
                    description: Some("Invalid JSON message".into()),
                }))
            }
        }
    }

    /// Process a received Integration-API request, response or event message.
    ///
    /// A request runs in its own task, so a slow request does not stop reading the next
    /// messages. The responses are sent with `sender` and can come in a different order.
    fn process_intg_api_message(&self, mut msg: WsMessage, sender: &Addr<WsSender>) {
        let req_id = msg.id.unwrap_or_default();

        match msg.kind.as_deref() {
            Some("req") => {
                let id = Arc::clone(&self.id);
                let sender = sender.clone();
                let request = Self::on_request(&id, &mut msg, &self.controller_addr);
                actix_web::rt::spawn(async move {
                    let result = match request {
                        Ok(request) => request.await.unwrap_or_else(|e| Err(e.into())),
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok(Some(response)) => sender.do_send(SendWsMessage::Msg(response)),
                        Ok(None) => {}
                        Err(e) => {
                            warn!(
                                "[{id}] Error processing received message '{}': {e}",
                                msg.msg.as_deref().unwrap_or_default()
                            );
                            send_error_response(&id, req_id, e, &sender);
                        }
                    }
                });
            }
            Some("resp") => {
                if let Err(e) = Self::on_response(&self.id, msg, &self.controller_addr) {
                    warn!("[{}] Error processing response: {e}", self.id);
                }
            }
            Some("event") => {
                if let Err(e) = Self::on_event(&self.id, msg, &self.controller_addr) {
                    warn!("[{}] Error processing event: {e}", self.id);
                }
            }
            Some(other) => {
                warn!("[{}] Unsupported message kind: {other}", self.id);
                let e = ServiceError::BadRequest(format!("Unsupported message kind: {other}"));
                send_error_response(&self.id, req_id, e, sender);
            }
            None => {
                let e = ServiceError::BadRequest("Missing property: kind".into());
                send_error_response(&self.id, req_id, e, sender);
            }
        }
    }

    /// Handle a binary protobuf message. Only voice audio is supported.
    fn handle_binary_message(&mut self, data: Bytes) -> Result<(), Option<CloseReason>> {
        let msg = IntegrationMessage::decode(data).map_err(|e| {
            error!("[{}] Invalid binary message: {e:?}", self.id);
            None
        })?;
        let msg = msg.message.ok_or_else(|| {
            error!("[{}] Unable to decode binary message", self.id);
            None
        })?;

        match msg {
            ProtoMsg::VoiceBegin(begin) => {
                self.voice = VoiceCount {
                    session_id: begin.session_id,
                    ..VoiceCount::default()
                };
                if let Some(cfg) = begin.configuration {
                    info!(
                        "[{}] => voice_begin({}): {cfg:?}",
                        self.id, begin.session_id
                    );
                    // Siri needs 16 kHz mono signed 16 bit PCM. Any other format is rejected.
                    if cfg.format != AudioFormat::Pcm as i32
                        || cfg.sample_format != SampleFormat::I16 as i32
                        || cfg.sample_rate != VOICE_SAMPLE_RATE
                        || cfg.channels != 1
                    {
                        self.controller_addr
                            .do_send(R2AudioChunkMsg::end(begin.session_id));
                        error!(
                            "[{}] Invalid audio format, closing connection: {cfg:?}",
                            self.id
                        );
                        return Err(Some(CloseReason {
                            code: CloseCode::Unsupported,
                            description: Some("Invalid audio format".into()),
                        }));
                    }
                } else {
                    warn!(
                        "[{}] => voice_begin({}) without audio format, assuming requested format",
                        self.id, begin.session_id
                    );
                }
            }
            ProtoMsg::VoiceData(data) => {
                if self.voice.session_id != data.session_id {
                    self.voice = VoiceCount {
                        session_id: data.session_id,
                        ..VoiceCount::default()
                    };
                }
                self.voice.chunks += 1;
                self.voice.bytes += data.samples.len() as u64;
                // No reply needed: the read loop does not wait for the controller.
                self.controller_addr.do_send(R2AudioChunkMsg {
                    session_id: data.session_id,
                    data: data.samples,
                });
            }
            ProtoMsg::VoiceEnd(end) => {
                let count = if self.voice.session_id == end.session_id {
                    self.voice
                } else {
                    VoiceCount::default()
                };
                log_voice_end(&self.id, end.session_id, count);
                self.controller_addr
                    .do_send(R2AudioChunkMsg::end(end.session_id));
            }
        }
        Ok(())
    }

    /// Send a message to the WebSocket session. Returns `Err` if the session is closed.
    async fn send(
        id: &str,
        msg_tracing_out: bool,
        msg: &WsMessage,
        session: &mut Session,
    ) -> Result<(), Option<CloseReason>> {
        match serde_json::to_string(msg) {
            Ok(text) => {
                if msg_tracing_out {
                    if is_backup_message(&text) {
                        debug!("[{id}] <- <backup data hidden>");
                    } else {
                        debug!("[{id}] <- {text}");
                    }
                }
                session.text(text).await.map_err(|e| {
                    error!("[{id}] WebSocket send error: {e}");
                    None
                })
            }
            Err(e) => {
                error!("[{id}] Error serializing outgoing message: {e}");
                Ok(())
            }
        }
    }
}

/// Returns `true` for a setup message with backup data. It contains the pairing credentials.
fn is_backup_message(text: &str) -> bool {
    text.contains("\"backup_data\"") || text.contains("\"restore_data\"")
}

/// Close the session. Does not wait long if the Remote does not read.
async fn close(session: Session, reason: Option<CloseReason>) {
    let _ = timeout(SEND_TIMEOUT, session.close(reason)).await;
}

fn send_error_response(id: &str, req_id: u32, error: ServiceError, sender: &Addr<WsSender>) {
    sender.do_send(SendWsMessage::Msg(service_error_to_ws_message(
        id, req_id, error,
    )));
}

fn service_error_to_ws_message(id: &str, req_id: u32, error: ServiceError) -> WsMessage {
    debug!("[{id}] Sending error response for: {error:?}");

    let (code, ws_err) = match error {
        ServiceError::InternalServerError(_) => {
            (500, WsResultMsgData::new("ERROR", "Internal server error"))
        }
        ServiceError::SerializationError(e) | ServiceError::BadRequest(e) => {
            (400, WsResultMsgData::new("BAD_REQUEST", e))
        }
        ServiceError::NotConnected => (
            503,
            WsResultMsgData::new("SERVICE_UNAVAILABLE", "Apple TV is not connected"),
        ),
        ServiceError::ServiceUnavailable(e) => {
            (503, WsResultMsgData::new("SERVICE_UNAVAILABLE", e))
        }
        ServiceError::Timeout => (
            408,
            WsResultMsgData::new("TIMEOUT", "Apple TV did not answer"),
        ),
        ServiceError::NotFound(e) => (404, WsResultMsgData::new("NOT_FOUND", e)),
        ServiceError::NotSupported(e) => (501, WsResultMsgData::new("NOT_IMPLEMENTED", e)),
    };

    WsMessage::error(req_id, code, ws_err)
}

/// Logs the audio received in one voice session.
fn log_voice_end(id: &str, session_id: u32, count: VoiceCount) {
    // 16 bit mono samples.
    let secs = count.bytes as f64 / f64::from(VOICE_SAMPLE_RATE * 2);
    info!(
        "[{id}] => voice_end({session_id}): {} chunks, {} bytes, {secs:.2} s of audio",
        count.chunks, count.bytes
    );
}
#[cfg(test)]
mod tests {
    use super::is_backup_message;

    #[test]
    fn backup_messages_are_found() {
        assert!(is_backup_message(
            r#"{"input_values":{"restore_data":"x"}}"#
        ));
        assert!(is_backup_message(r#"{"settings":[{"id":"backup_data"}]}"#));
        assert!(!is_backup_message(r#"{"input_values":{"pin":"1234"}}"#));
    }
}
