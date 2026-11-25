// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Apple TV devices of the controller: entity states, subscriptions and commands.

use crate::config_store::DeviceConfig;
use crate::controller::{Controller, R2AudioChunkMsg, R2RequestMsg, SendWsMessage};
use crate::device::{
    AppleTvActor, AudioChunk, Browse, Command, Connect, DeviceCache, DeviceEvent, DeviceStatus,
    Disconnect, Reply, Stop, UpdateConfig,
};
use crate::entities::{self, EntityKind, media_player};
use crate::errors::ServiceError;
use crate::util::{DeserializeMsgData, return_fut_err};
use actix::{Actor, Addr, AsyncContext, Context, Handler, Message, ResponseFuture};
use bytestring::ByteString;
use log::{debug, info, warn};
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use uc_api::EntityType;
use uc_api::intg::ws::BrowseMediaMsgData;
use uc_api::intg::{AssistantEvent, AvailableIntgEntity, DeviceState, EntityCommand};
use uc_api::ws::{EventCategory, WsMessage, WsResultMsgData};

/// Attribute that changes with each position update.
const POSITION_UPDATED_AT: &str = "media_position_updated_at";

/// `entity_change` data. Borrows the attributes, so that they are not copied to serialize them.
#[derive(Serialize)]
pub(super) struct EntityChangeRef<'a, A = &'a Map<String, Value>> {
    entity_type: EntityType,
    entity_id: &'a str,
    attributes: A,
}

/// The attributes of `keys`, read from the cache.
struct Picked<'a> {
    cache: &'a Map<String, Value>,
    keys: &'a [String],
}

impl Serialize for Picked<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.keys.len()))?;
        for key in self.keys {
            if let Some(value) = self.cache.get(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// Event, the same as [`WsMessage::event`] gives. Borrows the data, so that it is not copied to
/// serialize it.
#[derive(Serialize)]
struct EventRef<'a, T> {
    kind: &'static str,
    msg: &'a str,
    cat: EventCategory,
    ts: jiff::Timestamp,
    msg_data: T,
}

/// Serialize an event once, to send it to each Remote.
fn event_text<T: Serialize>(msg: &str, cat: EventCategory, data: T) -> Option<ByteString> {
    let event = EventRef {
        kind: "event",
        msg,
        cat,
        ts: jiff::Timestamp::now(),
        msg_data: data,
    };
    match serde_json::to_string(&event) {
        Ok(text) => Some(text.into()),
        Err(e) => {
            warn!("{msg} serialize error: {e}");
            None
        }
    }
}

/// Serialize an `entity_change` event once, to send it to each Remote.
fn entity_change_text<T: Serialize>(data: T) -> Option<ByteString> {
    event_text("entity_change", EventCategory::Entity, data)
}

/// A voice command failed: forget the voice session.
#[derive(Message)]
#[rtype(result = "()")]
struct EndVoiceSession(u32);

/// An active voice session.
pub(super) struct VoiceSession {
    /// Device key.
    pub key: Arc<str>,
    /// The Remote that started it. It gets the assistant events.
    pub ws_id: Arc<str>,
}

/// One configured Apple TV.
pub(super) struct Device {
    pub cfg: DeviceConfig,
    pub addr: Addr<AppleTvActor>,
    pub status: DeviceStatus,
    /// An awake Remote uses the device. Set by [`Controller::update_connections`].
    pub wanted: bool,
    /// Number of [`UpdateConfig`] messages sent to the actor.
    pub cfg_version: u64,
}

impl Device {
    /// Use `cfg` and send it to the actor.
    pub fn update_config(&mut self, cfg: DeviceConfig) {
        self.cfg = cfg;
        self.send_config();
    }

    /// Send `cfg` to the actor.
    fn send_config(&mut self) {
        self.cfg_version += 1;
        self.addr.do_send(UpdateConfig(self.cfg.clone()));
    }
}

/// What made [`Controller::update_connections`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    /// A Remote woke up: connect each used device, also when it was used before. This also
    /// tries a device with a pairing error again.
    Wake,
    /// A Remote went to sleep or left: disconnect each device that is not used.
    Sleep,
    /// Subscriptions or the setup changed: send only where the need changed. A connect that a
    /// command started for a device that is not used is not stopped.
    Change,
}

/// The message to send to a device: `Some(true)` for Connect, `Some(false)` for Disconnect.
const fn connection_message(want: bool, was_wanted: bool, trigger: Trigger) -> Option<bool> {
    let changed = want != was_wanted;
    match (want, trigger) {
        (true, Trigger::Wake) | (false, Trigger::Sleep) => Some(want),
        _ if changed => Some(want),
        _ => None,
    }
}

