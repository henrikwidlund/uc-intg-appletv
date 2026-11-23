// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Central controller: handles the Integration-API messages and the Apple TV device actors.

mod devices;
mod handler;
mod messages;
#[cfg(test)]
pub(crate) mod test_util;

pub use messages::*;

use crate::config_store::{ConfigStore, DriverSettings};
use crate::configuration::{DEF_SETUP_TIMEOUT_SEC, ENV_SETUP_TIMEOUT, Settings};
use crate::controller::devices::Device;
use crate::controller::handler::{AbortDriverSetup, SetupState};
use crate::device::DeviceContext;
use crate::errors::ServiceError;
use crate::server::ArtworkStore;
use actix::prelude::{Actor, Context, Recipient};
use actix::{AsyncContext, SpawnHandle};
use bytestring::ByteString;
use log::{debug, error, info, warn};
use rust_fsm::state_machine;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;
use uc_api::intg::{DeviceState, IntegrationDriverUpdate};
use uc_api::ws::{EventCategory, WsMessage};

state_machine! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub operation_mode(RequireSetup)

    RequireSetup => {
        ConfigurationAvailable => Running,
        AbortSetup => RequireSetup,
        SetupDriverRequest => SetupFlow [SetupFlowTimer],
    },
    Running => {
        SetupDriverRequest => SetupFlow [SetupFlowTimer],
        AbortSetup => RequireSetup,
    },
    SetupFlow => {
        RequestUserInput => WaitSetupUserData,
        Successful => Running [CancelSetupFlowTimer],
        SetupError => SetupError [CancelSetupFlowTimer],
        AbortSetup => RequireSetup [CancelSetupFlowTimer],
    },
    WaitSetupUserData => {
        SetupUserData => SetupFlow,
        SetupError => SetupError [CancelSetupFlowTimer],
        AbortSetup => RequireSetup [CancelSetupFlowTimer],
    },
    SetupError => {
        ConfigurationAvailable => Running,
        AbortSetup => RequireSetup,
        SetupDriverRequest => SetupFlow [SetupFlowTimer],
        SetupError => SetupError,
    }
}

/// One connected Remote.
#[allow(clippy::struct_excessive_bools)]
struct R2Session {
    recipient: Recipient<SendWsMessage>,
    /// Id of the last request message from the driver to the Remote.
    ws_id: u32,
    standby: bool,
    /// The Remote sent the `disconnect` event and wants no device connections until `connect`,
    /// `exit_standby` or its next request.
    devices_off: bool,
    subscribed_entities: HashSet<String>,
    /// Subscribed to all entities: `subscribe_events` without entity ids.
    subscribe_all: bool,
    /// The Remote read the entity states. Before that, `subscribe_events` sends nothing,
    /// because the Remote reads all states anyway, and changes in standby are not kept.
    states_read: bool,
    /// Entities whose current state the Remote may not have: changed after the states were read
    /// while not subscribed or while in standby, or left out of the answer. They are sent when
    /// subscribed, or when the Remote wakes up if subscribed already.
    stale: HashSet<String>,
}

impl R2Session {
    fn new(recipient: Recipient<SendWsMessage>) -> Self {
        Self {
            recipient,
            ws_id: 0,
            standby: false,
            devices_off: false,
            subscribed_entities: HashSet::default(),
            subscribe_all: false,
            states_read: false,
            stale: HashSet::default(),
        }
    }

    /// The Remote is awake and wants device connections.
    const fn awake(&self) -> bool {
        !self.standby && !self.devices_off
    }

    /// The Remote subscribed to an entity of the device with `key`. No subscription means no
    /// connection, also when `get_entity_states` returns all entities.
    fn uses(&self, key: &str) -> bool {
        self.subscribe_all
            || self
                .subscribed_entities
                .iter()
                .any(|id| devices::is_of_device(id, key))
    }

    /// The Remote gets events of this entity.
    fn wants(&self, entity_id: &str) -> bool {
        self.subscribe_all || self.subscribed_entities.contains(entity_id)
    }

    /// Changes of this entity must be kept: for a later subscription, or until the Remote wakes
    /// up.
    fn tracks(&self, entity_id: &str) -> bool {
        self.keeps(self.wants(entity_id))
    }

