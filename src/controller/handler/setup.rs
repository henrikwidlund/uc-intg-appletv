// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Driver setup flow: discovery, pairing and reconfiguration.

use crate::config_store::{
    DeviceConfig, DriverSettings, SiriMode, backup_json, device_key, parse_backup,
};
use crate::controller::handler::AbortDriverSetup;
use crate::controller::{Controller, operation_mode};
use crate::device::{UpdateConfig, discovery};
use crate::entities::EntityKind;
use crate::errors::ServiceError;
use crate::logging;
use actix::{ActorFutureExt, AsyncContext, Context, Handler, Message, WrapFuture};
use log::{debug, info, warn};
use ratv::discovery::Device;
use ratv::{Endpoints, PairOptions, PinPrompt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;
use uc_api::intg::{DriverSetupChange, IntegrationSetup, SetupDriver};
use uc_api::model::intg::{IntegrationSetupError, IntegrationSetupState, SetupChangeEventType};
use uc_api::ws::{EventCategory, WsMessage};

/// Delay so that the request is acknowledged before the next setup event.
const ACK_DELAY: Duration = Duration::from_millis(100);

/// Start the setup flow.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetupDriverMsg {
    pub ws_id: String,
    pub data: SetupDriver,
}

/// User input of a setup page.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetDriverUserDataMsg {
    pub ws_id: String,
    pub data: IntegrationSetup,
}

/// Apple TV found in the setup flow.
#[derive(Debug, Clone)]
pub struct Found {
    id: String,
    name: String,
    model: String,
    address: Option<IpAddr>,
    endpoints: Endpoints,
    known_ids: Vec<String>,
}

impl Found {
    fn new(device: &Device) -> Option<Self> {
        Some(Self {
            id: device.identifier()?.to_string(),
            name: device.name().to_string(),
            model: device.model().unwrap_or_default().to_string(),
            address: device.addresses.first().copied(),
            endpoints: Endpoints::from(device),
            known_ids: discovery::device_ids(device).map(str::to_string).collect(),
        })
    }
}

/// Page the setup flow waits for.
pub enum SetupState {
    /// Optional IP address for the discovery.
    Discover,
    /// First setup: restore a backup or continue with the discovery.
    RestoreChoice,
    /// Backup text of the devices. The page is only read by the user.
    Backup,
    /// Backup text to restore.
    Restore,
    /// Reconfigure: device and action.
    ConfigMode,
    /// List of found devices. `manual_address`: the user entered the IP address.
    ChooseDevice {
        found: Vec<Found>,
        manual_address: bool,
    },
    /// PIN shown on the Apple TV.
    Pin {
        prompt: Box<PinPrompt>,
        found: Found,
        global_volume: bool,
        manual_address: bool,
        /// Key of the device that is paired again.
        replace: Option<String>,
    },
    /// Options of a configured device, and the Apple TVs on the network it can be linked to.
    Configure { key: String, found: Vec<Found> },
}

/// Error of the input of a setup page.
enum InputError {
    /// Invalid input. The page is shown again with the message.
    Invalid(Box<SetupState>, String),
    /// The setup flow ends with this error.
    Failed(IntegrationSetupError),
}

impl Handler<SetupDriverMsg> for Controller {
    type Result = Result<(), ServiceError>;

    fn handle(&mut self, msg: SetupDriverMsg, ctx: &mut Self::Context) -> Self::Result {
        debug!("[{}] {:?}", msg.ws_id, msg.data);
        if self
            .sm_consume(&msg.ws_id, &operation_mode::Input::SetupDriverRequest, ctx)
            .is_err()
        {
            return Err(ServiceError::BadRequest(
                "Cannot start driver setup. Please abort setup first.".into(),
            ));
        }
        let reconfigure = msg.data.reconfigure == Some(true);
        if let Some(session) = self.sessions.get_mut(msg.ws_id.as_str()) {
            session.reconfiguring = msg.data.reconfigure;
        }
        self.end_setup_flow();
        self.setup_owner = Some(msg.ws_id.as_str().into());
        let flow = self.setup_flow;
        let ws_id = msg.ws_id;
        if reconfigure {
            ctx.run_later(ACK_DELAY, move |act, ctx| {
                if !act.is_setup_flow(flow, &ws_id) {
                    return;
                }
                let page = act.config_mode_page();
                act.setup = Some(SetupState::ConfigMode);
                act.request_input(&ws_id, page, ctx);
            });
        } else {
            // A first setup starts with a clean configuration.
            if let Err(e) = self.reset_devices() {
                self.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
                return Err(e);
            }
            ctx.run_later(ACK_DELAY, move |act, ctx| {
                if act.is_setup_flow(flow, &ws_id) {
                    act.setup = Some(SetupState::RestoreChoice);
                    let page = restore_choice_page(act.driver_settings);
                    act.request_input(&ws_id, page, ctx);
                }
            });
        }
        Ok(())
    }
}

impl Handler<SetDriverUserDataMsg> for Controller {
    type Result = Result<(), ServiceError>;

