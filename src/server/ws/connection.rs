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
    AudioConfiguration, AudioFormat, IntegrationMessage, SampleFormat,
    integration_message::Message as ProtoMsg,
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
        // Device events can come in bursts, for example at a connect: up to 26 messages per
        // Apple TV. Changes that do not fit are lost, because the controller sends only what
        // changed.
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

        // The driver does not check the auth token, so the authentication message is sent right away.
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
                self.handle_text_message(&text, sender);
                Ok(())
            }
            AggregatedMessage::Binary(data) => {
                self.handle_binary_message(data);
                Ok(())
            }
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

    /// Handle a JSON text message. A message that cannot be parsed is skipped. The connection
    /// stays open. A request with a readable id gets an error response.
    fn handle_text_message(&self, text: &str, sender: &Addr<WsSender>) {
        match serde_json::from_str::<WsMessage>(text) {
            Ok(msg) => self.process_intg_api_message(msg, sender),
            Err(e) => {
                // The error text has no part of `msg_data`, where PINs are.
                warn!("[{}] Invalid JSON message, skipped: {e}", self.id);
                if let Some(response) = invalid_message_response(&self.id, text, &e) {
                    sender.do_send(SendWsMessage::Msg(response));
                }
            }
        }
    }

    /// Process a received Integration-API request, response or event message.
    ///
    /// A request runs in its own task, so a slow request does not stop reading the next
    /// messages. The responses are sent with `sender`, or by the controller
    /// for responses it sends itself. They can come in a different order.
    fn process_intg_api_message(&self, mut msg: WsMessage, sender: &Addr<WsSender>) {
        match route(&msg) {
            Route::Request(req_id) => {
                let id = Arc::clone(&self.id);
                let sender = sender.clone();
                let request = Self::on_request(&id, req_id, &mut msg, &self.controller_addr);
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
            Route::Response => {
                if let Err(e) = Self::on_response(&self.id, msg, &self.controller_addr) {
                    warn!("[{}] Error processing response: {e}", self.id);
                }
            }
            Route::Event => {
                if let Err(e) = Self::on_event(&self.id, msg, &self.controller_addr) {
                    warn!("[{}] Error processing event: {e}", self.id);
                }
            }
            Route::RequestWithoutId => warn!("[{}] Request without id, skipped", self.id),
            Route::UnknownKind { kind, req_id } => {
                warn!("[{}] Unsupported message kind: {kind}", self.id);
                if let Some(req_id) = req_id {
                    let e = ServiceError::BadRequest(format!("Unsupported message kind: {kind}"));
                    send_error_response(&self.id, req_id, e, sender);
                }
            }
            Route::MissingKind { req_id } => {
                warn!("[{}] Message without kind", self.id);
                if let Some(req_id) = req_id {
                    let e = ServiceError::BadRequest("Missing property: kind".into());
                    send_error_response(&self.id, req_id, e, sender);
                }
            }
        }
    }

    /// Handle a binary protobuf message. Only voice audio is supported.
    ///
    /// A message that cannot be decoded is skipped. The connection stays open.
    fn handle_binary_message(&mut self, data: Bytes) {
        let msg = match IntegrationMessage::decode(data) {
            Ok(IntegrationMessage { message: Some(msg) }) => msg,
            Ok(IntegrationMessage { message: None }) => {
                warn!(
                    "[{}] Binary message without known content, skipped",
                    self.id
                );
                return;
            }
            Err(e) => {
                warn!("[{}] Invalid binary message, skipped: {e}", self.id);
                return;
            }
        };

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
                    // Only this voice session ends. Without audio, the Siri session sends an
                    // error event to the Remote.
                    if !supported(&cfg) {
                        self.voice.rejected = true;
                        self.audio.do_send(R2AudioChunkMsg::end(begin.session_id));
                        warn!(
                            "[{}] Unsupported audio format, voice session {} ended: {cfg:?}",
                            self.id, begin.session_id
                        );
                    }
                } else {
                    warn!(
                        "[{}] => voice_begin({}) without audio format, assuming requested format",
                        self.id, begin.session_id
                    );
                }
            }
            ProtoMsg::VoiceData(data) => {
                // Empty data is the end marker for the controller. Only `VoiceEnd` or an
                // unsupported format ends a session.
                if data.samples.is_empty() {
                    debug!("[{}] Empty voice data skipped", self.id);
                    return;
                }
                if self.voice.session_id != data.session_id {
                    self.voice = VoiceCount {
                        session_id: data.session_id,
                        ..VoiceCount::default()
                    };
                }
                if self.voice.rejected {
                    return;
                }
                self.voice.chunks += 1;
                self.voice.bytes += data.samples.len() as u64;
                // No reply needed: the read loop does not wait for the controller.
                self.audio.do_send(R2AudioChunkMsg {
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
                // Also sent for a rejected session: the first end marker can come before the
                // controller knows the session.
                self.audio.do_send(R2AudioChunkMsg::end(end.session_id));
            }
        }
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

/// How to handle a parsed message. Only a message with an id can get a response.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Request(u32),
    Response,
    Event,
    RequestWithoutId,
    /// Answered with an error if `req_id` is set.
    UnknownKind {
        kind: &'a str,
        req_id: Option<u32>,
    },
    /// Answered with an error if `req_id` is set.
    MissingKind {
        req_id: Option<u32>,
    },
}