    /// [`Self::tracks`] for an entity, with `wants` from [`Self::wants`].
    /// A subscribed entity is kept only in standby, and only [`Controller::wake`] ends standby.
    const fn keeps(&self, wants: bool) -> bool {
        self.states_read && (self.standby || !wants)
    }

    const fn new_msg_id(&mut self) -> u32 {
        self.ws_id += 1;
        self.ws_id
    }
}

/// Central controller handling the Integration-API messages.
pub struct Controller {
    /// Connected Remotes.
    sessions: HashMap<Arc<str>, R2Session>,
    /// Driver state reported to the Remote: error while one device has invalid credentials.
    /// Apple TV availability is shown with the entity states.
    device_state: DeviceState,
    drv_metadata: IntegrationDriverUpdate,
    /// State machine for the driver state: setup flow or running.
    machine: operation_mode::StateMachine,
    /// Driver setup timeout handle.
    setup_timeout: Option<SpawnHandle>,
    /// State of the active setup flow.
    setup: Option<SetupState>,
    /// Session that started the active setup flow.
    setup_owner: Option<Arc<str>>,
    /// Id of the setup flow. Changes on each start and end, so that old setup tasks are ignored.
    setup_flow: u32,
    /// A first setup or a reset: when an Apple TV is paired, a matching configured device keeps
    /// its key, id and name, and all other devices are removed. Nothing is removed if no Apple TV
    /// is paired. Discovery also shows configured devices. A restore replaces all devices on its
    /// own.
    setup_replace: bool,
    store: ConfigStore,
    driver_settings: DriverSettings,
    device_ctx: Arc<DeviceContext>,
    /// Configured Apple TVs by device key.
    devices: HashMap<Arc<str>, Device>,
    /// Last sent attributes by entity id.
    entities: HashMap<String, serde_json::Map<String, serde_json::Value>>,
    /// Device key of each active voice session.
    voice_sessions: HashMap<u32, Arc<str>>,
}

impl Controller {
    pub fn new(
        settings: Settings,
        driver_settings: DriverSettings,
        drv_metadata: IntegrationDriverUpdate,
        artwork: ArtworkStore,
    ) -> Self {
        let store = ConfigStore::new(settings.config_home.clone());
        let http_address = settings
            .interface
            .parse::<IpAddr>()
            .ok()
            .filter(|ip| !ip.is_unspecified());
        let device_ctx = Arc::new(DeviceContext {
            client_name: settings.client_name.clone(),
            http_address,
            http_port: settings.http_port,
            store: store.clone(),
            artwork,
            siri_mode: AtomicU8::new(driver_settings.siri_mode as u8),
        });
        Self {
            sessions: HashMap::default(),
            device_state: DeviceState::Connected,
            drv_metadata,
            machine: operation_mode::StateMachine::new(),
            setup_timeout: None,
            setup: None,
            setup_owner: None,
            setup_flow: 0,
            setup_replace: false,
            store,
            driver_settings,
            device_ctx,
            devices: HashMap::default(),
            entities: HashMap::default(),
            voice_sessions: HashMap::default(),
        }
    }

    /// Send a WebSocket message to a Remote. Messages are dropped while the Remote is in standby.
    fn send_r2_msg(&self, ws_id: &str, message: impl Into<SendWsMessage>) {
        if let Some(session) = self.sessions.get(ws_id) {
            Self::send_to_session(ws_id, session, message.into());
        } else {
            warn!("Attempting to send message but couldn't find session: {ws_id}");
        }
    }

    /// [`Self::send_r2_msg`] for a session that the caller found already.
    fn send_to_session(ws_id: &str, session: &R2Session, message: SendWsMessage) {
        if session.standby {
            if let SendWsMessage::Msg(m) = &message {
                debug!(
                    "[{ws_id}] Remote is in standby, not sending message: {:?}",
                    m.msg
                );
            }
            return;
        }
        if let Err(e) = session.recipient.try_send(message) {
            error!("[{ws_id}] Internal message send error: {e}");
        }
    }

    /// Send the answer to a request. It uses the same queue as the events, so a later event cannot
    /// be sent first. It is never dropped: not in standby and not with a full mailbox.
    fn send_r2_response(&self, ws_id: &str, message: impl Into<SendWsMessage>) {
        if let Some(session) = self.sessions.get(ws_id) {
            session.recipient.do_send(message.into());
        } else {
            warn!("Attempting to send response but couldn't find session: {ws_id}");
        }
    }