    fn handle(&mut self, msg: SetDriverUserDataMsg, ctx: &mut Self::Context) -> Self::Result {
        debug!("[{}] setup user data", msg.ws_id);
        if self
            .setup_owner
            .as_deref()
            .is_some_and(|owner| owner != msg.ws_id)
        {
            return Err(ServiceError::BadRequest(
                "Setup was started by another Remote.".into(),
            ));
        }
        let IntegrationSetup::InputValues(values) = msg.data else {
            return Err(ServiceError::BadRequest(
                "Invalid response: require input_values".into(),
            ));
        };
        if self
            .sm_consume(&msg.ws_id, &operation_mode::Input::SetupUserData, ctx)
            .is_err()
        {
            return Err(ServiceError::BadRequest(
                "Not waiting for driver user data. Please restart setup.".into(),
            ));
        }
        let ws_id = msg.ws_id;
        let Some(step) = self.setup.take() else {
            self.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
            return Err(ServiceError::BadRequest("No active setup step".into()));
        };
        let flow = self.setup_flow;
        match self.setup_input(&ws_id, step, &values, ctx) {
            Ok(()) => {}
            Err(InputError::Invalid(step, message)) => {
                info!("[{ws_id}] invalid setup input: {message}");
                // Show the same page again.
                self.setup = Some(*step);
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_input_again(&ws_id, &message, ctx);
                    }
                });
            }
            Err(InputError::Failed(error)) => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, Some(error), ctx);
                    }
                });
            }
        }
        Ok(())
    }
}

impl Handler<AbortDriverSetup> for Controller {
    type Result = ();

    fn handle(&mut self, msg: AbortDriverSetup, ctx: &mut Self::Context) {
        self.abort_setup(&msg.ws_id, msg.timeout, ctx);
    }
}

