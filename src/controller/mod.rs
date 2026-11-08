// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Central controller: handles the Integration-API messages and the Apple TV device actors.

mod devices;
mod handler;
mod messages;

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
use log::{debug, error, info, warn};
use rust_fsm::state_machine;
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
        R2Request => Running,
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
struct R2Session {
    recipient: Recipient<SendWsMessage>,
    /// Id of the last request message from the driver to the Remote.
    ws_id: u32,
    standby: bool,
    subscribed_entities: HashSet<String>,
    /// Flag if the session is in a reconfiguration setup flow.
    reconfiguring: Option<bool>,
}

impl R2Session {
    fn new(recipient: Recipient<SendWsMessage>) -> Self {
        Self {
            recipient,
            ws_id: 0,
            standby: false,
            subscribed_entities: HashSet::default(),
            reconfiguring: None,
        }
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
    /// Driver state reported to the Remote. Apple TV availability is shown with the entity states.
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
            siri_timing: AtomicU8::new(driver_settings.siri_timing as u8),
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
            store,
            driver_settings,
            device_ctx,
            devices: HashMap::default(),
            entities: HashMap::default(),
            voice_sessions: HashMap::default(),
        }
    }

    /// Send a WebSocket message to a Remote. Messages are dropped while the Remote is in standby.
    fn send_r2_msg(&self, message: WsMessage, ws_id: &str) {
        if let Some(session) = self.sessions.get(ws_id) {
            if session.standby {
                debug!(
                    "[{ws_id}] Remote is in standby, not sending message: {:?}",
                    message.msg
                );
                return;
            }
            if let Err(e) = session.recipient.try_send(SendWsMessage(message)) {
                error!("[{ws_id}] Internal message send error: {e}");
            }
        } else {
            warn!("Attempting to send message but couldn't find session: {ws_id}");
        }
    }

    /// Send a `device_state` event to a Remote.
    fn send_device_state(&self, ws_id: &str) {
        debug!("[{ws_id}] sending device_state: {}", self.device_state);
        self.send_r2_msg(device_state_event(self.device_state), ws_id);
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
        let devices = self.store.load_devices();
        info!("{} configured Apple TV devices", devices.len());
        if !devices.is_empty() {
            let _ = self.sm_consume("", &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        for cfg in devices {
            self.add_device(cfg, ctx);
        }
    }
}