impl Controller {
    /// Start the actor of a device. `cache` holds the saved lists, or empty lists.
    pub(super) fn add_device(
        &mut self,
        cfg: DeviceConfig,
        cache: DeviceCache,
        ctx: &Context<Self>,
    ) {
        let key: Arc<str> = cfg.key.as_str().into();
        let addr = AppleTvActor::new(
            key.clone(),
            cfg.clone(),
            cache,
            self.device_ctx.clone(),
            ctx.address().recipient(),
        )
        .start();
        // Remove first: `insert` keeps the old key, and events are matched by the key instance.
        // The new actor uses the same cache file.
        if let Some(old) = self.devices.remove(&*key) {
            old.addr.do_send(Stop {
                remove_cache: false,
            });
        }
        self.devices.insert(
            key,
            Device {
                cfg,
                addr,
                status: DeviceStatus::Disconnected,
                wanted: false,
                cfg_version: 0,
            },
        );
    }

    /// True if the key belongs to the running actor of the device.
    fn is_current(&self, key: &Arc<str>) -> bool {
        self.devices
            .get_key_value(&**key)
            .is_some_and(|(k, _)| Arc::ptr_eq(k, key))
    }

    /// Stop the actor of a device and forget its entities. The actor deletes the cache file when
    /// it stops, so the delete comes after its last save. If the driver stops before the actor
    /// handles `Stop`, the cache file stays. It is overwritten when the same device is configured
    /// again.
    pub(super) fn remove_device(&mut self, key: &str) -> Option<DeviceConfig> {
        let device = self.devices.remove(key)?;
        device.addr.do_send(Stop { remove_cache: true });
        self.entities
            .retain(|id, _| EntityKind::parse(id).is_none_or(|(k, _)| k != key));
        // A voice session stays until the stopping actor sends its last event.
        // The removed device might have had an error.
        self.update_device_state();
        Some(device.cfg)
    }

    /// The configuration of all devices, sorted by name and key.
    pub(super) fn sorted_configs(&self) -> Vec<DeviceConfig> {
        let mut devices: Vec<DeviceConfig> = self.devices.values().map(|d| d.cfg.clone()).collect();
        sort_configs(&mut devices);
        devices
    }

    /// Queue the save of the configuration of all devices. A write error is only logged.
    pub(super) fn queue_devices(&self) -> Result<(), ServiceError> {
        self.store.queue_devices(&self.sorted_configs())?;
        Ok(())
    }

    /// The entity definitions, only of `entity_type` if it is given, in the order of
    /// [`Self::send_order`].
    pub(super) fn available_entities(
        &self,
        entity_type: Option<EntityType>,
    ) -> Vec<AvailableIntgEntity> {
        let mut list: Vec<AvailableIntgEntity> = self
            .devices
            .values()
            .flat_map(|d| entities::definitions(&d.cfg))
            .filter(|e| entity_type.is_none_or(|t| e.entity_type == t))
            .collect();
        list.sort_unstable_by(|a, b| {
            self.send_order(a.entity_type, &a.entity_id)
                .cmp(&self.send_order(b.entity_type, &b.entity_id))
        });
        list
    }