impl Controller {
    /// Abort the setup flow, because of a timeout, an abort event or a disconnect of the Remote.
    pub(super) fn abort_setup(&mut self, ws_id: &str, timeout: bool, ctx: &mut Context<Self>) {
        debug!("[{ws_id}] abort driver setup, timeout: {timeout}");
        if self
            .setup_owner
            .as_deref()
            .is_some_and(|owner| owner != ws_id)
        {
            warn!("[{ws_id}] setup was started by another Remote, not aborting");
            return;
        }
        if timeout {
            self.finish_setup(ws_id, Some(IntegrationSetupError::Timeout), ctx);
            return;
        }
        self.end_setup_flow();
        if self
            .sm_consume(ws_id, &operation_mode::Input::AbortSetup, ctx)
            .is_err()
        {
            return;
        }
        // A reconfiguration that was aborted keeps the configured devices.
        if let Some(session) = self.sessions.get_mut(ws_id) {
            session.reconfiguring = None;
        }
        if !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
            self.connect_devices();
        }
    }

    /// Forget the active setup flow. Tasks of the old flow are ignored.
    fn end_setup_flow(&mut self) {
        self.setup = None;
        self.setup_owner = None;
        self.setup_flow = self.setup_flow.wrapping_add(1);
    }

    /// Returns `true` if `flow` is the active setup flow.
    fn is_setup_flow(&self, flow: u32, ws_id: &str) -> bool {
        let active = self.setup_flow == flow;
        if !active {
            debug!("[{ws_id}] ignoring result of an old setup flow");
        }
        active
    }

    /// Handle the input of a setup page.
    fn setup_input(
        &mut self,
        ws_id: &str,
        step: SetupState,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let flow = self.setup_flow;
        let ws_id = ws_id.to_string();
        match step {
            SetupState::Discover => {
                let address = parse_address(values.get("address").map(String::as_str))
                    .map_err(|e| InputError::Invalid(Box::new(SetupState::Discover), e))?;
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.discover(ws_id, address, ctx);
                    }
                });
            }
            SetupState::RestoreChoice => self.restore_choice(ws_id, values, ctx),
            SetupState::Backup => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, None, ctx);
                    }
                });
            }
            SetupState::Restore => self.restore(ws_id, values, ctx)?,
            SetupState::ConfigMode => self.config_mode(ws_id, values, ctx)?,
            SetupState::ChooseDevice {
                mut found,
                manual_address,
            } => {
                let choice = values.get("choice").map(String::as_str).unwrap_or_default();
                let Some(i) = found.iter().position(|f| f.id == choice) else {
                    return Err(InputError::Invalid(
                        Box::new(SetupState::ChooseDevice {
                            found,
                            manual_address,
                        }),
                        "Unknown device".into(),
                    ));
                };
                let found = found.swap_remove(i);
                let global_volume = parse_bool(values, "global_volume", true);
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.pair(ws_id, found, global_volume, manual_address, None, ctx);
                    }
                });
            }
            SetupState::Pin {
                prompt,
                found,
                global_volume,
                manual_address,
                replace,
            } => {
                let pin = values.get("pin").map_or("", |p| p.trim());
                if pin.is_empty() {
                    return Err(InputError::Invalid(
                        Box::new(SetupState::Pin {
                            prompt,
                            found,
                            global_volume,
                            manual_address,
                            replace,
                        }),
                        "Missing PIN".into(),
                    ));
                }
                self.finish_pairing(
                    ws_id,
                    prompt,
                    pin.to_string(),
                    found,
                    global_volume,
                    manual_address,
                    replace,
                    ctx,
                );
            }
            SetupState::Configure { key, found } => {
                let address = match parse_address(values.get("address").map(String::as_str)) {
                    Ok(a) => a,
                    Err(e) => {
                        return Err(InputError::Invalid(
                            Box::new(SetupState::Configure { key, found }),
                            e,
                        ));
                    }
                };
                let error = self.configure(&key, &found, address, values).err();
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, error, ctx);
                    }
                });
            }
        }
        Ok(())
    }

    /// First setup: restore a backup or continue with the discovery.
    fn restore_choice(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) {
        let debug_log = parse_bool(values, "debug_log", self.driver_settings.debug_log);
        self.set_debug_log(&ws_id, debug_log);
        self.set_siri_mode(&ws_id, values);
        let flow = self.setup_flow;
        let restore = parse_bool(values, "restore_from_backup", false);
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if !act.is_setup_flow(flow, &ws_id) {
                return;
            }
            if restore {
                act.setup = Some(SetupState::Restore);
                act.request_input(&ws_id, restore_page(), ctx);
            } else {
                act.request_discover(&ws_id, ctx);
            }
        });
    }

    /// Replace the devices with the devices of the pasted backup.
    ///
    /// Nothing of the pasted data is logged.
    fn restore(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let flow = self.setup_flow;
        let devices = values
            .get("restore_data")
            .and_then(|text| parse_backup(text))
            .ok_or_else(|| {
                warn!("[{ws_id}] invalid restore data");
                InputError::Invalid(Box::new(SetupState::Restore), RESTORE_FAILED.into())
            })?;
        info!("[{ws_id}] restoring {} device(s)", devices.len());
        let error = self.restore_devices(devices, ctx).err().map(|e| {
            warn!("[{ws_id}] saving the configuration failed: {e}");
            IntegrationSetupError::Other
        });
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if act.is_setup_flow(flow, &ws_id) {
                act.finish_setup(&ws_id, error, ctx);
            }
        });
        Ok(())
    }

    /// Ask for an optional IP address, then discover.
    fn request_discover(&mut self, ws_id: &str, ctx: &mut Context<Self>) {
        self.setup = Some(SetupState::Discover);
        self.request_input(ws_id, discover_page(), ctx);
    }

    /// Scan for Apple TVs and show the ones that are not configured.
    fn discover(&self, ws_id: String, address: Option<IpAddr>, ctx: &mut Context<Self>) {
        info!("[{ws_id}] discovering Apple TV devices, address: {address:?}");
        let flow = self.setup_flow;
        ctx.spawn(
            async move { discovery::scan_apple_tvs(address).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let devices = match result {
                        Ok(d) => d,
                        Err(e) => {
                            warn!("[{ws_id}] discovery failed: {e}");
                            act.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
                            return;
                        }
                    };
                    let found: Vec<Found> = devices
                        .iter()
                        .filter(|d| {
                            !act.devices
                                .values()
                                .any(|c| c.cfg.matches(discovery::device_ids(d)))
                        })
                        .filter_map(Found::new)
                        .collect();
                    if found.is_empty() {
                        info!("[{ws_id}] no new Apple TV found");
                        act.finish_setup(&ws_id, Some(IntegrationSetupError::NotFound), ctx);
                        return;
                    }
                    let page = choose_device_page(&found);
                    act.setup = Some(SetupState::ChooseDevice {
                        found,
                        manual_address: address.is_some(),
                    });
                    act.request_input(&ws_id, page, ctx);
                }),
        );
    }

    fn config_mode(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let unknown_device =
            || InputError::Invalid(Box::new(SetupState::ConfigMode), "Unknown device".into());
        let flow = self.setup_flow;
        let action = values.get("action").map(String::as_str).unwrap_or_default();
        let key = values.get("choice").map(String::as_str).unwrap_or_default();
        self.set_debug_log(
            &ws_id,
            parse_bool(values, "debug_log", self.driver_settings.debug_log),
        );
        self.set_siri_mode(&ws_id, values);
        match action {
            "settings" => self.finish_setup_later(ws_id, ctx),
            "add" => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            "remove" => {
                let cfg = self.remove_device(key).ok_or_else(unknown_device)?;
                info!("[{ws_id}] removed {}", cfg.name);
                for session in self.sessions.values_mut() {
                    session
                        .subscribed_entities
                        .retain(|id| EntityKind::parse(id).is_none_or(|(k, _)| k != key));
                }
                if let Err(e) = self.save_devices() {
                    warn!("[{ws_id}] saving the configuration failed: {e}");
                    return Err(InputError::Failed(IntegrationSetupError::Other));
                }
                self.finish_setup_later(ws_id, ctx);
            }
            "configure" => {
                let cfg = self
                    .devices
                    .get(key)
                    .ok_or_else(unknown_device)?
                    .cfg
                    .clone();
                self.request_configure(ws_id, cfg, ctx);
            }
            "pair" => {
                let cfg = self
                    .devices
                    .get(key)
                    .ok_or_else(unknown_device)?
                    .cfg
                    .clone();
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.pair_again(ws_id, cfg, ctx);
                    }
                });
            }
            "backup" => {
                let text = backup_json(&self.sorted_configs())
                    .filter(|_| !self.devices.is_empty())
                    .ok_or_else(|| {
                        InputError::Invalid(
                            Box::new(SetupState::ConfigMode),
                            "Nothing to back up".into(),
                        )
                    })?;
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.setup = Some(SetupState::Backup);
                        act.request_input(&ws_id, backup_page(&text), ctx);
                    }
                });
            }
            "restore" => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.setup = Some(SetupState::Restore);
                        act.request_input(&ws_id, restore_page(), ctx);
                    }
                });
            }
            "reset" => {
                if let Err(e) = self.reset_devices() {
                    warn!("[{ws_id}] saving the configuration failed: {e}");
                    return Err(InputError::Failed(IntegrationSetupError::Other));
                }
                info!("[{ws_id}] configuration reset");
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            _ => {
                return Err(InputError::Invalid(
                    Box::new(SetupState::ConfigMode),
                    format!("Unknown action: {action}"),
                ));
            }
        }
        Ok(())
    }

    /// End the setup after the request was acknowledged.
    fn finish_setup_later(&self, ws_id: String, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if act.is_setup_flow(flow, &ws_id) {
                act.finish_setup(&ws_id, None, ctx);
            }
        });
    }

    /// Turn the debug log on or off and save the setting.
    fn set_debug_log(&mut self, ws_id: &str, on: bool) {
        if self.driver_settings.debug_log == on {
            return;
        }
        self.driver_settings.debug_log = on;
        logging::set_debug(on);
        if logging::env_filter() {
            info!(
                "[{ws_id}] debug log {}, RUST_LOG is used instead",
                if on { "saved" } else { "off" }
            );
        } else {
            info!("[{ws_id}] debug log {}", if on { "on" } else { "off" });
        }
        if let Err(e) = self.store.save_settings(self.driver_settings) {
            warn!("[{ws_id}] saving the settings failed: {e}");
        }
    }

    /// Set the Siri timing from the setup page and save it.
    fn set_siri_mode(&mut self, ws_id: &str, values: &HashMap<String, String>) {
        let Some(timing) = values.get("siri_mode").and_then(|v| SiriMode::from_id(v)) else {
            return;
        };
        if self.driver_settings.siri_mode == timing {
            return;
        }
        self.driver_settings.siri_mode = timing;
        self.device_ctx.set_siri_mode(timing);
        info!("[{ws_id}] Siri timing: {}", timing.label());
        if let Err(e) = self.store.save_settings(self.driver_settings) {
            warn!("[{ws_id}] saving the settings failed: {e}");
        }
    }

    /// Remove all devices and their entities.
    fn reset_devices(&mut self) -> Result<(), ServiceError> {
        if self.devices.is_empty() {
            return Ok(());
        }
        let keys: Vec<String> = self.devices.keys().map(ToString::to_string).collect();
        for key in &keys {
            self.remove_device(key);
        }
        self.entities.clear();
        for session in self.sessions.values_mut() {
            session.subscribed_entities.clear();
        }
        self.save_devices()
    }

    /// Replace all devices with the devices of a backup.
    fn restore_devices(
        &mut self,
        devices: Vec<DeviceConfig>,
        ctx: &Context<Self>,
    ) -> Result<(), ServiceError> {
        let keys: Vec<String> = self.devices.keys().map(ToString::to_string).collect();
        for key in &keys {
            self.remove_device(key);
        }
        self.entities.clear();
        for session in self.sessions.values_mut() {
            session.subscribed_entities.clear();
        }
        for cfg in devices {
            self.add_device(cfg, ctx);
        }
        self.save_devices()
    }

    /// Scan the network, then show the options of a configured device.
    fn request_configure(&self, ws_id: String, cfg: DeviceConfig, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        ctx.spawn(
            async move { discovery::scan_apple_tvs(None).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let devices = result.unwrap_or_else(|e| {
                        warn!("[{ws_id}] discovery failed: {e}");
                        Vec::new()
                    });
                    // The device itself, and devices that are not configured.
                    let found: Vec<Found> = devices
                        .iter()
                        .filter(|d| {
                            cfg.matches(discovery::device_ids(d))
                                || !act
                                    .devices
                                    .values()
                                    .any(|c| c.cfg.matches(discovery::device_ids(d)))
                        })
                        .filter_map(Found::new)
                        .collect();
                    let page = configure_page(&cfg, &found);
                    act.setup = Some(SetupState::Configure {
                        key: cfg.key,
                        found,
                    });
                    act.request_input(&ws_id, page, ctx);
                }),
        );
    }

    /// Find a configured device and pair it again.
    fn pair_again(&self, ws_id: String, cfg: DeviceConfig, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        ctx.spawn(
            async move {
                let found = discovery::find(&cfg).await;
                (cfg, found)
            }
            .into_actor(self)
            .map(move |(cfg, result), act, ctx| {
                if !act.is_setup_flow(flow, &ws_id) {
                    return;
                }
                match result {
                    Ok(Some(device)) => {
                        let Some(mut found) = Found::new(&device) else {
                            act.finish_setup(&ws_id, Some(IntegrationSetupError::NotFound), ctx);
                            return;
                        };
                        // Keep the id from the first pairing.
                        found.id.clone_from(&cfg.id);
                        found.name.clone_from(&cfg.name);
                        if cfg.manual_address
                            && cfg.address.is_some_and(|a| device.addresses.contains(&a))
                        {
                            found.address = cfg.address;
                        }
                        act.pair(
                            ws_id,
                            found,
                            cfg.global_volume,
                            cfg.manual_address,
                            Some(cfg.key),
                            ctx,
                        );
                    }
                    Ok(None) => {
                        act.finish_setup(&ws_id, Some(IntegrationSetupError::NotFound), ctx);
                    }
                    Err(e) => {
                        warn!("[{ws_id}] discovery failed: {e}");
                        act.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
                    }
                }
            }),
        );
    }

    /// Start pairing. The Apple TV shows a PIN.
    fn pair(
        &self,
        ws_id: String,
        found: Found,
        global_volume: bool,
        manual_address: bool,
        replace: Option<String>,
        ctx: &mut Context<Self>,
    ) {
        let endpoints = found.endpoints;
        let target = endpoints.companion.or(endpoints.airplay);
        info!(
            "[{ws_id}] pairing with {} ({}) at {target:?}",
            found.name, found.model
        );
        let mut options = PairOptions::default();
        options.name.clone_from(&self.device_ctx.client_name);
        let flow = self.setup_flow;
        ctx.spawn(
            async move { ratv::pair(&endpoints, &options).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    match result {
                        Ok(prompt) => {
                            let page = pin_page(&found.name);
                            act.setup = Some(SetupState::Pin {
                                prompt: Box::new(prompt),
                                found,
                                global_volume,
                                manual_address,
                                replace,
                            });
                            act.request_input(&ws_id, page, ctx);
                        }
                        Err(e) => {
                            warn!("[{ws_id}] pairing failed: {e}");
                            act.finish_setup(&ws_id, Some(setup_error(&e)), ctx);
                        }
                    }
                }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_pairing(
        &self,
        ws_id: String,
        prompt: Box<PinPrompt>,
        pin: String,
        found: Found,
        global_volume: bool,
        manual_address: bool,
        replace: Option<String>,
        ctx: &mut Context<Self>,
    ) {
        let flow = self.setup_flow;
        ctx.spawn(
            async move { prompt.finish(&pin).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    // The setup was aborted or timed out: the user did not wait for the result.
                    // The pairing is not saved.
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let credentials = match result {
                        Ok(c) => c,
                        Err(e) => {
                            warn!("[{ws_id}] pairing failed: {e}");
                            act.finish_setup(&ws_id, Some(setup_error(&e)), ctx);
                            return;
                        }
                    };
                    info!("[{ws_id}] paired with {}", found.name);
                    let existing = replace
                        .as_deref()
                        .and_then(|k| act.devices.get(k))
                        .map(|d| d.cfg.clone());
                    let cfg = match existing {
                        Some(mut cfg) => {
                            cfg.credentials = credentials;
                            cfg.known_ids = found.known_ids;
                            cfg.address = found.address;
                            cfg
                        }
                        None => DeviceConfig {
                            key: device_key(&found.id),
                            id: found.id,
                            name: found.name,
                            known_ids: found.known_ids,
                            address: found.address,
                            manual_address,
                            credentials,
                            global_volume,
                        },
                    };
                    if let Some(device) = act.devices.get_mut(cfg.key.as_str()) {
                        device.cfg = cfg.clone();
                        device.addr.do_send(UpdateConfig(cfg));
                    } else {
                        act.add_device(cfg, ctx);
                    }
                    let error = act.save_devices().err().map(|e| {
                        warn!("[{ws_id}] saving the configuration failed: {e}");
                        IntegrationSetupError::Other
                    });
                    act.finish_setup(&ws_id, error, ctx);
                }),
        );
    }

    /// Apply the options page of a configured device.
    fn configure(
        &mut self,
        key: &str,
        found: &[Found],
        address: Option<IpAddr>,
        values: &HashMap<String, String>,
    ) -> Result<(), IntegrationSetupError> {
        let device = self
            .devices
            .get_mut(key)
            .ok_or(IntegrationSetupError::NotFound)?;
        let cfg = &mut device.cfg;
        cfg.global_volume = parse_bool(values, "global_volume", cfg.global_volume);
        cfg.manual_address = address.is_some();
        if address.is_some() {
            cfg.address = address;
        }
        // The identifiers can all change, for example after a tvOS update. Link to the chosen device.
        let choice = values.get("device").map(String::as_str).unwrap_or_default();
        if let Some(f) = found.iter().find(|f| f.id == choice) {
            if !cfg.matches(f.known_ids.iter().map(String::as_str)) {
                info!("[{}] linked to {} ({:?})", cfg.name, f.name, f.known_ids);
                cfg.known_ids.clone_from(&f.known_ids);
            }
            // Without a manual address, use the address of the found device.
            if !cfg.manual_address && f.address.is_some() {
                cfg.address = f.address;
            }
        }
        device.addr.do_send(UpdateConfig(cfg.clone()));
        self.save_devices().map_err(|e| {
            warn!("Saving the configuration failed: {e}");
            IntegrationSetupError::Other
        })
    }

    /// Show a setup page.
    fn request_input(&mut self, ws_id: &str, page: Value, ctx: &mut Context<Self>) {
        self.send_input_request(ws_id, page, None, ctx);
    }

    /// Show the page of the current step again, with an error message.
    fn request_input_again(&mut self, ws_id: &str, message: &str, ctx: &mut Context<Self>) {
        let page = match &self.setup {
            Some(SetupState::Discover) => Some(discover_page()),
            Some(SetupState::RestoreChoice) => Some(restore_choice_page(self.driver_settings)),
            Some(SetupState::Restore) => Some(restore_page()),
            Some(SetupState::Backup) => backup_json(&self.sorted_configs())
                .as_deref()
                .map(backup_page),
            Some(SetupState::ConfigMode) => Some(self.config_mode_page()),
            Some(SetupState::ChooseDevice { found, .. }) => Some(choose_device_page(found)),
            Some(SetupState::Pin { found, .. }) => Some(pin_page(&found.name)),
            Some(SetupState::Configure { key, found }) => self
                .devices
                .get(key.as_str())
                .map(|d| configure_page(&d.cfg, found)),
            None => None,
        };
        match page {
            Some(page) => self.send_input_request(ws_id, page, Some(message), ctx),
            None => self.finish_setup(ws_id, Some(IntegrationSetupError::Other), ctx),
        }
    }

    /// Send a setup page. With `error`, the page is shown again because of invalid input.
    fn send_input_request(
        &mut self,
        ws_id: &str,
        page: Value,
        error: Option<&str>,
        ctx: &mut Context<Self>,
    ) {
        if self
            .sm_consume(ws_id, &operation_mode::Input::RequestUserInput, ctx)
            .is_err()
        {
            return;
        }
        let mut event = json!({
            "event_type": SetupChangeEventType::Setup,
            "state": IntegrationSetupState::WaitUserAction,
            "require_user_action": { "input": page },
        });
        if let Some(message) = error {
            event["error"] = json!(IntegrationSetupError::InvalidInput);
            event["error_message"] = json!({ "en": message });
        }
        self.send_r2_msg(
            WsMessage::event("driver_setup_change", EventCategory::Device, event),
            ws_id,
        );
    }

    /// End the setup flow.
    fn finish_setup(
        &mut self,
        ws_id: &str,
        error: Option<IntegrationSetupError>,
        ctx: &mut Context<Self>,
    ) {
        self.end_setup_flow();
        let input = if error.is_none() {
            operation_mode::Input::Successful
        } else {
            operation_mode::Input::SetupError
        };
        if self.sm_consume(ws_id, &input, ctx).is_err() {
            return;
        }
        if let Some(session) = self.sessions.get_mut(ws_id) {
            session.reconfiguring = None;
        }
        // Configured devices keep working after a failed setup.
        if error.is_some() && !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        if matches!(self.machine.state(), operation_mode::State::Running) {
            self.connect_devices();
        }
        let state = if error.is_none() {
            IntegrationSetupState::Ok
        } else {
            IntegrationSetupState::Error
        };
        info!("[{ws_id}] setup flow finished: {state:?}");
        match serde_json::to_value(DriverSetupChange {
            event_type: SetupChangeEventType::Stop,
            state,
            error,
            error_message: None,
            require_user_action: None,
        }) {
            Ok(event) => self.send_r2_msg(
                WsMessage::event("driver_setup_change", EventCategory::Device, event),
                ws_id,
            ),
            Err(e) => warn!("driver_setup_change serialize error: {e}"),
        }
    }

    fn config_mode_page(&self) -> Value {
        let mut devices: Vec<_> = self.devices.values().map(|d| &d.cfg).collect();
        devices.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let mut items: Vec<Value> = devices
            .iter()
            .map(|c| json!({ "id": c.key, "label": { "en": format!("{} ({})", c.name, c.id) } }))
            .collect();
        let mut actions = vec![json!({ "id": "add", "label": { "en": "Add a new device" } })];
        if items.is_empty() {
            items.push(json!({ "id": "", "label": { "en": "---" } }));
        } else {
            actions.extend([
                json!({ "id": "remove", "label": { "en": "Delete selected device" } }),
                json!({ "id": "configure", "label": { "en": "Configure selected device" } }),
                json!({ "id": "pair", "label": { "en": "Pair selected device again" } }),
                json!({ "id": "reset", "label": { "en": "Reset configuration and reconfigure" } }),
                json!({ "id": "backup", "label": { "en": "Backup configuration" } }),
            ]);
        }
        actions.extend([
            json!({ "id": "restore", "label": { "en": "Restore configuration from backup" } }),
            json!({ "id": "settings", "label": { "en": "Save settings" } }),
        ]);
        let first = devices.first().map(|c| c.key.as_str()).unwrap_or_default();
        json!({
            "title": { "en": "Configuration mode" },
            "settings": [
                {
                    "id": "choice",
                    "label": { "en": "Configured devices" },
                    "field": { "dropdown": { "value": first, "items": items } }
                },
                {
                    "id": "action",
                    "label": { "en": "Action" },
                    "field": { "dropdown": { "value": "add", "items": actions } }
                },
                debug_log_field(self.driver_settings.debug_log),
                siri_mode_field(self.driver_settings.siri_mode)
            ]
        })
    }
}

