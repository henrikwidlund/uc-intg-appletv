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
use actix_ws::{AggregatedMessage, CloseCode, CloseReason, ProtocolError, Session};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::{debug, error, info, warn};
use prost::Message;
use std::borrow::Cow;
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
/// Longest wait for a close frame after a read error.
const BUFFERED_CLOSE_TIMEOUT: Duration = Duration::from_millis(100);
/// This many messages that are not a JSON object within [`INVALID_WINDOW`] close the connection.
/// A Remote does not send them: many of them are a bug or another client.
const INVALID_LIMIT: u8 = 5;
/// Starts at the first message that is not a JSON object.
const INVALID_WINDOW: Duration = Duration::from_secs(60);
/// Keys with secret string values: the pairing PIN, and the backup with the pairing keys.
const SECRET_KEYS: [&str; 3] = ["\"pin\"", "\"restore_data\"", "\"backup_data\""];

/// Sends messages from the controller to the WebSocket connection.
///
/// Messages are sent one after another. Events have no id, so the Remote must get them in the
/// order they were created. A send only puts the frame in the session queue, so this is fast.
/// Responses to requests are sent here too.
struct WsSender {
    id: Arc<str>,
    session: Session,
    msg_tracing_out: bool,
    /// Start of the last send, for the trace. `handle` runs only after the previous send is done,
    /// because `wait` blocks the mailbox.
    last_send: Option<Instant>,
    /// The session is closed: the messages that wait are dropped.
    closed: bool,
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
        if self.closed {
            return;
        }
        let id = Arc::clone(&self.id);
        let trace = self.msg_tracing_out.then(|| {
            let now = Instant::now();
            self.last_send
                .replace(now)
                .map_or(Duration::ZERO, |last| now.duration_since(last))
        });
        let mut session = self.session.clone();

        // `wait` blocks the mailbox until the message is sent: keeps the order.
        ctx.wait(
            async move {
                let send = async {
                    match msg {
                        SendWsMessage::Msg(m) => WsConn::send(&id, trace, &m, &mut session).await,
                        SendWsMessage::Text(text) => {
                            if let Some(gap) = trace {
                                trace_out(&id, gap, &text);
                            }
                            session.text(text).await.map_err(|_| None)
                        }
                    }
                };
                let Ok(result) = timeout(SEND_TIMEOUT, send).await else {
                    // A lost message leaves an old state on the Remote. After a new connection,
                    // the Remote gets the full state again.
                    warn!("[{id}] Remote does not read, closing the connection");
                    close(session, None).await;
                    return true;
                };
                result.is_err()
            }
            .into_actor(self)
            .map(|closed, act, _| {
                if closed && !act.closed {
                    act.closed = true;
                    debug!(
                        "[{}] WebSocket closed, waiting messages are dropped",
                        act.id
                    );
                }
            }),
        );
    }
}

/// Counts messages that are not a JSON object within [`INVALID_WINDOW`].
#[derive(Debug)]
pub(super) struct InvalidMessages {
    count: u8,
    since: Instant,
}

impl InvalidMessages {
    pub(super) fn new() -> Self {
        Self {
            count: 0,
            since: Instant::now(),
        }
    }

    /// Count one invalid message. True if the connection must be closed. The window starts at
    /// the first invalid message.
    fn add(&mut self, now: Instant) -> bool {
        if self.count == 0 || now.duration_since(self.since) > INVALID_WINDOW {
            self.since = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count >= INVALID_LIMIT
    }
}

/// Returns [`WsSender::closed`].
#[cfg(test)]
#[derive(actix::Message)]
#[rtype(result = "bool")]
struct IsClosed;

#[cfg(test)]
impl Handler<IsClosed> for WsSender {
    type Result = bool;