    /// States of the subscribed entities, or of all entities without subscriptions.
    /// Same order as [`Self::available_entities`].
    pub(super) fn entity_states(&self, ws_id: &str) -> Vec<EntityChangeRef<'_>> {
        let session = self.sessions.get(ws_id);
        let mut states: Vec<EntityChangeRef<'_>> = self
            .entities
            .iter()
            .filter(|(id, _)| {
                session.is_none_or(|s| s.subscribed_entities.is_empty() || s.wants(id))
            })
            .filter_map(|(id, attrs)| entity_change(id, attrs))
            .collect();
        states.sort_unstable_by(|a, b| {
            self.send_order(a.entity_type, a.entity_id)
                .cmp(&self.send_order(b.entity_type, b.entity_id))
        });
        states
    }

    /// The Remote reads the entity states now. Entities in the answer are not stale any more.
    /// Entities left out of the answer are marked stale, so they are sent when subscribed.
    pub(super) fn mark_states_read(&mut self, ws_id: &str) {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        session.states_read = true;
        if session.subscribe_all || session.subscribed_entities.is_empty() {
            session.stale.clear();
            return;
        }
        session
            .stale
            .retain(|id| !session.subscribed_entities.contains(id));
        for id in self.entities.keys() {
            if !session.subscribed_entities.contains(id) && !session.stale.contains(id) {
                session.stale.insert(id.clone());
            }
        }
    }

    /// Sort key for sending entities: media players first, then remotes, then the rest.
    /// Within each group: device name, device key, entity id.
    fn send_order<'a>(&'a self, ty: EntityType, id: &'a str) -> (u8, &'a str, &'a str, &'a str) {
        let key = EntityKind::parse(id).map_or("", |(k, _)| k);
        let name = self.devices.get(key).map_or("", |d| d.cfg.name.as_str());
        (send_rank(ty), name, key, id)
    }

    /// Subscribe entities. `None` or no ids means all entities.
    ///
    /// Sends only the entities that changed after the Remote read the states. The Remote reads
    /// the states itself, and one event per entity can fill the mailbox of the Remote core.
    pub(super) fn subscribe(&mut self, ws_id: &str, entity_ids: Option<Vec<String>>) {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        let entity_ids = entity_ids.unwrap_or_default();
        let all = entity_ids.is_empty();
        let stale: Vec<String> = if all {
            session.subscribe_all = true;
            session.stale.drain().collect()
        } else {
            let mut stale = Vec::new();
            for id in entity_ids {
                if let Some(changed) = session.stale.take(&id) {
                    stale.push(changed);
                }
                session.subscribed_entities.insert(id);
            }
            stale
        };
        self.send_entities(ws_id, &stale);
        self.update_connections(Trigger::Change);
    }

    /// Send the subscribed entities that changed while the Remote was in standby. Each entity is
    /// sent once, with its current state. Sending all entities at each wake-up can fill the
    /// mailbox of the Remote core.
    ///
    /// Subscribed entities are stale only after a standby, so call it after
    /// [`Controller::wake`] returned true or after a wake-up event.
    pub(super) fn send_missed_states(&mut self, ws_id: &str) {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        if session.standby || session.stale.is_empty() {
            return;
        }
        // Other stale entities stay for a later subscription.
        let (all, subscribed) = (session.subscribe_all, &session.subscribed_entities);
        let missed: Vec<String> = session
            .stale
            .extract_if(|id| all || subscribed.contains(id))
            .collect();
        self.send_entities(ws_id, &missed);
    }

    /// Send the current state of each entity in `ids`, in the order of [`Self::send_order`].
    fn send_entities(&self, ws_id: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut changes: Vec<EntityChangeRef<'_>> = ids
            .iter()
            .filter_map(|id| self.entities.get_key_value(id))
            .filter_map(|(id, attrs)| entity_change(id, attrs))
            .collect();
        changes.sort_unstable_by(|a, b| {
            self.send_order(a.entity_type, a.entity_id)
                .cmp(&self.send_order(b.entity_type, b.entity_id))
        });
        for change in &changes {
            self.send_entity_change(ws_id, change);
        }
    }

    /// Unsubscribe entities. `None` means all entities, no ids means none.
    pub(super) fn unsubscribe(&mut self, ws_id: &str, entity_ids: Option<&[String]>) {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        match entity_ids {
            None => {
                session.subscribe_all = false;
                session.subscribed_entities.clear();
            }
            Some([]) => return,
            Some(entity_ids) => {
                if session.subscribe_all {
                    // "All" becomes the list of all configured entities, so single ids can be
                    // removed. Also entities without a state yet.
                    session.subscribe_all = false;
                    for key in self.devices.keys() {
                        session
                            .subscribed_entities
                            .extend(EntityKind::all().map(|kind| kind.entity_id(key)));
                    }
                }
                for id in entity_ids {
                    session.subscribed_entities.remove(id);
                }
            }
        }
        self.update_connections(Trigger::Change);
    }

    /// Store in [`Device::wanted`] which devices an awake Remote subscribed to, and send Connect
    /// or Disconnect as `trigger` says, see [`connection_message`].
    /// Call it after each change that can change which devices an awake Remote uses.
    pub(super) fn update_connections(&mut self, trigger: Trigger) {
        for (key, device) in &mut self.devices {
            let want = self.sessions.values().any(|s| s.awake() && s.uses(key));
            let message = connection_message(want, device.wanted, trigger);
            device.wanted = want;
            match message {
                Some(true) => device.addr.do_send(Connect),
                Some(false) => {
                    if matches!(
                        device.status,
                        DeviceStatus::Connecting | DeviceStatus::Connected
                    ) {
                        debug!(
                            "[{}] not used by an awake Remote, disconnecting",
                            device.cfg.name
                        );
                    }
                    device.addr.do_send(Disconnect);
                }
                None => {}
            }
        }
    }

    pub(super) fn entity_command(
        &mut self,
        msg: R2RequestMsg,
        _resp_msg: &'static str,
        ctx: &Context<Self>,
    ) -> ResponseFuture<Result<Option<WsMessage>, ServiceError>> {
        let req_id = msg.req_id;
        let ws_id = Arc::clone(&msg.ws_id);
        let command: EntityCommand = match msg.deserialize() {
            Ok(c) => c,
            Err(e) => return_fut_err!(e.into()),
        };
        let addr = match self.device_for(&command.entity_id) {
            Ok(v) => v,
            Err(e) => return_fut_err!(e),
        };
        // The Remote subscribes the entities it uses. The command still runs: the device
        // connects for it.
        if let Some((key, _)) = EntityKind::parse(&command.entity_id)
            && !self.sessions.values().any(|s| s.uses(key))
        {
            warn!(
                "command for {}, but no Remote subscribed an entity of this device",
                command.entity_id
            );
        }
        let (addr, kind) = addr;
        if kind.entity_type() != command.entity_type {
            return_fut_err!(ServiceError::BadRequest(format!(
                "Invalid entity type for {}",
                command.entity_id
            )));
        }
        let voice_session = self.voice_session(&command, &ws_id);
        let controller = ctx.address();
        // The device answers a voice command through the controller, so the answer comes
        // before the assistant events of the session.
        let reply = (kind == EntityKind::VoiceAssistant).then_some(Reply { ws_id, req_id });
        let answered = reply.is_some();
        // Queued now, so a `voice_start` reaches the device before the audio that follows it.
        // `addr` is a new clone, and a new address never waits: `send` queues the command at
        // once, also when the mailbox is full.
        let request = addr.send(Command {
            kind,
            cmd_id: command.cmd_id,
            params: command.params,
            reply,
        });
        Box::pin(async move {
            let result = request
                .await
                .unwrap_or_else(|e| Err(ServiceError::from_device_mailbox(e)));
            if let Err(e) = result {
                if let Some(session_id) = voice_session {
                    controller.do_send(EndVoiceSession(session_id));
                }
                return Err(e);
            }
            if answered {
                return Ok(None);
            }
            // The device answers before the command runs. Errors after this reply are only
            // logged.
            Ok(Some(command_sent(req_id)))
        })
    }

    pub(super) fn browse_media(
        &self,
        msg: R2RequestMsg,
        resp_msg: &'static str,
    ) -> ResponseFuture<Result<Option<WsMessage>, ServiceError>> {
        let req_id = msg.req_id;
        let data: BrowseMediaMsgData = match msg.deserialize() {
            Ok(d) => d,
            Err(e) => return_fut_err!(e.into()),
        };
        let (addr, kind) = match self.device_for(&data.entity_id) {
            Ok(v) => v,
            Err(e) => return_fut_err!(e),
        };
        if kind != EntityKind::MediaPlayer {
            return_fut_err!(ServiceError::BadRequest(
                "Only media players can be browsed".into()
            ));
        }
        Box::pin(async move {
            let result = addr
                .send(Browse {
                    media_id: data.media_id,
                    paging: data.paging.unwrap_or_default(),
                })
                .await
                .map_err(ServiceError::from_device_mailbox)??;
            Ok(Some(WsMessage::response(req_id, resp_msg, result)))
        })
    }

    fn device_for(
        &self,
        entity_id: &str,
    ) -> Result<(Addr<AppleTvActor>, EntityKind), ServiceError> {
        let (key, kind) = EntityKind::parse(entity_id)
            .ok_or_else(|| ServiceError::NotFound(format!("Unknown entity: {entity_id}")))?;
        let device = self
            .devices
            .get(key)
            .ok_or_else(|| ServiceError::NotFound(format!("Unknown entity: {entity_id}")))?;
        Ok((device.addr.clone(), kind))
    }

    fn send_entity_change(&self, ws_id: &str, change: &EntityChangeRef<'_>) {
        if let Some(text) = entity_change_text(change) {
            self.send_r2_msg(ws_id, SendWsMessage::Text(text));
        }
    }

    /// Store changed attributes and send them to the subscribed Remotes.
    fn apply_changes(&mut self, key: &str, updates: Vec<(EntityKind, Map<String, Value>)>) {
        // Late event of a removed device.
        if !self.devices.contains_key(key) {
            return;
        }
        let mut entity_id = String::new();
        for (kind, attrs) in updates {
            entity_id.clear();
            kind.write_entity_id(key, &mut entity_id);
            let (mut send, mut tracked) = (false, false);
            for s in self.sessions.values() {
                let wants = s.wants(&entity_id);
                // A Remote in standby gets the entity when it wakes up, if it read the states.
                send |= !s.standby && wants;
                tracked |= s.keeps(wants);
            }
            // The attribute names are only needed to send them.
            let (any, changed) = if let Some(cache) = self.entities.get_mut(&entity_id) {
                diff(cache, attrs, send)
            } else {
                let mut attrs = attrs;
                stamp_first_position(&mut attrs);
                let changed = if send {
                    attrs.keys().cloned().collect()
                } else {
                    Vec::new()
                };
                let any = !attrs.is_empty();
                self.entities.insert(entity_id.clone(), attrs);
                (any, changed)
            };
            if !any {
                continue;
            }
            if tracked {
                for session in self.sessions.values_mut() {
                    if session.tracks(&entity_id) && !session.stale.contains(&entity_id) {
                        session.stale.insert(entity_id.clone());
                    }
                }
            }
            if send {
                self.send_changes(kind, &entity_id, &changed);
            }
        }
    }

    /// Send `entity_change` with the attributes of `keys` to the awake Remotes that subscribed the
    /// entity. The message is serialized once.
    fn send_changes(&self, kind: EntityKind, entity_id: &str, keys: &[String]) {
        let mut targets = self
            .sessions
            .iter()
            .filter(|(_, s)| !s.standby && s.wants(entity_id))
            .peekable();
        let Some(cache) = self.entities.get(entity_id) else {
            return;
        };
        if targets.peek().is_none() {
            return;
        }
        let Some(text) = entity_change_text(EntityChangeRef {
            entity_type: kind.entity_type(),
            entity_id,
            attributes: Picked { cache, keys },
        }) else {
            return;
        };
        for (ws_id, session) in targets {
            Self::send_to_session(ws_id, session, SendWsMessage::Text(text.clone()));
        }
    }

    /// Driver state: error while one device has invalid credentials.
    fn update_device_state(&mut self) {
        let state = if self
            .devices
            .values()
            .any(|d| d.status == DeviceStatus::AuthError)
        {
            DeviceState::Error
        } else {
            DeviceState::Connected
        };
        if state != self.device_state {
            self.device_state = state;
            for ws_id in self.sessions.keys() {
                self.send_device_state(ws_id);
            }
        }
    }
}