/// Shown for invalid restore data. It must not contain the data.
const RESTORE_FAILED: &str = "Restore failed or invalid backup data. Please try again.";

fn restore_choice_page(settings: DriverSettings) -> Value {
    json!({
        "title": { "en": "Restore" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Integration upgrade" },
                "field": { "label": { "value": { "en": "Do you have a configuration backup? Restore it now. Otherwise, continue to add a new device.\n\nYou can make a backup later: run the setup again and choose _Backup configuration_." } } }
            },
            {
                "id": "restore_from_backup",
                "label": { "en": "Restore from backup" },
                "field": { "checkbox": { "value": false } }
            },
            debug_log_field(settings.debug_log),
            siri_mode_field(settings.siri_mode)
        ]
    })
}

fn restore_page() -> Value {
    json!({
        "title": { "en": "Restore configuration" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Restore configuration" },
                "field": { "label": { "value": { "en": "Paste the backup below. It replaces all configured devices." } } }
            },
            {
                "id": "restore_data",
                "label": { "en": "Backup data" },
                "field": { "password": { "value": "" } }
            }
        ]
    })
}

fn backup_page(text: &str) -> Value {
    json!({
        "title": { "en": "Backup configuration" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Backup" },
                "field": { "label": { "value": { "en": "Copy the data below and keep it in a safe place. It contains the pairing credentials of your Apple TV devices. Do not share it." } } }
            },
            {
                "id": "backup_data",
                "label": { "en": "Backup data" },
                "field": { "textarea": { "value": text } }
            }
        ]
    })
}