    /// Mark the Remote awake: not in standby and not after `disconnect`.
    /// Returns true if it was not awake before.
    fn wake(&mut self, ws_id: &str) -> bool {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return false;
        };
        let woke = !session.awake();
        session.standby = false;
        session.devices_off = false;
        woke
    }

    /// Send a `device_state` event to a Remote.
    fn send_device_state(&self, ws_id: &str) {
        debug!("[{ws_id}] sending device_state: {}", self.device_state);
        self.send_r2_msg(ws_id, device_state_event(self.device_state));
    }

    /// Perform a state machine transition for the given input.
    ///
    /// Returns an error if the transition is not allowed in the current state.
    fn sm_consume(
        &mut self,
        ws_id: &str,
        input: &operation_mode::Input,
        ctx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        let old_state = self.machine.state().clone();
        match self.machine.consume(input) {
            Ok(output) => {
                let state = self.machine.state();
                if *state != old_state {
                    info!("State machine transition: {old_state:?} -> {state:?}");
                }
                match output {
                    Some(operation_mode::Output::SetupFlowTimer) => {
                        if let Some(handle) = self.setup_timeout.take() {
                            ctx.cancel_future(handle);
                        }
                        if let Some(timeout) = setup_timeout() {
                            debug!("Starting SetupFlowTimer: {} sec", timeout.as_secs());
                            self.setup_timeout = Some(ctx.notify_later(
                                AbortDriverSetup {
                                    ws_id: ws_id.to_string(),
                                    timeout: true,
                                },
                                timeout,
                            ));
                        }
                    }
                    Some(operation_mode::Output::CancelSetupFlowTimer) => {
                        if let Some(handle) = self.setup_timeout.take() {
                            ctx.cancel_future(handle);
                        }
                    }
                    None => {}
                }
                Ok(())
            }
            Err(_) => Err(ServiceError::BadRequest(format!(
                "Transition {input:?} not allowed in state {old_state:?}"
            ))),
        }
    }
}

/// Mailbox size of the controller. The actix default is 16.
const CONTROLLER_MAILBOX_CAPACITY: usize = 1024;

/// Successful response, the same as [`WsMessage::response`] gives. Borrows the data, so that it
/// is not copied to serialize it.
#[derive(Serialize)]
struct ResponseRef<'a, T> {
    kind: &'static str,
    req_id: u32,
    msg: &'a str,
    code: u16,
    msg_data: T,
}

/// Serialize a successful response.
fn response_text<T: Serialize>(
    req_id: u32,
    msg: &str,
    msg_data: T,
) -> Result<ByteString, ServiceError> {
    let response = ResponseRef {
        kind: "resp",
        req_id,
        msg,
        code: 200,
        msg_data,
    };
    serde_json::to_string(&response)
        .map(ByteString::from)
        .map_err(|e| ServiceError::InternalServerError(format!("Error serializing {msg}: {e}")))
}

fn device_state_event(state: DeviceState) -> WsMessage {
    WsMessage::event(
        "device_state",
        EventCategory::Device,
        json!({ "state": state }),
    )
}

/// Get the setup flow timeout. `None` if disabled with `UC_SETUP_TIMEOUT=0`.
fn setup_timeout() -> Option<Duration> {
    let timeout = env::var(ENV_SETUP_TIMEOUT).map_or(DEF_SETUP_TIMEOUT_SEC, |value| {
        value.trim().parse().unwrap_or_else(|_| {
            warn!("Invalid {ENV_SETUP_TIMEOUT} value '{value}', using default: {DEF_SETUP_TIMEOUT_SEC}");
            DEF_SETUP_TIMEOUT_SEC
        })
    });
    (timeout > 0).then(|| Duration::from_secs(timeout))
}