/// Merge new attributes into the cache. Returns whether anything changed, and the names of the
/// attributes that changed if `collect` is set.
fn diff(
    cache: &mut Map<String, Value>,
    attrs: Map<String, Value>,
    collect: bool,
) -> (bool, Vec<String>) {
    let mut any = false;
    let mut changed = Vec::new();
    let mut updated_at = false;
    let mut new_position = false;
    for (name, value) in attrs {
        if name == POSITION_UPDATED_AT {
            updated_at = true;
            continue;
        }
        if cache.get(&name) != Some(&value) {
            // The Remote counts on from the timestamp while playing, so a resume needs a new one.
            new_position |= matches!(name.as_str(), "media_position" | "media_duration" | "state");
            any = true;
            if collect {
                changed.push(name.clone());
            }
            cache.insert(name, value);
        }
    }
    // The timestamp is only sent with a new position or state.
    if updated_at && new_position {
        if collect {
            changed.push(POSITION_UPDATED_AT.into());
        }
        cache.insert(
            POSITION_UPDATED_AT.into(),
            media_player::position_timestamp().into(),
        );
    }
    (any, changed)
}

/// The first attributes of an entity: a position gets its time, so that the Remote counts on
/// from the start.
fn stamp_first_position(attrs: &mut Map<String, Value>) {
    if attrs.contains_key(POSITION_UPDATED_AT) && attrs.contains_key("media_position") {
        attrs.insert(
            POSITION_UPDATED_AT.into(),
            media_player::position_timestamp().into(),
        );
    }
}