fn discover_page() -> Value {
    json!({
        "title": { "en": "Setup mode" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Discover or connect to Apple TV device" },
                "field": { "label": { "value": { "en": "Leave blank to use auto-discovery and click _Next_.\n\nThe device must be on the same network as the remote." } } }
            },
            {
                "id": "address",
                "label": { "en": "IP address (same network only)" },
                "field": { "text": { "value": "" } }
            }
        ]
    })
}

fn debug_log_field(value: bool) -> Value {
    json!({
        "id": "debug_log",
        "label": { "en": "Debug log (for error reports, more log lines)" },
        "field": { "checkbox": { "value": value } }
    })
}

fn siri_mode_field(value: SiriMode) -> Value {
    let items: Vec<Value> = SiriMode::ALL
        .iter()
        .map(|t| json!({ "id": t.id(), "label": { "en": t.label() } }))
        .collect();
    json!({
        "id": "siri_mode",
        "label": { "en": "Siri mode (test)" },
        "field": { "dropdown": { "value": value.id(), "items": items } }
    })
}

fn global_volume_field(value: bool) -> Value {
    json!({
        "id": "global_volume",
        "label": { "en": "Change volume on all connected devices" },
        "field": { "checkbox": { "value": value } }
    })
}

fn device_label(f: &Found) -> String {
    f.address
        .map_or_else(|| f.name.clone(), |a| format!("{} ({a})", f.name))
}