fn route(msg: &WsMessage) -> Route<'_> {
    match (msg.kind.as_deref(), msg.id) {
        (Some("req"), Some(req_id)) => Route::Request(req_id),
        (Some("req"), None) => Route::RequestWithoutId,
        (Some("resp"), _) => Route::Response,
        (Some("event"), _) => Route::Event,
        (Some(kind), req_id) => Route::UnknownKind { kind, req_id },
        (None, req_id) => Route::MissingKind { req_id },
    }
}

/// Error response for a message that `WsMessage` cannot parse. Only a request with a readable id
/// gets one.
fn invalid_message_response(id: &str, text: &str, e: &serde_json::Error) -> Option<WsMessage> {
    match serde_json::from_str(text) {
        Ok(RequestId {
            kind: Some("req"),
            id: Some(req_id),
        }) => {
            let e = ServiceError::BadRequest(format!("Invalid message: {e}"));
            Some(service_error_to_ws_message(id, req_id, e))
        }
        _ => None,
    }
}

/// The fields of a request that are needed for an error response.
#[derive(serde::Deserialize)]
struct RequestId<'a> {
    #[serde(borrow)]
    kind: Option<&'a str>,
    id: Option<u32>,
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

/// Returns `true` for the audio format that Siri needs: 16 kHz mono signed 16 bit PCM.
/// An unset `format` or `channels` (0) means the default: PCM and 1 channel.
const fn supported(cfg: &AudioConfiguration) -> bool {
    (cfg.format == AudioFormat::Unknown as i32 || cfg.format == AudioFormat::Pcm as i32)
        && cfg.sample_format == SampleFormat::I16 as i32
        && cfg.sample_rate == VOICE_SAMPLE_RATE
        && cfg.channels <= 1
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
    use super::{RequestId, Route, invalid_message_response, is_backup_message, route, supported};
    use crate::configuration::HeartbeatSettings;
    use crate::controller::R2AudioChunkMsg;
    use crate::controller::test_util::controller;
    use crate::server::ws::WsConn;
    use actix::{Actor, Handler, MessageResult};
    use prost::Message;
    use uc_api::intg::proto::{
        AudioConfiguration, AudioFormat, IntegrationMessage, RemoteVoiceBegin, RemoteVoiceData,
        RemoteVoiceEnd, SampleFormat, integration_message::Message as ProtoMsg,
    };
    use uc_api::ws::WsMessage;

    fn config(
        channels: u32,
        sample_rate: u32,
        sample_format: SampleFormat,
        format: i32,
    ) -> AudioConfiguration {
        AudioConfiguration {
            channels,
            sample_rate,
            sample_format: sample_format as i32,
            format,
        }
    }

    #[test]
    fn supported_audio_format() {
        let pcm = AudioFormat::Pcm as i32;
        let unset = AudioFormat::Unknown as i32;
        assert!(supported(&config(1, 16_000, SampleFormat::I16, pcm)));
        assert!(supported(&config(0, 16_000, SampleFormat::I16, pcm)));
        assert!(supported(&config(1, 16_000, SampleFormat::I16, unset)));
        assert!(!supported(&config(2, 16_000, SampleFormat::I16, pcm)));
        assert!(!supported(&config(1, 16_000, SampleFormat::I16, 7)));
        assert!(!supported(&config(1, 16_000, SampleFormat::I32, pcm)));
        assert!(!supported(&config(1, 16_000, SampleFormat::Unknown, pcm)));
        assert!(!supported(&config(1, 8_000, SampleFormat::I16, pcm)));
        assert!(!supported(&config(1, 0, SampleFormat::I16, pcm)));
    }

    fn encode(msg: ProtoMsg) -> bytes::Bytes {
        IntegrationMessage { message: Some(msg) }
            .encode_to_vec()
            .into()
    }

    /// Collects the session id and size of each audio chunk.
    #[derive(Default)]
    struct AudioProbe(Vec<(u32, usize)>);

    impl Actor for AudioProbe {
        type Context = actix::Context<Self>;
    }

    impl Handler<R2AudioChunkMsg> for AudioProbe {
        type Result = ();
        fn handle(&mut self, msg: R2AudioChunkMsg, _: &mut Self::Context) {
            self.0.push((msg.session_id, msg.data.len()));
        }
    }

    /// Returns the chunks received so far. Comes after all chunks sent before it.
    #[derive(actix::Message)]
    #[rtype(result = "Vec<(u32, usize)>")]
    struct Take;

    impl Handler<Take> for AudioProbe {
        type Result = MessageResult<Take>;
        fn handle(&mut self, _: Take, _: &mut Self::Context) -> Self::Result {
            MessageResult(std::mem::take(&mut self.0))
        }
    }

    #[actix::test]
    async fn bad_voice_format_ends_only_the_voice_session() {
        let mut conn = WsConn::new(
            "test".into(),
            controller("ws-voice"),
            HeartbeatSettings::default(),
        );
        let probe = AudioProbe::default().start();
        conn.audio = probe.clone().recipient();
        let pcm = AudioFormat::Pcm as i32;
        // Not a valid protobuf message: skipped.
        conn.handle_binary_message(bytes::Bytes::from_static(&[0xff, 0xff, 0xff]));
        // An empty message has no content: skipped.
        assert!(
            IntegrationMessage::decode(bytes::Bytes::new())
                .expect("decode")
                .message
                .is_none()
        );
        conn.handle_binary_message(bytes::Bytes::new());

        conn.handle_binary_message(encode(ProtoMsg::VoiceBegin(RemoteVoiceBegin {
            session_id: 7,
            configuration: Some(config(2, 16_000, SampleFormat::I16, pcm)),
        })));
        conn.handle_binary_message(encode(ProtoMsg::VoiceData(RemoteVoiceData {
            session_id: 7,
            samples: vec![0; 64].into(),
        })));
        conn.handle_binary_message(encode(ProtoMsg::VoiceEnd(RemoteVoiceEnd { session_id: 7 })));
        // End marker at the bad begin, no audio, end marker at the end.
        assert_eq!(vec![(7, 0), (7, 0)], probe.send(Take).await.expect("probe"));

        // The next session works again.
        conn.handle_binary_message(encode(ProtoMsg::VoiceBegin(RemoteVoiceBegin {
            session_id: 8,
            configuration: Some(config(1, 16_000, SampleFormat::I16, pcm)),
        })));
        conn.handle_binary_message(encode(ProtoMsg::VoiceData(RemoteVoiceData {
            session_id: 8,
            samples: vec![0; 64].into(),
        })));
        // Empty data does not end the session.
        conn.handle_binary_message(encode(ProtoMsg::VoiceData(RemoteVoiceData {
            session_id: 8,
            samples: bytes::Bytes::new(),
        })));
        assert_eq!(vec![(8, 64)], probe.send(Take).await.expect("probe"));
        assert_eq!(1, conn.voice.chunks);
        assert_eq!(64, conn.voice.bytes);
    }

    #[test]
    fn route_needs_id_for_a_response() {
        let msg = |text: &str| serde_json::from_str::<WsMessage>(text).expect("json");
        let req = msg(r#"{"kind":"req","id":5,"msg":"get_version"}"#);
        assert_eq!(Route::Request(5), route(&req));
        let req = msg(r#"{"kind":"req","msg":"get_version"}"#);
        assert_eq!(Route::RequestWithoutId, route(&req));
        assert_eq!(
            Route::Response,
            route(&msg(r#"{"kind":"resp","req_id":1}"#))
        );
        assert_eq!(Route::Event, route(&msg(r#"{"kind":"event","msg":"x"}"#)));
        let other = msg(r#"{"kind":"foo","id":3}"#);
        assert_eq!(
            Route::UnknownKind {
                kind: "foo",
                req_id: Some(3)
            },
            route(&other)
        );
        let none = msg(r#"{"msg":"x"}"#);
        assert_eq!(Route::MissingKind { req_id: None }, route(&none));
        let unknown = msg(r#"{"kind":"foo"}"#);
        assert_eq!(
            Route::UnknownKind {
                kind: "foo",
                req_id: None
            },
            route(&unknown)
        );
        let missing = msg(r#"{"id":4,"msg":"x"}"#);
        assert_eq!(Route::MissingKind { req_id: Some(4) }, route(&missing));
    }

    #[test]
    fn invalid_message_response_only_for_requests_with_id() {
        let response = |text: &str| {
            let e = serde_json::from_str::<WsMessage>(text).expect_err("invalid");
            invalid_message_response("test", text, &e)
                .map(|m| serde_json::to_value(m).expect("json"))
        };
        let resp =
            response(r#"{"kind":"req","id":5,"msg":"get_version","cat":"x"}"#).expect("response");
        assert_eq!(5, resp["req_id"]);
        assert_eq!(400, resp["code"]);
        assert_eq!("BAD_REQUEST", resp["msg_data"]["code"]);
        assert!(response(r#"{"kind":"event","id":5,"cat":"x"}"#).is_none());
        assert!(response(r#"{"kind":"req","cat":"x"}"#).is_none());
        assert!(response("{not json").is_none());
    }

    #[test]
    fn request_id_of_invalid_message() {
        // `cat` is not a valid event category, so `WsMessage` cannot parse it.
        let text = r#"{"kind":"req","id":5,"msg":"get_version","cat":"x"}"#;
        assert!(serde_json::from_str::<WsMessage>(text).is_err());
        let id: RequestId<'_> = serde_json::from_str(text).expect("id");
        assert_eq!((Some("req"), Some(5)), (id.kind, id.id));
        assert!(serde_json::from_str::<RequestId<'_>>("{not json").is_err());
    }

    #[test]
    fn backup_messages_are_found() {
        assert!(is_backup_message(
            r#"{"input_values":{"restore_data":"x"}}"#
        ));
        assert!(is_backup_message(r#"{"settings":[{"id":"backup_data"}]}"#));
        assert!(!is_backup_message(r#"{"input_values":{"pin":"1234"}}"#));
    }
}