    fn handle(&mut self, _: IsClosed, _: &mut Self::Context) -> bool {
        self.closed
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

        // The driver does not check the auth token, so the authentication message is sent right away.
        let _ = session.text(self.authentication.clone()).await;

        let sender = WsSender {
            id: Arc::clone(&self.id),
            session: session.clone(),
            msg_tracing_out: self.msg_tracing_out,
            last_send: None,
            closed: false,
        }
        .start();

        if let Err(e) = self
            .controller_addr
            .send(NewR2Session {
                addr: sender.clone().recipient(),
                id: self.id.clone(),
                local_ip: self.local_ip,
            })
            .await
        {
            error!(
                "[{}] Error registering new WebSocket connection: {e}",
                self.id
            );
            let _ = session
                .close(Some(CloseReason {
                    code: CloseCode::Error,
                    description: Some("internal error".into()),
                }))
                .await;
            debug!("[{}] stopped", self.id);
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
                            let reply = if let Some(reason) = buffered_close(&mut stream).await {
                                debug!("[{}] Read error before the close frame: {e:?}", self.id);
                                info!(
                                    "[{}] Remote closed connection. Reason: {reason:?}",
                                    self.id
                                );
                                // Answered like a close in `handle_stream_message`.
                                Some(reason.unwrap_or_else(|| CloseCode::Normal.into()))
                            } else {
                                info!("[{}] Closing WebSocket: {e:?}", self.id);
                                None
                            };
                            close(session, reply).await;
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
                    debug!("[{}] -> {}", self.id, redact(&text));
                }
                self.handle_text_message(&text, sender)
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
                // The close is answered with a close, with the same code and description.
                Err(Some(reason.unwrap_or_else(|| CloseCode::Normal.into())))
            }
        }
    }

    /// Handle a JSON text message. A message that cannot be parsed is skipped. A request with a
    /// readable id gets an error response. The connection is closed after [`INVALID_LIMIT`]
    /// messages that are not a JSON object within [`INVALID_WINDOW`]. A JSON object of an unknown
    /// form is not counted: a newer Remote can send it.
    fn handle_text_message(
        &mut self,
        text: &str,
        sender: &Addr<WsSender>,
    ) -> Result<(), Option<CloseReason>> {
        let e = match serde_json::from_str::<WsMessage>(text) {
            Ok(msg) => {
                self.process_intg_api_message(msg, sender);
                return Ok(());
            }
            Err(e) => e,
        };
        let json = text.trim_start().starts_with('{')
            && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok();
        if !json && self.invalid.add(Instant::now()) {
            warn!(
                "[{}] {INVALID_LIMIT} invalid messages, closing the connection: {e}",
                self.id
            );
            return Err(Some(CloseReason {
                code: CloseCode::Policy,
                description: Some("Invalid messages".into()),
            }));
        }
        // The error text has no part of `msg_data`, where PINs are.
        if json {
            warn!("[{}] Unknown message, skipped: {e}", self.id);
        } else {
            warn!("[{}] Invalid message, skipped: {e}", self.id);
        }
        if json && let Some(response) = invalid_message_response(&self.id, text, &e) {
            sender.do_send(SendWsMessage::Msg(response));
        }
        Ok(())
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
    ///
    /// `trace` is the time since the previous outgoing message. `None` if outgoing messages are not
    /// traced.
    async fn send(
        id: &str,
        trace: Option<Duration>,
        msg: &WsMessage,
        session: &mut Session,
    ) -> Result<(), Option<CloseReason>> {
        match serde_json::to_string(msg) {
            Ok(text) => {
                if let Some(gap) = trace {
                    trace_out(id, gap, &text);
                }
                // The only error is a closed session. The caller logs it once.
                session.text(text).await.map_err(|_| None)
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

/// Logs an outgoing message with the time since the previous one. Secrets are hidden.
fn trace_out(id: &str, gap: Duration, text: &str) {
    debug!("[{id}] <- (+{} ms) {}", gap.as_millis(), redact(text));
}

/// `text` with secret string values replaced by `***`, for the message trace. Empty values stay,
/// so the trace shows that a field was empty. The backup page has the backup in a `value` field,
/// so in a message with `backup_data` every `value` string is replaced too.
fn redact(text: &str) -> Cow<'_, str> {
    let mut ranges = Vec::new();
    for key in SECRET_KEYS {
        string_values(text, key, &mut ranges);
    }
    if text.contains("\"backup_data\"") {
        string_values(text, "\"value\"", &mut ranges);
    }
    if ranges.is_empty() {
        return Cow::Borrowed(text);
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in ranges {
        if start < at {
            continue;
        }
        out.push_str(&text[at..start]);
        out.push_str("***");
        at = end;
    }
    out.push_str(&text[at..]);
    Cow::Owned(out)
}

/// Adds the byte ranges of the non-empty string values of `key` in the JSON `text`. A range is the
/// part between the quotes, or up to the end of `text` if the closing quote is missing. `key`
/// includes its quotes.
fn string_values(text: &str, key: &str, ranges: &mut Vec<(usize, usize)>) {
    for (pos, _) in text.match_indices(key) {
        let Some(value) = text[pos + key.len()..].trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(content) = value.trim_start().strip_prefix('"') else {
            continue;
        };
        let start = text.len() - content.len();
        let mut escaped = false;
        let end = content.bytes().position(|b| {
            let quote = !escaped && b == b'"';
            escaped = !escaped && b == b'\\';
            quote
        });
        // Without a closing quote, the rest of the text is hidden.
        let end = end.unwrap_or(content.len());
        if end > 0 {
            ranges.push((start, start + end));
        }
    }
}

/// The close frame that the Remote sent just before the connection ended, if any.
///
/// When a close frame and the end of the connection arrive together, actix-ws returns the read
/// error first and the close frame on the next read. The Remote closes its socket right after its
/// close frame, so this is the normal case for a close by the Remote.
///
/// Other messages are skipped. Stops at an error, at the end of the stream, or after
/// [`BUFFERED_CLOSE_TIMEOUT`]. `Some(None)` is a close frame without a reason.
async fn buffered_close<S>(stream: &mut S) -> Option<Option<CloseReason>>
where
    S: Stream<Item = Result<AggregatedMessage, ProtocolError>> + Unpin,
{
    let find = async {
        while let Some(Ok(msg)) = stream.next().await {
            if let AggregatedMessage::Close(reason) = msg {
                return Some(reason);
            }
        }
        None
    };
    timeout(BUFFERED_CLOSE_TIMEOUT, find).await.ok().flatten()
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
    use super::{
        INVALID_LIMIT, INVALID_WINDOW, InvalidMessages, RequestId, Route, invalid_message_response,
        redact, route, supported,
    };
    use crate::configuration::HeartbeatSettings;
    use crate::controller::R2AudioChunkMsg;
    use crate::controller::test_util::controller;
    use crate::server::ws::WsConn;
    use actix::{Actor, Handler, MessageResult};
    use bytestring::ByteString;
    use prost::Message;
    use std::borrow::Cow;
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
            None,
            controller("ws-voice"),
            HeartbeatSettings::default(),
            ByteString::new(),
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
    fn invalid_messages_reach_the_limit() {
        let mut invalid = InvalidMessages::new();
        let start = std::time::Instant::now();
        for _ in 1..INVALID_LIMIT {
            assert!(!invalid.add(start));
        }
        assert!(invalid.add(start));
    }

    #[test]
    fn invalid_messages_are_counted_again_after_the_window() {
        let mut invalid = InvalidMessages::new();
        let start = std::time::Instant::now();
        for _ in 1..INVALID_LIMIT {
            assert!(!invalid.add(start));
        }
        let later = start + INVALID_WINDOW + std::time::Duration::from_secs(1);
        for _ in 1..INVALID_LIMIT {
            assert!(!invalid.add(later));
        }
        assert!(invalid.add(later), "new window full");
    }

    #[test]
    fn invalid_window_starts_at_the_first_invalid_message() {
        let mut invalid = InvalidMessages::new();
        let first = std::time::Instant::now() + std::time::Duration::from_secs(50);
        assert!(!invalid.add(first));
        let later = first + std::time::Duration::from_secs(15);
        for _ in 2..INVALID_LIMIT {
            assert!(!invalid.add(later));
        }
        assert!(invalid.add(later), "within the window of the first message");
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
    fn trace_hides_pin_and_backup() {
        assert_eq!(
            r#"{"kind":"req","msg":"set_driver_user_data","msg_data":{"input_values":{"pin":"***"}}}"#,
            redact(
                r#"{"kind":"req","msg":"set_driver_user_data","msg_data":{"input_values":{"pin":"1234"}}}"#
            )
        );
        assert_eq!(
            r#"{"input_values": {"restore_data" : "***", "address":"10.0.0.2"}}"#,
            redact(
                r#"{"input_values": {"restore_data" : "[{\"credentials\":\"k\"}]", "address":"10.0.0.2"}}"#
            )
        );
        // Keys are sorted: the backup `value` comes before `"id":"backup_data"`.
        assert_eq!(
            r#"{"settings":[{"field":{"textarea":{"value":"***"}},"id":"backup_data"}]}"#,
            redact(
                r#"{"settings":[{"field":{"textarea":{"value":"[{\"a\":1}]"}},"id":"backup_data"}]}"#
            )
        );
    }

    #[test]
    fn trace_hides_escapes_utf8_and_cut_values() {
        assert_eq!(r#"{"pin":"***","a":1}"#, redact(r#"{"pin":"12\\","a":1}"#));
        assert_eq!(
            r#"{"pin":"***","a":1}"#,
            redact(r#"{"pin":"1\\\"2","a":1}"#)
        );
        assert_eq!(
            r#"{"pin":"***","ä":"€"}"#,
            redact(r#"{"pin":"ü€x","ä":"€"}"#)
        );
        assert_eq!(
            r#"{"input_values":{"pin":"***"#,
            redact(r#"{"input_values":{"pin":"1234"#)
        );
        let page = r#"{"msg_data":{"error_message":{"en":"no"},"page":{"settings":[{"field":{"label":{"value":{"en":"Copy"}}},"id":"info"},{"field":{"textarea":{"value":"[{\"k\":\"s\"}]"}},"id":"backup_data"}]}}}"#;
        let hidden = r#"{"msg_data":{"error_message":{"en":"no"},"page":{"settings":[{"field":{"label":{"value":{"en":"Copy"}}},"id":"info"},{"field":{"textarea":{"value":"***"}},"id":"backup_data"}]}}}"#;
        assert_eq!(hidden, redact(page));
    }

    #[test]
    fn trace_keeps_other_values() {
        let page = r#"{"settings":[{"field":{"text":{"value":""}},"id":"pin"}]}"#;
        assert!(
            matches!(redact(page), Cow::Borrowed(_)),
            "empty PIN field and the id stay"
        );
        let text = r#"{"msg":"entity_change","msg_data":{"attributes":{"media_title":"pin"}}}"#;
        assert!(matches!(redact(text), Cow::Borrowed(_)));
        // Text with `\"pin\":\"x\"` inside another string stays.
        let text = r#"{"msg":"x","text":"a \"pin\":\"x\" b"}"#;
        assert!(matches!(redact(text), Cow::Borrowed(_)));
        // An empty value stays, so the trace shows that the field was empty.
        assert_eq!(
            r#"{"input_values":{"pin":""}}"#,
            redact(r#"{"input_values":{"pin":""}}"#)
        );
    }

    /// A WebSocket client over a plain TCP stream, for the close handshake.
    mod raw {
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpStream};
        use std::time::Duration;

        pub fn connect(addr: SocketAddr) -> TcpStream {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            s.write_all(
                b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
                  Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\r\n",
            )
            .expect("write");
            let mut head = Vec::new();
            let mut b = [0u8];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut b).expect("read");
                head.push(b[0]);
            }
            assert!(head.starts_with(b"HTTP/1.1 101"), "upgrade");
            s
        }

        /// Read one frame from the server. Returns the opcode and the payload.
        pub fn read(s: &mut TcpStream) -> (u8, Vec<u8>) {
            let mut h = [0u8; 2];
            s.read_exact(&mut h).expect("read");
            let len = match h[1] & 0x7f {
                126 => {
                    let mut l = [0u8; 2];
                    s.read_exact(&mut l).expect("read");
                    usize::from(u16::from_be_bytes(l))
                }
                127 => panic!("frame too long"),
                n => usize::from(n),
            };
            let mut payload = vec![0; len];
            s.read_exact(&mut payload).expect("read");
            (h[0] & 0x0f, payload)
        }

        /// Send a masked frame from the client, as the protocol requires.
        pub fn send(s: &mut TcpStream, opcode: u8, payload: &[u8]) {
            let mask = [1u8, 2, 3, 4];
            let len = u8::try_from(payload.len()).expect("short");
            assert!(len < 126);
            let mut frame = vec![0x80 | opcode, 0x80 | len];
            frame.extend_from_slice(&mask);
            frame.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
            s.write_all(&frame).expect("write");
        }
    }

    /// The local address of the Remote connection reaches the controller.
    #[actix::test]
    async fn remote_connection_address_is_stored() {
        use crate::controller::test_util::RemoteIp;
        use crate::server::{Authentication, save_local_address, ws_index};
        use actix_web::{App, HttpServer, web};

        let addr = controller("ws-local-ip");
        let controller = web::Data::new(addr.clone());
        let heartbeat = web::Data::new(HeartbeatSettings::default());
        let metadata = serde_json::from_value(serde_json::json!({})).expect("metadata");
        let auth = web::Data::new(Authentication::new(&metadata));
        let server = HttpServer::new(move || {
            App::new()
                .app_data(heartbeat.clone())
                .app_data(auth.clone())
                .app_data(controller.clone())
                .service(ws_index)
        })
        .on_connect(save_local_address)
        .workers(1)
        .disable_signals()
        .bind(("127.0.0.1", 0))
        .expect("bind");
        let server_addr = server.addrs()[0];
        let server = server.run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        let client = tokio::task::spawn_blocking(move || raw::connect(server_addr))
            .await
            .expect("client");
        let mut stored = None;
        // Up to 5 s, like the read timeout of the raw client.
        for _ in 0..250 {
            stored = addr.send(RemoteIp).await.expect("send");
            if stored.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(Some(server_addr.ip()), stored);
        drop(client);
        handle.stop(false).await;
    }

    #[actix::test]
    async fn buffered_close_finds_close_or_stops() {
        use super::buffered_close;
        use actix_ws::{AggregatedMessage, CloseCode, ProtocolError};
        use futures::stream;

        let close = AggregatedMessage::Close(Some(CloseCode::Normal.into()));
        let ping = AggregatedMessage::Ping(bytes::Bytes::new());
        let mut s = stream::iter([Ok(ping), Ok(close)]);
        assert_eq!(
            Some(Some(CloseCode::Normal.into())),
            buffered_close(&mut s).await
        );
        let mut s = stream::iter([Ok(AggregatedMessage::Close(None))]);
        assert_eq!(Some(None), buffered_close(&mut s).await);
        let mut s = stream::iter(Vec::<Result<AggregatedMessage, ProtocolError>>::new());
        assert_eq!(None, buffered_close(&mut s).await);
        let mut s = stream::iter([
            Err(ProtocolError::Overflow),
            Ok(AggregatedMessage::Close(None)),
        ]);
        assert_eq!(None, buffered_close(&mut s).await);
        // `buffered_close` stops at the time limit for a stream that does not end.
        let mut s = stream::pending::<Result<AggregatedMessage, ProtocolError>>();
        assert_eq!(None, buffered_close(&mut s).await);
    }

    #[actix::test]
    async fn invalid_messages_get_errors_and_close_the_connection() {
        use crate::server::{Authentication, ws_index};
        use actix_web::{App, HttpServer, web};

        let controller = web::Data::new(controller("ws-invalid"));
        let heartbeat = web::Data::new(HeartbeatSettings::default());
        let metadata = serde_json::from_value(serde_json::json!({})).expect("metadata");
        let auth = web::Data::new(Authentication::new(&metadata));
        let server = HttpServer::new(move || {
            App::new()
                .app_data(heartbeat.clone())
                .app_data(auth.clone())
                .app_data(controller.clone())
                .service(ws_index)
        })
        .workers(1)
        .disable_signals()
        .bind(("127.0.0.1", 0))
        .expect("bind");
        let addr = server.addrs()[0];
        let server = server.run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        let (answers, close) = tokio::task::spawn_blocking(move || {
            let mut s = raw::connect(addr);
            // The authentication message.
            assert_eq!(1, raw::read(&mut s).0);
            for text in [
                // Not a JSON object: counted, no response.
                "{not json",
                "[1,",
                "[]",
                // A JSON object of an unknown form: not counted. With an id: an error response.
                r#"{"kind":"event","cat":"x"}"#,
                r#"{"kind":"req","cat":"x"}"#,
                r#"{"kind":"req","id":8,"msg":"x","cat":"x"}"#,
                // Valid with an id: a response.
                r#"{"kind":"req","id":7,"msg":"get_driver_version"}"#,
                // The 4th message that is not a JSON object.
                "{",
            ] {
                raw::send(&mut s, 0x1, text.as_bytes());
            }
            let keep = |answers: &mut Vec<serde_json::Value>, payload: &[u8]| {
                let value: serde_json::Value = serde_json::from_slice(payload).expect("json");
                if value["kind"] == "resp" {
                    answers.push(value);
                }
            };
            let mut answers = Vec::new();
            while answers.len() < 2 {
                let (opcode, payload) = raw::read(&mut s);
                if opcode == 0x1 {
                    keep(&mut answers, &payload);
                }
            }
            // The 5th message that is not a JSON object closes the connection.
            raw::send(&mut s, 0x1, b"]");
            loop {
                let (opcode, payload) = raw::read(&mut s);
                match opcode {
                    0x1 => keep(&mut answers, &payload),
                    0x8 => {
                        let code = payload
                            .first_chunk::<2>()
                            .map(|code| u16::from_be_bytes(*code));
                        return (answers, code);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("client");
        handle.stop(false).await;

        assert_eq!(Some(1008), close, "policy close code");
        let mut ids: Vec<_> = answers
            .iter()
            .filter_map(|a| a["req_id"].as_u64())
            .collect();
        ids.sort_unstable();
        assert_eq!(2, answers.len(), "{answers:?}");
        assert_eq!(vec![7, 8], ids, "{answers:?}");
        let error = answers.iter().find(|a| a["req_id"] == 8).expect("error");
        assert_eq!(400, error["code"]);
    }

    /// After a failed send, the sender is marked closed.
    #[actix::test]
    async fn sender_stops_after_a_failed_send() {
        use super::{IsClosed, WsSender};
        use crate::controller::SendWsMessage;
        use actix_web::{FromRequest, test::TestRequest, web};

        let (req, mut payload) = TestRequest::default()
            .insert_header(("upgrade", "websocket"))
            .insert_header(("connection", "upgrade"))
            .insert_header(("sec-websocket-version", "13"))
            .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
            .to_http_parts();
        let body = web::Payload::from_request(&req, &mut payload)
            .await
            .expect("payload");
        let (response, session, _stream) = actix_ws::handle(&req, body).expect("upgrade");
        // The response holds the receiving end: without it each send fails.
        drop(response);
        let sender = WsSender {
            id: "test".into(),
            session,
            msg_tracing_out: false,
            last_send: None,
            closed: false,
        }
        .start();
        assert!(!sender.send(IsClosed).await.expect("send"));
        sender
            .send(SendWsMessage::Text("{}".into()))
            .await
            .expect("send");
        assert!(
            sender.send(IsClosed).await.expect("send"),
            "closed after the error"
        );
    }

    #[actix::test]
    async fn close_is_answered_with_the_same_code_and_reason() {
        use crate::server::{Authentication, ws_index};
        use actix_web::{App, HttpServer, web};

        let controller = web::Data::new(controller("ws-close"));
        let heartbeat = web::Data::new(HeartbeatSettings::default());
        let metadata = serde_json::from_value(serde_json::json!({})).expect("metadata");
        let auth = web::Data::new(Authentication::new(&metadata));
        let server = HttpServer::new(move || {
            App::new()
                .app_data(heartbeat.clone())
                .app_data(auth.clone())
                .app_data(controller.clone())
                .service(ws_index)
        })
        .workers(1)
        .disable_signals()
        .bind(("127.0.0.1", 0))
        .expect("bind");
        let addr = server.addrs()[0];
        let server = server.run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        let answer = tokio::task::spawn_blocking(move || {
            let mut s = raw::connect(addr);
            let mut close = 4000u16.to_be_bytes().to_vec();
            close.extend_from_slice(b"bye");
            raw::send(&mut s, 0x8, &close);
            loop {
                let (opcode, payload) = raw::read(&mut s);
                if opcode == 0x8 {
                    return payload;
                }
            }
        })
        .await
        .expect("client");
        let mut expected = 4000u16.to_be_bytes().to_vec();
        expected.extend_from_slice(b"bye");
        assert_eq!(expected, answer);
        handle.stop(false).await;
    }
}