/// The entity belongs to the device with this key.
pub(super) fn is_of_device(entity_id: &str, key: &str) -> bool {
    EntityKind::parse(entity_id).is_some_and(|(k, _)| k == key)
}

/// Send order of an entity type: media player, remote, then the rest.
const fn send_rank(ty: EntityType) -> u8 {
    match ty {
        EntityType::MediaPlayer => 0,
        EntityType::Remote => 1,
        _ => 2,
    }
}

fn entity_change<'a>(
    entity_id: &'a str,
    attributes: &'a Map<String, Value>,
) -> Option<EntityChangeRef<'a>> {
    let (_, kind) = EntityKind::parse(entity_id)?;
    Some(EntityChangeRef {
        entity_type: kind.entity_type(),
        entity_id,
        attributes,
    })
}

/// Answer to an entity command that started.
fn command_sent(req_id: u32) -> WsMessage {
    WsMessage::response(req_id, "result", WsResultMsgData::new("OK", "Command sent"))
}

impl Handler<DeviceEvent> for Controller {
    type Result = ();

    fn handle(&mut self, msg: DeviceEvent, _: &mut Self::Context) {
        if let DeviceEvent::Changes { key, .. }
        | DeviceEvent::Config { key, .. }
        | DeviceEvent::Status { key, .. } = &msg
            && !self.is_current(key)
        {
            // From a removed or replaced actor.
            return;
        }
        match msg {
            DeviceEvent::Changes { key, changes } => self.apply_changes(&key, changes),
            DeviceEvent::Config { key, cfg, version } => {
                // An actor that did not handle the last `UpdateConfig` yet has old values.
                let Some(device) = self
                    .devices
                    .get_mut(&*key)
                    .filter(|d| d.cfg_version == version)
                else {
                    return;
                };
                // Only take the values the device finds on the network.
                let own = &mut device.cfg;
                let address = own.address != cfg.address;
                if own.name == cfg.name && own.known_ids == cfg.known_ids && !address {
                    return;
                }
                info!("[{}] saving changed configuration", cfg.name);
                own.name = cfg.name;
                own.known_ids = cfg.known_ids;
                if address {
                    own.address = cfg.address;
                }
                // The device actors share this thread: do not wait for the disk.
                if let Err(e) = self.queue_devices() {
                    warn!("Saving the configuration failed: {e}");
                }
            }
            DeviceEvent::Status { key, status } => {
                if let Some(device) = self.devices.get_mut(&*key) {
                    device.status = status;
                    self.update_device_state();
                }
            }
            DeviceEvent::Accepted(Reply { ws_id, req_id }) => {
                self.send_r2_response(&ws_id, SendWsMessage::Msg(command_sent(req_id)));
            }
            DeviceEvent::Assistant(event) => {
                let session_id = assistant_session_id(&event);
                let last = matches!(
                    event,
                    AssistantEvent::Finished { .. } | AssistantEvent::Error { .. }
                );
                let ws_id = if last {
                    self.voice_sessions.remove(&session_id).map(|v| v.ws_id)
                } else {
                    self.voice_sessions
                        .get(&session_id)
                        .map(|v| v.ws_id.clone())
                };
                let session = ws_id.as_ref().and_then(|w| self.sessions.get_key_value(w));
                let Some((ws_id, session)) = session.filter(|(_, s)| !s.standby) else {
                    debug!(
                        "assistant_event of voice session {session_id} not sent: no voice session or no awake Remote"
                    );
                    return;
                };
                if let Some(text) = event_text("assistant_event", EventCategory::Entity, &event) {
                    Self::send_to_session(ws_id, session, SendWsMessage::Text(text));
                }
            }
        }
    }
}