fn choose_device_page(found: &[Found]) -> Value {
    let items: Vec<Value> = found
        .iter()
        .map(|f| json!({ "id": f.id, "label": { "en": device_label(f) } }))
        .collect();
    json!({
        "title": { "en": "Please choose your Apple TV" },
        "settings": [
            {
                "id": "choice",
                "label": { "en": "Choose your Apple TV" },
                "field": { "dropdown": { "value": found.first().map(|f| f.id.as_str()).unwrap_or_default(), "items": items } }
            },
            global_volume_field(true)
        ]
    })
}

fn pin_page(name: &str) -> Value {
    json!({
        "title": { "en": "Enter the PIN" },
        "settings": [
            {
                "id": "pin",
                "label": { "en": format!("Enter the PIN shown on {name}") },
                "field": { "text": { "value": "", "regex": "^[0-9]{4}$" } }
            }
        ]
    })
}

fn configure_page(cfg: &DeviceConfig, found: &[Found]) -> Value {
    let address = if cfg.manual_address {
        cfg.address.map(|a| a.to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let current = found
        .iter()
        .find(|f| cfg.matches(f.known_ids.iter().map(String::as_str)))
        .map_or("", |f| f.id.as_str());
    let mut items: Vec<Value> = found
        .iter()
        .map(
            |f| json!({ "id": f.id, "label": { "en": format!("{} ({})", device_label(f), f.id) } }),
        )
        .collect();
    items.push(json!({ "id": "", "label": { "en": "Not found: keep the current identifiers" } }));
    json!({
        "title": { "en": "Configure your Apple TV" },
        "settings": [
            {
                "id": "device",
                "label": { "en": "Apple TV on the network" },
                "field": { "dropdown": { "value": current, "items": items } }
            },
            {
                "id": "address",
                "label": { "en": "IP address (optional)" },
                "field": { "text": { "value": address } }
            },
            global_volume_field(cfg.global_volume)
        ]
    })
}

fn parse_address(value: Option<&str>) -> Result<Option<IpAddr>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| format!("Invalid IP address: {v}")),
    }
}