impl Actor for Controller {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Remote events and device events must not be dropped in a burst.
        ctx.set_mailbox_capacity(CONTROLLER_MAILBOX_CAPACITY);
        let devices = self.store.load_devices();
        info!("{} configured Apple TV devices", devices.len());
        if !devices.is_empty() {
            let _ = self.sm_consume("", &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        for cfg in devices {
            let cache = self.store.load_cache(&cfg.key).unwrap_or_default();
            self.add_device(cfg, cache, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{R2Session, SendWsMessage};
    use actix::{Actor, Context, Handler};

    struct Sink;

    impl Actor for Sink {
        type Context = Context<Self>;
    }

    impl Handler<SendWsMessage> for Sink {
        type Result = ();
        fn handle(&mut self, _: SendWsMessage, _: &mut Context<Self>) {}
    }

    #[actix::test]
    async fn session_awake_and_uses() {
        let mut s = R2Session::new(Sink.start().recipient());
        assert!(s.awake());
        assert!(!s.uses("abc"), "no subscription connects nothing");

        s.subscribed_entities.insert("media_player.abc".into());
        assert!(s.uses("abc"));
        assert!(!s.uses("ab"), "key must match the whole key part");

        s.subscribed_entities.clear();
        s.subscribed_entities
            .insert("select.abc.playback_speed".into());
        assert!(s.uses("abc"));

        s.subscribed_entities.clear();
        s.subscribe_all = true;
        assert!(s.uses("xyz"));

        s.standby = true;
        assert!(!s.awake());
        s.standby = false;
        s.devices_off = true;
        assert!(!s.awake());
    }

    mod flow {
        use crate::config_store::tests::device;
        use crate::controller::devices::Device;
        use crate::controller::test_util::{TestRemote, connect, controller, settle};
        use crate::controller::{Controller, R2EventMsg, R2RequestMsg};
        use crate::device::{AppleTvActor, DeviceEvent, DeviceStatus, Reply};
        use crate::entities::EntityKind;
        use crate::errors::ServiceError;
        use actix::{Addr, AsyncContext, Context, Handler, Message};
        use serde_json::{Map, Value, json};
        use std::sync::{Arc, Mutex};
        use uc_api::intg::AssistantEvent;
        use uc_api::intg::ws::{R2Event, R2Request};

        /// Adds a device to the controller without a running device actor, so that nothing
        /// connects to the network.
        #[derive(Message)]
        #[rtype(result = "Option<Arc<str>>")]
        struct AddDevice;

        impl Handler<AddDevice> for Controller {
            type Result = Option<Arc<str>>;
            fn handle(&mut self, _: AddDevice, _: &mut Context<Self>) -> Option<Arc<str>> {
                let cfg = device();
                let key: Arc<str> = cfg.key.as_str().into();
                let addr = Context::<AppleTvActor>::new().address();
                self.devices.insert(
                    key.clone(),
                    Device {
                        cfg,
                        addr,
                        status: DeviceStatus::Disconnected,
                        wanted: false,
                    },
                );
                Some(key)
            }
        }

        /// Send a request and return the response that the controller returns. The Remote does
        /// not get it. For a request that the controller answers itself, it returns `None` and
        /// the Remote gets the answer.
        async fn request(
            controller: &Addr<Controller>,
            ws_id: &str,
            request: R2Request,
        ) -> Option<Value> {
            request_data(controller, ws_id, request, None).await
        }

        /// [`request`] with `msg_data`.
        async fn request_data(
            controller: &Addr<Controller>,
            ws_id: &str,
            request: R2Request,
            msg_data: Option<Value>,
        ) -> Option<Value> {
            controller
                .send(R2RequestMsg {
                    ws_id: ws_id.into(),
                    req_id: 1,
                    request,
                    msg_data,
                })
                .await
                .expect("send")
                .expect("ok")
                .map(|m| serde_json::to_value(m).expect("json"))
        }

        async fn event(controller: &Addr<Controller>, ws_id: &str, event: R2Event) {
            controller
                .send(R2EventMsg {
                    ws_id: ws_id.into(),
                    event,
                })
                .await
                .expect("send");
        }

        fn change(key: &Arc<str>, state: &str) -> DeviceEvent {
            change_of(key, EntityKind::MediaPlayer, state)
        }

        fn change_of(key: &Arc<str>, kind: EntityKind, state: &str) -> DeviceEvent {
            let mut attrs = Map::new();
            attrs.insert("state".into(), state.into());
            DeviceEvent::Changes {
                key: key.clone(),
                changes: vec![(kind, attrs)],
            }
        }

        /// Subscribe one entity.
        async fn subscribe(controller: &Addr<Controller>, ws_id: &str, entity_id: &str) {
            request_data(
                controller,
                ws_id,
                R2Request::SubscribeEvents,
                Some(json!({ "entity_ids": [entity_id] })),
            )
            .await;
        }

        /// One Remote with a device.
        async fn single(name: &str) -> (Addr<Controller>, Arc<str>, TestRemote) {
            let controller = controller(name);
            let a = connect(&controller, "a").await;
            let key = controller
                .send(AddDevice)
                .await
                .expect("send")
                .expect("key");
            (controller, key, a)
        }

        /// The `entity_change` events of `entity_id`.
        fn changes(messages: &Mutex<Vec<Value>>, entity_id: &str) -> Vec<Value> {
            messages
                .lock()
                .expect("lock")
                .iter()
                .filter(|m| m["msg"] == "entity_change" && m["msg_data"]["entity_id"] == entity_id)
                .cloned()
                .collect()
        }

        fn position(messages: &Mutex<Vec<Value>>, f: impl Fn(&Value) -> bool) -> Option<usize> {
            messages.lock().expect("lock").iter().position(f)
        }

        /// Returns `true` if the controller knows the voice session.
        #[derive(Message)]
        #[rtype(result = "bool")]
        struct HasVoiceSession(u32);

        impl Handler<HasVoiceSession> for Controller {
            type Result = bool;
            fn handle(&mut self, msg: HasVoiceSession, _: &mut Context<Self>) -> bool {
                self.voice_sessions.contains_key(&msg.0)
            }
        }

        #[actix::test]
        async fn voice_start_for_a_stopped_device_ends_the_session() {
            let (controller, key, a) = single("voice-stopped").await;
            settle(&controller, &[&a]).await;
            let before = a.lock().expect("lock").len();
            let result = controller
                .send(R2RequestMsg {
                    ws_id: "a".into(),
                    req_id: 3,
                    request: R2Request::EntityCommand,
                    msg_data: Some(json!({
                        "entity_type": "voice_assistant",
                        "entity_id": EntityKind::VoiceAssistant.entity_id(&key),
                        "cmd_id": "voice_start",
                        "params": { "session_id": 9 },
                    })),
                })
                .await
                .expect("send");
            assert_eq!(
                Err(ServiceError::NotFound(
                    "The Apple TV was removed or reconfigured".into()
                )),
                result.map(|_| ())
            );
            settle(&controller, &[&a]).await;
            assert_eq!(before, a.lock().expect("lock").len(), "no message");
            assert!(!controller.send(HasVoiceSession(9)).await.expect("send"));
        }

        #[actix::test]
        async fn voice_reply_comes_before_assistant_events() {
            let controller = controller("voice-reply");
            let a = connect(&controller, "a").await;
            // The device sends these in this order: the reply when Siri started, then the
            // events of the session.
            controller.do_send(DeviceEvent::Accepted(Reply {
                ws_id: "a".into(),
                req_id: 7,
            }));
            controller.do_send(DeviceEvent::Assistant(AssistantEvent::Finished {
                entity_id: "voice_assistant.x".into(),
                session_id: 1,
            }));
            settle(&controller, &[&a]).await;
            let reply = position(&a, |m| m["req_id"] == 7 && m["code"] == 200);
            let event = position(&a, |m| m["msg"] == "assistant_event");
            assert!(reply.is_some());
            assert!(reply < event, "reply first");
        }

        #[actix::test]
        async fn assistant_event_goes_to_each_remote() {
            let controller = controller("assistant-broadcast");
            let a = connect(&controller, "a").await;
            let b = connect(&controller, "b").await;
            let c = connect(&controller, "c").await;
            event(&controller, "c", R2Event::EnterStandby).await;
            controller.do_send(DeviceEvent::Assistant(AssistantEvent::Finished {
                entity_id: "voice_assistant.x".into(),
                session_id: 2,
            }));
            settle(&controller, &[&a, &b, &c]).await;
            let events = |r: &TestRemote| -> Vec<Value> {
                r.lock()
                    .expect("lock")
                    .iter()
                    .filter(|m| m["msg"] == "assistant_event")
                    .cloned()
                    .collect()
            };
            assert!(events(&c).is_empty(), "in standby");
            let (a, b) = (events(&a), events(&b));
            assert_eq!(1, a.len());
            assert_eq!(a, b, "the same text");
            assert_eq!("event", a[0]["kind"]);
            assert_eq!("ENTITY", a[0]["cat"]);
            assert_eq!(
                json!({ "type": "finished", "entity_id": "voice_assistant.x", "session_id": 2 }),
                a[0]["msg_data"]
            );
        }

        #[actix::test]
        async fn entity_states_response_comes_before_a_later_change() {
            let controller = controller("states-order");
            let messages = connect(&controller, "a").await;
            let key = controller
                .send(AddDevice)
                .await
                .expect("send")
                .expect("key");
            assert!(
                request(&controller, "a", R2Request::SubscribeEvents)
                    .await
                    .is_some()
            );

            let response = controller.send(R2RequestMsg {
                ws_id: "a".into(),
                req_id: 7,
                request: R2Request::GetEntityStates,
                msg_data: None,
            });
            controller.do_send(change(&key, "ON"));
            assert!(
                response.await.expect("send").expect("ok").is_none(),
                "the controller sends the response itself"
            );
            settle(&controller, &[&messages]).await;

            let resp =
                position(&messages, |m| m["kind"] == "resp" && m["req_id"] == 7).expect("response");
            let entity_id = EntityKind::MediaPlayer.entity_id(&key);
            let change = position(&messages, |m| {
                m["msg"] == "entity_change" && m["msg_data"]["entity_id"] == entity_id.as_str()
            })
            .expect("event");
            assert!(
                resp < change,
                "response {resp} must come before the event {change}"
            );
            let resp = messages.lock().expect("lock")[resp].clone();
            assert_eq!(
                json!({ "kind": "resp", "req_id": 7, "msg": "entity_states", "code": 200, "msg_data": [] }),
                resp
            );
        }

        #[actix::test]
        async fn device_state_response_is_sent_by_the_controller() {
            let controller = controller("device-state");
            let messages = connect(&controller, "a").await;
            settle(&controller, &[&messages]).await;
            let before = messages.lock().expect("lock").len();
            assert!(
                request(&controller, "a", R2Request::GetDeviceState)
                    .await
                    .is_none()
            );
            settle(&controller, &[&messages]).await;
            let messages = messages.lock().expect("lock");
            assert_eq!(before + 1, messages.len());
            assert_eq!("device_state", messages[before]["msg"]);
            assert_eq!("CONNECTED", messages[before]["msg_data"]["state"]);
        }

        async fn standby_setup(name: &str) -> (Addr<Controller>, Arc<str>, TestRemote, TestRemote) {
            let controller = controller(name);
            let a = connect(&controller, "a").await;
            let b = connect(&controller, "b").await;
            let key = controller
                .send(AddDevice)
                .await
                .expect("send")
                .expect("key");
            for ws_id in ["a", "b"] {
                request(&controller, ws_id, R2Request::GetEntityStates).await;
                request(&controller, ws_id, R2Request::SubscribeEvents).await;
            }
            event(&controller, "a", R2Event::EnterStandby).await;
            controller.send(change(&key, "ON")).await.expect("send");
            controller
                .send(change(&key, "PLAYING"))
                .await
                .expect("send");
            settle(&controller, &[&a, &b]).await;
            (controller, key, a, b)
        }

        #[actix::test]
        async fn changes_in_standby_are_sent_once_at_wake_up() {
            let (controller, key, a, b) = standby_setup("standby-wake").await;
            let entity_id = EntityKind::MediaPlayer.entity_id(&key);
            assert_eq!(
                Vec::<Value>::new(),
                changes(&a, &entity_id),
                "nothing in standby"
            );
            assert_eq!(2, changes(&b, &entity_id).len());

            let before = a.lock().expect("lock").len();
            event(&controller, "a", R2Event::ExitStandby).await;
            settle(&controller, &[&a]).await;
            let missed = changes(&a, &entity_id);
            assert_eq!(1, missed.len());
            assert_eq!("PLAYING", missed[0]["msg_data"]["attributes"]["state"]);
            let first = a.lock().expect("lock")[before]["msg"].clone();
            assert_eq!("device_state", first, "device_state comes first");

            // Sent only once.
            event(&controller, "a", R2Event::ExitStandby).await;
            settle(&controller, &[&a]).await;
            assert_eq!(1, changes(&a, &entity_id).len());
        }

        #[actix::test]
        async fn entity_states_after_standby_sends_no_events() {
            let (controller, key, a, _) = standby_setup("standby-states").await;
            let entity_id = EntityKind::MediaPlayer.entity_id(&key);
            assert!(
                request(&controller, "a", R2Request::GetEntityStates)
                    .await
                    .is_none()
            );
            settle(&controller, &[&a]).await;
            assert_eq!(
                Vec::<Value>::new(),
                changes(&a, &entity_id),
                "the response holds the state"
            );
            let response = a
                .lock()
                .expect("lock")
                .iter()
                .rfind(|m| m["msg"] == "entity_states")
                .cloned()
                .expect("response");
            assert_eq!("PLAYING", response["msg_data"][0]["attributes"]["state"]);

            // Another request does not send it either.
            request(&controller, "a", R2Request::GetDriverVersion).await;
            settle(&controller, &[&a]).await;
            assert_eq!(Vec::<Value>::new(), changes(&a, &entity_id));
        }

        #[actix::test]
        async fn device_state_request_comes_before_missed_states() {
            let (controller, key, a, _) = standby_setup("standby-device-state").await;
            let entity_id = EntityKind::MediaPlayer.entity_id(&key);
            let before = a.lock().expect("lock").len();
            assert!(
                request(&controller, "a", R2Request::GetDeviceState)
                    .await
                    .is_none()
            );
            settle(&controller, &[&a]).await;
            let messages = a.lock().expect("lock");
            assert_eq!(before + 2, messages.len());
            assert_eq!("device_state", messages[before]["msg"]);
            assert_eq!("entity_change", messages[before + 1]["msg"]);
            assert_eq!(
                entity_id.as_str(),
                messages[before + 1]["msg_data"]["entity_id"]
            );
        }

        #[actix::test]
        async fn wake_sends_only_subscribed_ids() {
            let (controller, key, a) = single("standby-ids").await;
            let player = EntityKind::MediaPlayer.entity_id(&key);
            let remote = EntityKind::Remote.entity_id(&key);
            request(&controller, "a", R2Request::GetEntityStates).await;
            subscribe(&controller, "a", &player).await;
            event(&controller, "a", R2Event::EnterStandby).await;
            controller
                .send(change(&key, "PLAYING"))
                .await
                .expect("send");
            controller
                .send(change_of(&key, EntityKind::Remote, "ON"))
                .await
                .expect("send");
            event(&controller, "a", R2Event::ExitStandby).await;
            settle(&controller, &[&a]).await;
            assert_eq!(1, changes(&a, &player).len());
            assert_eq!(Vec::<Value>::new(), changes(&a, &remote));

            // The other entity stays stale until it is subscribed.
            subscribe(&controller, "a", &remote).await;
            settle(&controller, &[&a]).await;
            assert_eq!(1, changes(&a, &remote).len());
            assert_eq!(1, changes(&a, &player).len());
        }

        #[actix::test]
        async fn entity_states_clears_subscribed_stale_ids() {
            let (controller, key, a) = single("standby-ids-states").await;
            let player = EntityKind::MediaPlayer.entity_id(&key);
            request(&controller, "a", R2Request::GetEntityStates).await;
            subscribe(&controller, "a", &player).await;
            event(&controller, "a", R2Event::EnterStandby).await;
            controller
                .send(change(&key, "PLAYING"))
                .await
                .expect("send");
            request(&controller, "a", R2Request::GetEntityStates).await;
            request(&controller, "a", R2Request::GetDriverVersion).await;
            // A wake-up event sends missed states also when the Remote is awake.
            event(&controller, "a", R2Event::ExitStandby).await;
            settle(&controller, &[&a]).await;
            assert_eq!(Vec::<Value>::new(), changes(&a, &player));
        }

        #[actix::test]
        async fn no_missed_states_before_states_read() {
            let (controller, key, a) = single("standby-unread").await;
            let player = EntityKind::MediaPlayer.entity_id(&key);
            request(&controller, "a", R2Request::SubscribeEvents).await;
            event(&controller, "a", R2Event::EnterStandby).await;
            controller
                .send(change(&key, "PLAYING"))
                .await
                .expect("send");
            event(&controller, "a", R2Event::ExitStandby).await;
            settle(&controller, &[&a]).await;
            assert_eq!(Vec::<Value>::new(), changes(&a, &player));
        }

        #[actix::test]
        async fn other_request_sends_changes_from_standby() {
            let (controller, key, a, _) = standby_setup("standby-request").await;
            let entity_id = EntityKind::MediaPlayer.entity_id(&key);
            assert!(
                request(&controller, "a", R2Request::GetDriverVersion)
                    .await
                    .is_some()
            );
            settle(&controller, &[&a]).await;
            assert_eq!(1, changes(&a, &entity_id).len());
        }
    }
}
