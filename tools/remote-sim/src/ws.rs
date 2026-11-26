// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket client that acts as the Remote. It uses the WebSocket client of the Remote's core
//! service (`actix-ws-client`).

use crate::Fail;
use actix::{Actor, Addr};
use actix_ws_client::{
    Connect, Disconnect, DisconnectReason, GenericWebSocketClient, HandlerAction, IsConnected,
    SendBinary, SendText, WebSocketConfig, WebSocketMessageHandler,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Longest time to wait for the answer to a request. The driver must answer in less than 10 s.
const ANSWER_LIMIT: Duration = Duration::from_secs(15);

/// What the WebSocket client reports to [`Client`].
enum Inbound {
    /// Time of the connect.
    Connected(Instant),
    Text(String),
    Closed(DisconnectReason),
}

/// Sends the callbacks of the WebSocket client to [`Client`].
struct Bridge(mpsc::UnboundedSender<Inbound>);

impl WebSocketMessageHandler for Bridge {
    fn on_text(&mut self, text: String) -> Vec<HandlerAction> {
        let _ = self.0.send(Inbound::Text(text));
        Vec::new()
    }

    fn on_connected(&mut self) -> Vec<HandlerAction> {
        let _ = self.0.send(Inbound::Connected(Instant::now()));
        Vec::new()
    }

    fn on_closed(&mut self, reason: &DisconnectReason) -> Vec<HandlerAction> {
        let _ = self.0.send(Inbound::Closed(reason.clone()));
        Vec::new()
    }
}

pub struct Client {
    /// The only address of the client: dropping it closes the connection and stops the client.
    ws: Addr<GenericWebSocketClient<Bridge>>,
    inbound: mpsc::UnboundedReceiver<Inbound>,
    /// Why the connection closed. `None` until a close is read, and again when
    /// [`Self::wait_connected`] gets a new connection.
    closed: Option<DisconnectReason>,
    next_id: u32,
    /// Latest attributes of each entity, from `entity_change` events and entity states.
    attrs: HashMap<String, Map<String, Value>>,
    /// Setup events that were not read yet.
    setup: VecDeque<Value>,
    /// `assistant_event` data that was not read yet.
    assistant: VecDeque<Value>,
    /// Entities that had the state `UNAVAILABLE` in an `entity_change` event.
    unavailable: Vec<String>,
}

/// Settings of the main client: it does not reconnect, so a closed connection fails the check.
pub fn no_reconnect() -> WebSocketConfig {
    WebSocketConfig::builder()
        .max_reconnect_attempts(0u32)
        .build()
        .expect("valid WebSocket settings")
}

impl Client {
    pub async fn connect(port: u16) -> Result<Self, Fail> {
        Self::connect_with(port, no_reconnect()).await
    }

    /// Connect with the WebSocket client settings in `config`.
    pub async fn connect_with(port: u16, config: WebSocketConfig) -> Result<Self, Fail> {
        let (tx, inbound) = mpsc::unbounded_channel();
        let ws = GenericWebSocketClient::new(
            "remote-sim",
            format!("ws://127.0.0.1:{port}/"),
            config,
            Bridge(tx),
        )?
        .start();
        // Dropping the last address on an error stops the client.
        ws.send(Connect).await??;
        let mut client = Self {
            ws,
            inbound,
            closed: None,
            next_id: 0,
            attrs: HashMap::new(),
            setup: VecDeque::new(),
            assistant: VecDeque::new(),
            unavailable: Vec::new(),
        };
        // The driver sends the authentication answer at once.
        client
            .pump(Instant::now() + Duration::from_millis(300))
            .await?;
        Ok(client)
    }

    /// Read messages until `deadline`. Returns the first text message. Fails if the connection is
    /// closed.
    async fn pump(&mut self, deadline: Instant) -> Result<Option<Value>, Fail> {
        loop {
            if let Some(reason) = &self.closed {
                return Err(Fail::new(format!("connection closed: {reason:?}")));
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            let Ok(inbound) = timeout(wait, self.inbound.recv()).await else {
                return Ok(None);
            };
            match inbound {
                // The client actor stopped.
                None => self.closed = Some(DisconnectReason::ClosedLocally),
                Some(Inbound::Closed(reason)) => self.closed = Some(reason),
                Some(Inbound::Connected(_)) => {}
                Some(Inbound::Text(text)) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|e| Fail::new(format!("invalid JSON from the driver: {e}")))?;
                    self.record(&value);
                    return Ok(Some(value));
                }
            }
        }
    }

    /// Wait until the connection closes. Returns the reason, or `None` after `secs`. Other
    /// messages are read.
    pub async fn wait_closed(&mut self, secs: u64) -> Option<DisconnectReason> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while self.closed.is_none() && Instant::now() < deadline {
            let _ = self.pump(deadline).await;
        }
        self.closed.clone()
    }

    /// Wait for a new connection after a close. Only a client that reconnects gets one. Returns
    /// the time of the connect. Fails after `secs`.
    pub async fn wait_connected(&mut self, secs: u64) -> Result<Instant, Fail> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match timeout(wait, self.inbound.recv()).await {
                Ok(Some(Inbound::Connected(at))) => {
                    self.closed = None;
                    return Ok(at);
                }
                Ok(Some(Inbound::Closed(reason))) => self.closed = Some(reason),
                Ok(Some(Inbound::Text(text))) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|e| Fail::new(format!("invalid JSON from the driver: {e}")))?;
                    self.record(&value);
                }
                Ok(None) => return Err(Fail::new("the WebSocket client stopped")),
                Err(_) => {
                    return Err(Fail::new(format!(
                        "no new connection in {secs} s, last close: {:?}",
                        self.closed
                    )));
                }
            }
        }
    }

    /// True while the WebSocket client is connected.
    pub async fn is_connected(&self) -> bool {
        self.ws.send(IsConnected).await.unwrap_or(false)
    }

    /// Close the connection with status 1000. Waits until the driver has closed the connection, at
    /// most the close timeout of the settings (5 s by default).
    pub async fn close(&self) {
        let _ = self.ws.send(Disconnect).await;
    }

    fn record(&mut self, message: &Value) {
        if message["kind"] != "event" {
            return;
        }
        let data = &message["msg_data"];
        match message["msg"].as_str() {
            Some("entity_change") => {
                if let (Some(id), Some(attrs)) =
                    (data["entity_id"].as_str(), data["attributes"].as_object())
                {
                    if attrs.get("state").and_then(Value::as_str) == Some("UNAVAILABLE")
                        && !self.unavailable.iter().any(|u| u == id)
                    {
                        self.unavailable.push(id.to_owned());
                    }
                    self.attrs
                        .entry(id.to_owned())
                        .or_default()
                        .extend(attrs.clone());
                }
            }
            Some("driver_setup_change") => self.setup.push_back(data.clone()),
            Some("assistant_event") => self.assistant.push_back(data.clone()),
            _ => {}
        }
    }

    pub async fn send(&mut self, value: Value) -> Result<(), Fail> {
        self.ws.send(SendText::new(value.to_string())).await??;
        Ok(())
    }

    pub async fn send_binary(&mut self, data: Vec<u8>) -> Result<(), Fail> {
        self.ws.send(SendBinary::new(data)).await??;
        Ok(())
    }

    /// Forget the `assistant_event` events that were not read yet and the entities seen as
    /// `UNAVAILABLE`.
    pub fn clear_events(&mut self) {
        self.assistant.clear();
        self.unavailable.clear();
    }

    /// True if an `entity_change` event since [`Self::clear_events`] made `entity` unavailable.
    pub fn was_unavailable(&self, entity: &str) -> bool {
        self.unavailable.iter().any(|u| u == entity)
    }

    /// The next `assistant_event` data that was already read, without a wait.
    pub fn pop_assistant(&mut self) -> Option<Value> {
        self.assistant.pop_front()
    }

    /// The next `assistant_event` data, or `None` after `secs`.
    pub async fn next_assistant(&mut self, secs: u64) -> Result<Option<Value>, Fail> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(event) = self.assistant.pop_front() {
                return Ok(Some(event));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            self.pump(deadline).await?;
        }
    }

    /// Send a request and wait for the response.
    pub async fn request(&mut self, msg: &str, data: Value) -> Result<Value, Fail> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "kind": "req", "id": id, "msg": msg, "msg_data": data }))
            .await?;
        let deadline = Instant::now() + ANSWER_LIMIT;
        loop {
            match self.pump(deadline).await? {
                None => return Err(Fail::new(format!("no answer to {msg} in 15 s"))),
                Some(v) if v["kind"] == "resp" && v["req_id"] == id => return Ok(v),
                Some(_) => {}
            }
        }
    }

    /// A request that must answer with code 200. Returns `msg_data`.
    pub async fn request_ok(&mut self, msg: &str, data: Value) -> Result<Value, Fail> {
        let response = self.request(msg, data).await?;
        if response["code"] == 200 {
            Ok(response["msg_data"].clone())
        } else {
            Err(Fail::new(format!(
                "{msg}: code {}, {}",
                response["code"], response["msg_data"]
            )))
        }
    }

    pub async fn send_event(&mut self, msg: &str) -> Result<(), Fail> {
        self.send(json!({ "kind": "event", "msg": msg, "cat": "DEVICE" }))
            .await
    }

    /// Send an entity command. Returns the response and the time that the driver took.
    pub async fn command(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        cmd_id: &str,
        params: Value,
    ) -> Result<(Value, Duration), Fail> {
        let start = Instant::now();
        let response = self
            .request(
                "entity_command",
                json!({
                    "entity_type": entity_type,
                    "entity_id": entity_id,
                    "cmd_id": cmd_id,
                    "params": params,
                }),
            )
            .await?;
        Ok((response, start.elapsed()))
    }

    /// A command that must work and be answered in less than 10 s.
    pub async fn command_ok(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        cmd_id: &str,
        params: Value,
    ) -> Result<(), Fail> {
        let (response, took) = self.command(entity_type, entity_id, cmd_id, params).await?;
        if response["code"] != 200 {
            return Err(Fail::new(format!(
                "{cmd_id} on {entity_id}: code {}, {}",
                response["code"], response["msg_data"]
            )));
        }
        if took > Duration::from_secs(10) {
            return Err(Fail::new(format!("{cmd_id} took {} ms", took.as_millis())));
        }
        Ok(())
    }

    /// Read the states of all entities into the cache.
    pub async fn refresh_states(&mut self) -> Result<(), Fail> {
        let states = self.request_ok("get_entity_states", Value::Null).await?;
        for state in states.as_array().into_iter().flatten() {
            if let (Some(id), Some(attrs)) =
                (state["entity_id"].as_str(), state["attributes"].as_object())
            {
                self.attrs
                    .entry(id.to_owned())
                    .or_default()
                    .extend(attrs.clone());
            }
        }
        Ok(())
    }

    /// Forget the cached states, for example after a mode change.
    pub fn forget_states(&mut self) {
        self.attrs.clear();
    }

    pub fn attr(&self, entity: &str, attr: &str) -> Option<&Value> {
        self.attrs.get(entity)?.get(attr)
    }

    pub fn text(&self, entity: &str, attr: &str) -> String {
        match self.attr(entity, attr) {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }

    /// Wait until the attribute of an entity has a value that `accept` takes.
    pub async fn wait_attr(
        &mut self,
        entity: &str,
        attr: &str,
        secs: u64,
        accept: impl Fn(&Value) -> bool,
    ) -> Result<Value, Fail> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = self.attr(entity, attr).filter(|v| accept(v)) {
                return Ok(v.clone());
            }
            if Instant::now() >= deadline {
                return Err(Fail::new(format!(
                    "{entity} {attr}: no expected value in {secs} s, last: {}",
                    self.attr(entity, attr)
                        .map_or("none".into(), Value::to_string)
                )));
            }
            self.pump(deadline.min(Instant::now() + Duration::from_millis(200)))
                .await?;
        }
    }

    pub async fn wait_text(
        &mut self,
        entity: &str,
        attr: &str,
        secs: u64,
        expected: &str,
    ) -> Result<(), Fail> {
        self.wait_attr(entity, attr, secs, |v| v.as_str() == Some(expected))
            .await
            .map(|_| ())
    }

    /// Read events for `secs`.
    pub async fn listen(&mut self, secs: u64) -> Result<(), Fail> {
        self.listen_ms(secs * 1000).await
    }

    pub async fn listen_ms(&mut self, ms: u64) -> Result<(), Fail> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            self.pump(deadline).await?;
        }
        Ok(())
    }

    /// The next `driver_setup_change` event.
    pub async fn next_setup(&mut self, secs: u64) -> Result<Value, Fail> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(event) = self.setup.pop_front() {
                return Ok(event);
            }
            if Instant::now() >= deadline {
                return Err(Fail::new(format!("no setup event in {secs} s")));
            }
            self.pump(deadline).await?;
        }
    }

    /// The next setup page. Fails if the setup ends.
    pub async fn next_page(&mut self, secs: u64) -> Result<Value, Fail> {
        loop {
            let event = self.next_setup(secs).await?;
            if let Some(input) = event["require_user_action"]["input"].as_object() {
                return Ok(Value::Object(input.clone()));
            }
            if is_stop(&event) {
                return Err(Fail::new(format!("setup ended: {event}")));
            }
        }
    }

    /// The end of the setup. Fails if the state is not `OK`.
    pub async fn setup_done(&mut self, secs: u64) -> Result<(), Fail> {
        loop {
            let event = self.next_setup(secs).await?;
            if is_stop(&event) {
                return if event["state"] == "OK" {
                    Ok(())
                } else {
                    Err(Fail::new(format!("setup failed: {event}")))
                };
            }
        }
    }

    /// Answer the current setup page.
    pub async fn answer(&mut self, values: Value) -> Result<(), Fail> {
        self.request_ok("set_driver_user_data", json!({ "input_values": values }))
            .await
            .map(|_| ())
    }
}

fn is_stop(event: &Value) -> bool {
    event["event_type"]
        .as_str()
        .is_some_and(|t| t.eq_ignore_ascii_case("stop"))
}

/// The ids of the fields of a setup page.
pub fn field_ids(page: &Value) -> Vec<String> {
    page["settings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect()
}

/// The value of a field of a setup page.
pub fn field_value(page: &Value, id: &str) -> Option<String> {
    let setting = page["settings"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == id)?;
    let field = setting["field"].as_object()?.values().next()?;
    match &field["value"] {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}