fn parse_bool(values: &HashMap<String, String>, key: &str, default: bool) -> bool {
    values.get(key).map_or(default, |v| v == "true")
}

fn setup_error(e: &ratv::Error) -> IntegrationSetupError {
    match e {
        e if e.is_auth() => IntegrationSetupError::AuthorizationError,
        ratv::Error::PairingStep { source, .. } => setup_error(source),
        ratv::Error::Hap(_) => IntegrationSetupError::AuthorizationError,
        ratv::Error::Timeout | ratv::Error::NoAnswer(_) => IntegrationSetupError::Timeout,
        ratv::Error::Io(_) | ratv::Error::NoEndpoints => IntegrationSetupError::ConnectionRefused,
        _ => IntegrationSetupError::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(None, parse_address(None).expect("ok"));
        assert_eq!(None, parse_address(Some(" ")).expect("ok"));
        assert_eq!(
            Some(IpAddr::from([10, 0, 0, 2])),
            parse_address(Some("10.0.0.2")).expect("ok")
        );
        assert!(parse_address(Some("tv.local")).is_err());
    }

    mod flow {
        use crate::config_store::DriverSettings;
        use crate::configuration::{HeartbeatSettings, Settings};
        use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
        use crate::controller::{Controller, NewR2Session, R2SessionDisconnect, SendWsMessage};
        use crate::server::ArtworkStore;
        use actix::{Actor, Addr, Context, Handler};
        use serde_json::{Value, json};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use uc_api::intg::IntegrationSetup;

        /// Collects the events sent to a Remote.
        struct Remote(Arc<Mutex<Vec<Value>>>);

        impl Actor for Remote {
            type Context = Context<Self>;
        }

        impl Handler<SendWsMessage> for Remote {
            type Result = ();
            fn handle(&mut self, msg: SendWsMessage, _: &mut Context<Self>) {
                if let (Some("driver_setup_change"), Some(data)) =
                    (msg.0.msg.as_deref(), msg.0.msg_data)
                    && let Ok(mut events) = self.0.lock()
                {
                    events.push(data);
                }
            }
        }

        fn controller(name: &str) -> Addr<Controller> {
            let dir =
                std::env::temp_dir().join(format!("uc-intg-appletv-{name}-{}", std::process::id()));
            let settings = Settings {
                interface: "127.0.0.1".into(),
                http_port: 0,
                config_home: dir.clone(),
                client_name: "test".into(),
                heartbeat: HeartbeatSettings::default(),
            };
            let metadata = serde_json::from_value(json!({})).expect("metadata");
            Controller::new(
                settings,
                DriverSettings::default(),
                metadata,
                ArtworkStore::new(dir.join("artwork")),
            )
            .start()
        }

        async fn connect(controller: &Addr<Controller>, id: &str) -> Arc<Mutex<Vec<Value>>> {
            let events = Arc::default();
            let addr = Remote(Arc::clone(&events)).start();
            controller
                .send(NewR2Session {
                    addr: addr.recipient(),
                    id: id.into(),
                })
                .await
                .expect("send");
            events
        }

        fn input(ws_id: &str, address: &str) -> SetDriverUserDataMsg {
            values(ws_id, &[("address", address)])
        }

        fn values(ws_id: &str, values: &[(&str, &str)]) -> SetDriverUserDataMsg {
            let values = values
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect();
            SetDriverUserDataMsg {
                ws_id: ws_id.into(),
                data: IntegrationSetup::InputValues(values),
            }
        }

        fn last(events: &Mutex<Vec<Value>>) -> Value {
            events
                .lock()
                .expect("lock")
                .last()
                .cloned()
                .unwrap_or_default()
        }

        #[actix::test]
        async fn invalid_input_shows_the_page_again() {
            let controller = controller("invalid-input");
            let events = connect(&controller, "a").await;
            connect(&controller, "b").await;
            let data = serde_json::from_value(json!({ "setup_data": {} })).expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "a".into(),
                    data,
                })
                .await
                .expect("send")
                .expect("setup started");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(json!("WAIT_USER_ACTION"), last(&events)["state"]);
            assert_eq!(
                json!("restore_from_backup"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            // Another Remote cannot send input.
            assert!(
                controller
                    .send(input("b", ""))
                    .await
                    .expect("send")
                    .is_err()
            );

            // Continue without a restore.
            controller
                .send(values("a", &[("restore_from_backup", "false")]))
                .await
                .expect("send")
                .expect("ok");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                json!("address"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            controller
                .send(input("a", "no-ip"))
                .await
                .expect("send")
                .expect("ok");
            tokio::time::sleep(Duration::from_millis(300)).await;
            let event = last(&events);
            assert_eq!(json!("WAIT_USER_ACTION"), event["state"]);
            assert_eq!(json!("INVALID_INPUT"), event["error"]);
            assert!(event["require_user_action"]["input"].is_object());

            // The setup still waits for input.
            controller
                .send(input("a", "also-no-ip"))
                .await
                .expect("send")
                .expect("ok");

            // A disconnect of the owner ends the setup, so another Remote can start one.
            controller
                .send(R2SessionDisconnect { id: "a".into() })
                .await
                .expect("send");
            let data = serde_json::from_value(json!({ "setup_data": {} })).expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "b".into(),
                    data,
                })
                .await
                .expect("send")
                .expect("setup started");
        }

        #[actix::test]
        async fn invalid_restore_data_is_rejected_without_echo() {
            let controller = controller("restore");
            let events = connect(&controller, "a").await;
            let data = serde_json::from_value(json!({ "setup_data": {} })).expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "a".into(),
                    data,
                })
                .await
                .expect("send")
                .expect("setup started");
            tokio::time::sleep(Duration::from_millis(300)).await;

            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                json!("restore_data"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            controller
                .send(values("a", &[("restore_data", "secret-not-a-backup")]))
                .await
                .expect("send")
                .expect("ok");
            tokio::time::sleep(Duration::from_millis(300)).await;
            let event = last(&events);
            assert_eq!(json!("INVALID_INPUT"), event["error"]);
            assert!(!event.to_string().contains("secret-not-a-backup"));
            assert_eq!(
                json!("restore_data"),
                event["require_user_action"]["input"]["settings"][1]["id"]
            );
        }
    }
}