impl Handler<R2AudioChunkMsg> for Controller {
    type Result = ();

    fn handle(&mut self, msg: R2AudioChunkMsg, _: &mut Self::Context) {
        let device = self
            .voice_sessions
            .get(&msg.session_id)
            .and_then(|v| self.devices.get(&v.key));
        let Some(device) = device else {
            // An end marker can come again after the session has ended.
            if msg.data.is_empty() {
                debug!("End of unknown voice session {}", msg.session_id);
            } else {
                warn!("Audio for unknown voice session {}", msg.session_id);
            }
            return;
        };
        device.addr.do_send(AudioChunk {
            session_id: msg.session_id,
            data: msg.data,
        });
    }
}

impl Handler<EndVoiceSession> for Controller {
    type Result = ();

    fn handle(&mut self, msg: EndVoiceSession, _: &mut Self::Context) {
        self.voice_sessions.remove(&msg.0);
    }
}

impl Controller {
    /// Remember the device of a voice session, for the audio chunks, and the Remote that
    /// started it. Returns the session id.
    pub(super) fn voice_session(
        &mut self,
        command: &EntityCommand,
        ws_id: &Arc<str>,
    ) -> Option<u32> {
        if command.entity_type != EntityType::VoiceAssistant {
            return None;
        }
        let session_id = command
            .params
            .as_ref()
            .and_then(|p| p.get("session_id"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        if let (Some(session_id), Some((key, _))) =
            (session_id, EntityKind::parse(&command.entity_id))
            && let Some((key, _)) = self.devices.get_key_value(key)
        {
            self.voice_sessions.insert(
                session_id,
                VoiceSession {
                    key: key.clone(),
                    ws_id: ws_id.clone(),
                },
            );
            return Some(session_id);
        }
        None
    }
}

/// Sort device configurations by name and key, the order of `config.json`.
pub(super) fn sort_configs(devices: &mut [DeviceConfig]) {
    devices.sort_unstable_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
}

/// Session id of an assistant event.
const fn assistant_session_id(event: &AssistantEvent) -> u32 {
    match event {
        AssistantEvent::Ready { session_id, .. }
        | AssistantEvent::SttResponse { session_id, .. }
        | AssistantEvent::TextResponse { session_id, .. }
        | AssistantEvent::SpeechResponse { session_id, .. }
        | AssistantEvent::Finished { session_id, .. }
        | AssistantEvent::Error { session_id, .. } => *session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn connection_message_rules() {
        use Trigger::{Change, Sleep, Wake};
        // (want, was_wanted, trigger, message)
        let cases = [
            (true, false, Change, Some(true)),
            (false, true, Change, Some(false)),
            // No change: a connect started by a command is not stopped.
            (false, false, Change, None),
            (true, true, Change, None),
            // Wake connects again, for example after a pairing error.
            (true, true, Wake, Some(true)),
            (true, false, Wake, Some(true)),
            (false, false, Wake, None),
            (false, true, Wake, Some(false)),
            // Sleep disconnects also a device that a command connected.
            (false, false, Sleep, Some(false)),
            (false, true, Sleep, Some(false)),
            (true, true, Sleep, None),
            (true, false, Sleep, Some(true)),
        ];
        for (want, was, trigger, expected) in cases {
            assert_eq!(
                connection_message(want, was, trigger),
                expected,
                "{want} {was} {trigger:?}"
            );
        }
    }

    #[test]
    fn diff_keeps_only_changes() {
        let mut cache = map(json!({ "state": "ON", "media_position": 1 }));
        let (_, changed) = diff(
            &mut cache,
            map(json!({ "state": "ON", "media_title": "Song" })),
            true,
        );
        assert_eq!(vec!["media_title"], changed);
        assert_eq!(Some(&json!("Song")), cache.get("media_title"));
    }

    fn keys(m: &Map<String, Value>) -> Vec<&str> {
        let mut k: Vec<&str> = m.keys().map(String::as_str).collect();
        k.sort_unstable();
        k
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort_unstable();
        v
    }

    #[test]
    fn position_timestamp_only_with_new_position() {
        let mut cache = map(json!({ "media_position": 1 }));
        let (any, changed) = diff(
            &mut cache,
            map(json!({ "media_position": 1, POSITION_UPDATED_AT: null })),
            true,
        );
        assert!(!any, "only a timestamp is no change");
        assert_eq!(Vec::<String>::new(), changed);
        let (_, changed) = diff(
            &mut cache,
            map(json!({ "media_position": 2, POSITION_UPDATED_AT: null })),
            true,
        );
        assert_eq!(vec!["media_position", POSITION_UPDATED_AT], sorted(changed));
        assert!(cache[POSITION_UPDATED_AT].is_string());
    }

    #[test]
    fn position_timestamp_with_new_state() {
        let mut cache = map(json!({ "state": "PAUSED", "media_position": 130 }));
        let (_, changed) = diff(
            &mut cache,
            map(json!({ "state": "PLAYING", "media_position": 130, POSITION_UPDATED_AT: null })),
            true,
        );
        assert_eq!(vec![POSITION_UPDATED_AT, "state"], sorted(changed));
    }

    #[test]
    fn first_position_gets_timestamp() {
        let mut attrs = map(json!({ "media_position": 5, POSITION_UPDATED_AT: null }));
        stamp_first_position(&mut attrs);
        assert!(attrs[POSITION_UPDATED_AT].is_string());

        let mut attrs = map(json!({ "state": "ON" }));
        stamp_first_position(&mut attrs);
        assert!(!attrs.contains_key(POSITION_UPDATED_AT));
    }

    #[test]
    fn diff_without_collect_updates_cache_only() {
        let mut cache = map(json!({ "media_position": 1 }));
        let (any, changed) = diff(
            &mut cache,
            map(json!({ "media_position": 2, POSITION_UPDATED_AT: null })),
            false,
        );
        assert!(any, "a change without names");
        assert_eq!(Vec::<String>::new(), changed);
        assert_eq!(vec!["media_position", POSITION_UPDATED_AT], keys(&cache));
        assert!(cache[POSITION_UPDATED_AT].is_string());
    }

    #[test]
    fn response_text_matches_ws_message() {
        let a = map(json!({ "state": "ON", "volume": null }));
        let b = map(json!({ "state": "PLAYING" }));
        let states: Vec<EntityChangeRef<'_>> = [("media_player.x", &a), ("remote.x", &b)]
            .into_iter()
            .filter_map(|(id, attrs)| entity_change(id, attrs))
            .collect();
        let text = crate::controller::response_text(3, "entity_states", &states).expect("text");
        let own: Value = serde_json::from_str(&text).expect("json");
        let expected =
            serde_json::to_value(WsMessage::response(3, "entity_states", &states)).expect("json");
        assert_eq!(expected, own);
        assert_eq!(2, own["msg_data"].as_array().map_or(0, Vec::len));
    }

    #[test]
    fn event_text_matches_ws_message() {
        let event = AssistantEvent::Finished {
            entity_id: "voice_assistant.x".into(),
            session_id: 4,
        };
        let text = event_text("assistant_event", EventCategory::Entity, &event).expect("text");
        let mut own: Value = serde_json::from_str(&text).expect("json");
        let data = serde_json::to_value(&event).expect("json");
        let mut expected = serde_json::to_value(WsMessage::event(
            "assistant_event",
            EventCategory::Entity,
            data,
        ))
        .expect("json");
        assert!(own["ts"].is_string());
        own.as_object_mut().expect("object").remove("ts");
        expected.as_object_mut().expect("object").remove("ts");
        assert_eq!(expected, own);
    }

    /// A new position: build the attributes, merge them into the cache and build the event text.
    #[cfg(feature = "dhat-heap")]
    #[ignore = "counts the allocations of all threads, run alone"]
    #[test]
    fn dhat_position_event() {
        use crate::device::state::{Dirty, Snapshot};
        use crate::entities::{EntityKind, attributes};
        use ratv::mrp::PlaybackState;

        let mut s = Snapshot {
            power: crate::device::state::Power::On,
            state: Some(PlaybackState::Playing),
            elapsed: Some(10.0),
            elapsed_timestamp: Some(100.0),
            rate: Some(1.0),
            duration: Some(200.0),
            ..Snapshot::default()
        };
        let event = |s: &Snapshot, cache: &mut Map<String, Value>| {
            let mut attrs = Map::new();
            attributes(
                s,
                EntityKind::MediaPlayer,
                Dirty::POSITION,
                100.0,
                &mut attrs,
            );
            let (any, keys) = diff(cache, attrs, true);
            assert!(any);
            entity_change_text(EntityChangeRef {
                entity_type: EntityType::MediaPlayer,
                entity_id: "media_player.x",
                attributes: Picked { cache, keys: &keys },
            })
            .expect("text")
        };
        let mut cache = Map::new();
        event(&s, &mut cache);
        s.elapsed = Some(20.0);
        let (text, blocks) = crate::util::alloc_count::blocks(|| event(&s, &mut cache));
        assert!(text.contains("\"media_position\":20"), "{text}");
        assert!(text.contains("media_position_updated_at"), "{text}");
        assert!(
            !text.contains("media_duration"),
            "only changed attributes: {text}"
        );
        // Attribute names and the map, the changed names, the timestamp and the text. The text
        // can grow when its timestamp is long: 12 is the most.
        assert!(blocks <= 12, "{blocks} allocations");
    }

    #[test]
    fn entity_change_text_has_only_the_keys() {
        let cache = map(json!({ "state": "ON", "media_title": "Song" }));
        let keys = ["media_title".to_string()];
        let text = entity_change_text(EntityChangeRef {
            entity_type: EntityType::MediaPlayer,
            entity_id: "media_player.x",
            attributes: Picked {
                cache: &cache,
                keys: &keys,
            },
        })
        .expect("text");
        let v: Value = serde_json::from_str(&text).expect("json");
        assert_eq!("event", v["kind"]);
        assert_eq!("entity_change", v["msg"]);
        assert_eq!("ENTITY", v["cat"]);
        assert!(v["ts"].is_string());
        assert_eq!(
            json!({ "entity_type": "media_player", "entity_id": "media_player.x", "attributes": { "media_title": "Song" } }),
            v["msg_data"]
        );
    }

    mod cache {
        use crate::config_store::ConfigStore;
        use crate::config_store::tests::device;
        use crate::controller::Controller;
        use crate::controller::test_util::{DeviceAddr, controller, test_dir, wait_stopped};
        use crate::device::{DeviceCache, SaveCacheNow};
        use actix::{Context, Handler, Message};

        /// Queue a cache save in the actor, then remove the device or start a new actor for it.
        /// The save waits in the mailbox of the actor until the controller is done.
        #[derive(Message)]
        #[rtype(result = "()")]
        struct SaveThenStop {
            remove: bool,
        }

        impl Handler<SaveThenStop> for Controller {
            type Result = ();
            fn handle(&mut self, msg: SaveThenStop, ctx: &mut Context<Self>) {
                let cfg = device();
                let device = self.devices.get(cfg.key.as_str()).expect("device");
                device.addr.do_send(SaveCacheNow);
                if msg.remove {
                    self.remove_device(&cfg.key);
                } else {
                    self.add_device(cfg, DeviceCache::default(), ctx);
                }
            }
        }

        /// Run [`SaveThenStop`] and return `true` if the cache file exists at the end.
        async fn cache_exists_after(name: &str, remove: bool) -> bool {
            let dir = test_dir(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            let cfg = device();
            ConfigStore::new(&dir)
                .save_devices(std::slice::from_ref(&cfg))
                .expect("save")
                .wait()
                .await
                .expect("write");
            let controller = controller(name);
            let actor = controller
                .send(DeviceAddr(cfg.key.clone()))
                .await
                .expect("send")
                .expect("actor");
            controller
                .send(SaveThenStop { remove })
                .await
                .expect("send");
            wait_stopped(&controller, &actor).await;
            let exists = dir.join(format!("cache_{}.json", cfg.key)).exists();
            let _ = std::fs::remove_dir_all(&dir);
            exists
        }

        #[actix::test]
        async fn removed_device_has_no_cache_after_a_late_save() {
            assert!(!cache_exists_after("cache-remove", true).await);
        }

        #[actix::test]
        async fn replaced_device_keeps_the_cache() {
            assert!(cache_exists_after("cache-replace", false).await);
        }
    }
}
