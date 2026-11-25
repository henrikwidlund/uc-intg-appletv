// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Driver setup flow: discovery, pairing and reconfiguration.

use crate::config_store::{
    DeviceConfig, DriverSettings, SaveWait, SiriMode, backup_json, device_key, parse_backup,
};
use crate::controller::devices::{Trigger, sort_configs};
use crate::controller::handler::AbortDriverSetup;
use crate::controller::{Controller, SendWsMessage, operation_mode, response_text};
use crate::device::{DeviceCache, SiriModeChanged, discovery, pairing_not_valid};
use crate::entities::EntityKind;
use crate::errors::{NOT_SENT, ServiceError, not_read};
use crate::logging;
use actix::{ActorFutureExt, AsyncContext, Context, Handler, Message, WrapFuture};
use log::{debug, error, info, warn};
use ratv::discovery::Device;
use ratv::{Credentials, Endpoints, PairOptions, PinPrompt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use uc_api::intg::{DriverSetupChange, IntegrationSetup, SetupDriver};
use uc_api::model::intg::{IntegrationSetupError, IntegrationSetupState, SetupChangeEventType};
use uc_api::ws::{EventCategory, WsMessage};

/// Time between the acknowledgment of a setup request and the next setup event. The order
/// does not depend on it: the acknowledgment goes first through the same queue.
const ACK_DELAY: Duration = Duration::from_millis(100);

/// Start the setup flow.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetupDriverMsg {
    pub ws_id: String,
    pub data: SetupDriver,
    /// Request id and response message. The handler sends the acknowledgment.
    pub ack: Option<(u32, &'static str)>,
}

/// User input of a setup page.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetDriverUserDataMsg {
    pub ws_id: String,
    pub data: IntegrationSetup,
    /// Request id and response message. The handler sends the acknowledgment.
    pub ack: Option<(u32, &'static str)>,
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
            address: device.addresses.first().map(|a| a.ip),
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
        self.send_ack(&msg.ws_id, msg.ack);
        let reconfigure = msg.data.reconfigure == Some(true);
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
            // A first setup replaces the configuration when it ends with a paired Apple TV.
            self.setup_replace = true;
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
        self.send_ack(&ws_id, msg.ack);
        let Some(step) = self.setup.take() else {
            warn!("[{ws_id}] setup input without an active setup step");
            self.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
            return Ok(());
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
    /// Acknowledge a setup request. It goes through the event queue, so it comes before the
    /// setup events that follow.
    fn send_ack(&self, ws_id: &str, ack: Option<(u32, &'static str)>) {
        let Some((req_id, msg)) = ack else { return };
        match response_text(req_id, msg, Value::Null) {
            Ok(text) => self.send_r2_response(ws_id, SendWsMessage::Text(text)),
            Err(e) => error!("[{ws_id}] setup response: {e:?}"),
        }
    }

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
        if let Some(handle) = self.pairing.take() {
            ctx.cancel_future(handle);
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
        if !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
            self.update_connections(Trigger::Change);
        }
    }

    /// Forget the active setup flow. Tasks of the old flow are ignored.
    fn end_setup_flow(&mut self) {
        self.setup = None;
        self.setup_owner = None;
        self.setup_replace = false;
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
                let choice = values.get("choice").map_or_default(String::as_str);
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
                self.configure_device(ws_id, &key, &found, address, values, ctx);
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
        &self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let devices = values
            .get("restore_data")
            .and_then(|text| parse_backup(text))
            .ok_or_else(|| {
                warn!("[{ws_id}] invalid restore data");
                InputError::Invalid(Box::new(SetupState::Restore), RESTORE_FAILED.into())
            })?;
        info!("[{ws_id}] restoring {} device(s)", devices.len());
        let mut devices = devices;
        sort_configs(&mut devices);
        let save = self.store.save_devices(&devices);
        self.apply_after_save(ws_id, save, ctx, move |act, ctx| {
            act.restore_devices(devices, ctx);
        });
        Ok(())
    }

    /// Ask for an optional IP address, then discover.
    fn request_discover(&mut self, ws_id: &str, ctx: &mut Context<Self>) {
        self.setup = Some(SetupState::Discover);
        self.request_input(ws_id, discover_page(), ctx);
    }

    /// Scan for Apple TVs and show the ones that are not configured. A first setup or a reset
    /// shows all.
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
                            act.setup_replace
                                || !act
                                    .devices
                                    .values()
                                    .any(|c| c.cfg.matches(discovery::device_ids(d)))
                        })
                        .filter_map(Found::new)
                        .collect();
                    if found.is_empty() {
                        info!("[{ws_id}] no Apple TV found to add");
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

    /// Check the action of the configuration page, and that it has a device if it needs one.
    fn check_config_action(&self, action: &str, key: &str) -> Result<(), InputError> {
        let invalid = |m: String| Err(InputError::Invalid(Box::new(SetupState::ConfigMode), m));
        match action {
            "settings" | "add" | "restore" | "reset" => Ok(()),
            "remove" | "configure" | "pair" if self.devices.contains_key(key) => Ok(()),
            "remove" | "configure" | "pair" => invalid("Unknown device".into()),
            "backup" if !self.devices.is_empty() => Ok(()),
            "backup" => invalid("Nothing to back up".into()),
            _ => invalid(format!("Unknown action: {action}")),
        }
    }

    fn config_mode(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let flow = self.setup_flow;
        let action = values.get("action").map_or_default(String::as_str);
        let key = values.get("choice").map_or_default(String::as_str);
        // Check the input first: a page that is shown again changes no settings.
        self.check_config_action(action, key)?;
        let backup = if action == "backup" {
            Some(backup_json(&self.sorted_configs()).ok_or_else(|| {
                InputError::Invalid(Box::new(SetupState::ConfigMode), "Backup failed".into())
            })?)
        } else {
            None
        };
        self.set_debug_log(
            &ws_id,
            parse_bool(values, "debug_log", self.driver_settings.debug_log),
        );
        self.set_siri_mode(&ws_id, values);
        // `check_config_action` has checked the action, the device and that devices exist.
        match action {
            "settings" => self.finish_setup_later(ws_id, None, ctx),
            "add" => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            "remove" => {
                let mut configs = self.sorted_configs();
                configs.retain(|c| c.key != key);
                let save = self.store.save_devices(&configs);
                let (key, id) = (key.to_owned(), ws_id.clone());
                self.apply_after_save(ws_id, save, ctx, move |act, _| {
                    if let Some(cfg) = act.forget_device(&key) {
                        info!("[{id}] removed {}", cfg.name);
                    }
                });
            }
            "configure" => {
                let cfg = self.devices[key].cfg.clone();
                self.request_configure(ws_id, cfg, ctx);
            }
            "pair" => {
                let cfg = self.devices[key].cfg.clone();
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.pair_again(ws_id, cfg, ctx);
                    }
                });
            }
            "backup" => {
                let text = backup.unwrap_or_default();
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
                // The devices are replaced when the setup ends with a paired Apple TV.
                self.setup_replace = true;
                info!("[{ws_id}] devices are replaced when an Apple TV is paired");
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Apply the options page of a configured device: write the configuration, then use it.
    fn configure_device(
        &self,
        ws_id: String,
        key: &str,
        found: &[Found],
        address: Option<IpAddr>,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) {
        let mut configs = self.sorted_configs();
        let Some(cfg) = configs.iter_mut().find(|c| c.key == key) else {
            self.finish_setup_later(ws_id, Some(IntegrationSetupError::NotFound), ctx);
            return;
        };
        configure(cfg, found, address, values, false);
        let save = self.store.save_devices(&configs);
        let (key, found, values) = (key.to_owned(), found.to_vec(), values.clone());
        self.apply_after_save(ws_id, save, ctx, move |act, _| {
            // Apply the page again: the device can have changed its address or ids meanwhile.
            if let Some(device) = act.devices.get_mut(key.as_str()) {
                let mut cfg = device.cfg.clone();
                configure(&mut cfg, &found, address, &values, true);
                device.update_config(cfg);
            }
        });
    }

    /// Wait for `save`, the write of the new device list, then change the devices with `apply`
    /// and end the setup. `apply` makes the devices match the written list. If `apply` keeps
    /// values that a device found on the network during the write, the queued write after
    /// `apply` saves them. If the write fails, nothing changes and the setup ends with an error.
    /// `apply` also runs when the setup was aborted during the write, so that memory and disk
    /// stay the same.
    fn apply_after_save<F>(
        &self,
        ws_id: String,
        save: io::Result<SaveWait>,
        ctx: &mut Context<Self>,
        apply: F,
    ) where
        F: FnOnce(&mut Self, &mut Context<Self>) + 'static,
    {
        let flow = self.setup_flow;
        ctx.spawn(
            async move {
                match save {
                    Ok(wait) => wait.wait().await,
                    Err(e) => Err(e),
                }
            }
            .into_actor(self)
            .map(move |result, act, ctx| {
                let error = match result {
                    Ok(()) => {
                        info!("[{ws_id}] configuration saved");
                        apply(act, ctx);
                        // A device can have queued an older list during the write.
                        if let Err(e) = act.queue_devices() {
                            warn!("[{ws_id}] saving the configuration failed: {e}");
                        }
                        // Without the end of the setup, nothing else makes the devices usable.
                        if act.setup_flow != flow {
                            act.devices_changed(&ws_id, ctx);
                        }
                        None
                    }
                    Err(e) => {
                        error!("[{ws_id}] error writing config.json, nothing changed: {e}");
                        Some(IntegrationSetupError::Other)
                    }
                };
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, error, ctx);
                    }
                });
            }),
        );
    }

    /// Use the changed devices after the setup flow has ended: leave the setup state if there are
    /// devices now, and connect the devices that a Remote uses.
    fn devices_changed(&mut self, ws_id: &str, ctx: &mut Context<Self>) {
        if !self.devices.is_empty()
            && matches!(
                self.machine.state(),
                operation_mode::State::RequireSetup | operation_mode::State::SetupError
            )
        {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        if matches!(self.machine.state(), operation_mode::State::Running) {
            self.update_connections(Trigger::Change);
        }
    }

    /// End the setup after the request was acknowledged.
    fn finish_setup_later(
        &self,
        ws_id: String,
        error: Option<IntegrationSetupError>,
        ctx: &mut Context<Self>,
    ) {
        let flow = self.setup_flow;
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if act.is_setup_flow(flow, &ws_id) {
                act.finish_setup(&ws_id, error, ctx);
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

    /// Set the Siri mode from the setup page and save it.
    fn set_siri_mode(&mut self, ws_id: &str, values: &HashMap<String, String>) {
        let Some(mode) = values.get("siri_mode").and_then(|v| SiriMode::from_id(v)) else {
            return;
        };
        if self.driver_settings.siri_mode == mode {
            return;
        }
        self.driver_settings.siri_mode = mode;
        self.device_ctx.set_siri_mode(mode);
        for device in self.devices.values() {
            device.addr.do_send(SiriModeChanged);
        }
        info!("[{ws_id}] Siri mode: {}", mode.label());
        if let Err(e) = self.store.save_settings(self.driver_settings) {
            warn!("[{ws_id}] saving the settings failed: {e}");
        }
    }

    /// Remove a device, and its changes that a Remote did not get yet. The subscriptions stay
    /// as the Remotes sent them. The configuration is not saved.
    fn forget_device(&mut self, key: &str) -> Option<DeviceConfig> {
        let cfg = self.remove_device(key)?;
        let other = |id: &String| EntityKind::parse(id).is_none_or(|(k, _)| k != key);
        for session in self.sessions.values_mut() {
            session.stale.retain(other);
        }
        Some(cfg)
    }

    /// Remove all devices except `keep`. The configuration is not saved.
    fn forget_other_devices(&mut self, keep: &str) {
        let keys: Vec<Arc<str>> = self
            .devices
            .keys()
            .filter(|k| k.as_ref() != keep)
            .cloned()
            .collect();
        for key in &keys {
            self.forget_device(key);
        }
    }

    /// Replace all devices with the devices of a backup. The configuration is not saved.
    /// The subscriptions stay as the Remotes sent them.
    fn restore_devices(&mut self, devices: Vec<DeviceConfig>, ctx: &Context<Self>) {
        let keys: Vec<Arc<str>> = self.devices.keys().cloned().collect();
        for key in &keys {
            // A kept key keeps its stale entries: the new actor sends the state again.
            if devices.iter().any(|d| d.key == **key) {
                self.remove_device(key);
            } else {
                self.forget_device(key);
            }
        }
        self.entities.clear();
        // The old actors delete their cache files when they stop. Start with empty lists.
        for cfg in devices {
            self.add_device(cfg, DeviceCache::default(), ctx);
        }
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
                            && cfg
                                .address
                                .is_some_and(|a| device.addresses.iter().any(|f| f.ip == a))
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
        &mut self,
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
        let handle = ctx.spawn(
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
                            log_pairing_failure(&ws_id, &e);
                            act.finish_setup(&ws_id, Some(setup_error(&e)), ctx);
                        }
                    }
                }),
        );
        if let Some(old) = self.pairing.replace(handle) {
            ctx.cancel_future(old);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_pairing(
        &mut self,
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
        let handle = ctx.spawn(
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
                            log_pairing_failure(&ws_id, &e);
                            act.finish_setup(&ws_id, Some(setup_error(&e)), ctx);
                            return;
                        }
                    };
                    act.save_pairing(
                        ws_id,
                        credentials,
                        found,
                        global_volume,
                        manual_address,
                        replace,
                        ctx,
                    );
                }),
        );
        if let Some(old) = self.pairing.replace(handle) {
            ctx.cancel_future(old);
        }
    }

    /// Save the new pairing, then use it. `replace` is the key of the device that is paired
    /// again. A first setup or a reset removes the other devices.
    #[allow(clippy::too_many_arguments)]
    fn save_pairing(
        &self,
        ws_id: String,
        credentials: Credentials,
        found: Found,
        global_volume: bool,
        manual_address: bool,
        replace: Option<String>,
        ctx: &mut Context<Self>,
    ) {
        info!("[{ws_id}] paired with {}", found.name);
        let existing = match replace.as_deref() {
            Some(k) => self.devices.get(k).map(|d| &d.cfg),
            // A first setup or a reset keeps the key, id and name of a matching
            // configured device.
            None if self.setup_replace => {
                matching_config(self.devices.values().map(|d| &d.cfg), &found)
            }
            None => None,
        }
        .cloned();
        let cfg = match existing {
            Some(mut cfg) => {
                cfg.credentials = credentials;
                cfg.known_ids = found.known_ids;
                cfg.address = found.address;
                cfg.manual_address = manual_address;
                cfg.global_volume = global_volume;
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
        let replace_all = self.setup_replace;
        let mut configs = if replace_all {
            Vec::new()
        } else {
            self.sorted_configs()
        };
        configs.retain(|c| c.key != cfg.key);
        configs.push(cfg.clone());
        sort_configs(&mut configs);
        let save = self.store.save_devices(&configs);
        self.apply_after_save(ws_id, save, ctx, move |act, ctx| {
            if replace_all {
                act.forget_other_devices(&cfg.key);
            }
            if let Some(device) = act.devices.get_mut(cfg.key.as_str()) {
                device.update_config(cfg);
            } else {
                // A new device starts with empty lists.
                act.add_device(cfg, DeviceCache::default(), ctx);
            }
        });
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
            ws_id,
            WsMessage::event("driver_setup_change", EventCategory::Device, event),
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
        // Configured devices keep working after a failed setup.
        if error.is_some() && !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        if matches!(self.machine.state(), operation_mode::State::Running) {
            self.update_connections(Trigger::Change);
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
                ws_id,
                WsMessage::event("driver_setup_change", EventCategory::Device, event),
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
        let first = devices.first().map_or_default(|c| c.key.as_str());
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

/// Apply the options page to the configuration of a device. `log` logs a new link.
fn configure(
    cfg: &mut DeviceConfig,
    found: &[Found],
    address: Option<IpAddr>,
    values: &HashMap<String, String>,
    log: bool,
) {
    cfg.global_volume = parse_bool(values, "global_volume", cfg.global_volume);
    cfg.manual_address = address.is_some();
    if address.is_some() {
        cfg.address = address;
    }
    // The identifiers can all change, for example after a tvOS update. Link to the chosen device.
    let choice = values.get("device").map_or_default(String::as_str);
    if let Some(f) = found.iter().find(|f| f.id == choice) {
        if !cfg.matches(f.known_ids.iter().map(String::as_str)) {
            if log {
                info!("[{}] linked to {} ({:?})", cfg.name, f.name, f.known_ids);
            }
            cfg.known_ids.clone_from(&f.known_ids);
        }
        // Without a manual address, use the address of the found device.
        if !cfg.manual_address && f.address.is_some() {
            cfg.address = f.address;
        }
    }
}

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
                "field": { "label": { "value": { "en": "Leave blank to use auto-discovery and click _Next_.\n\nAuto-discovery finds devices on the same network as the remote. Enter the IP address for a device on another network." } } }
            },
            {
                "id": "address",
                "label": { "en": "IP address (optional)" },
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
        "label": { "en": "Siri protocol (change only to find errors)" },
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
                "field": { "dropdown": { "value": found.first().map_or_default(|f| f.id.as_str()), "items": items } }
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
        cfg.address.map_or_default(|a| a.to_string())
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

/// The configuration that belongs to `found`. A match of the pairing id comes first, then the
/// match with the smallest key.
fn matching_config<'a>(
    configs: impl Iterator<Item = &'a DeviceConfig>,
    found: &Found,
) -> Option<&'a DeviceConfig> {
    let mut first = None;
    for cfg in configs {
        if cfg.id == found.id {
            return Some(cfg);
        }
        if first.is_none_or(|f: &DeviceConfig| cfg.key < f.key)
            && cfg.matches(found.known_ids.iter().map(String::as_str))
        {
            first = Some(cfg);
        }
    }
    first
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

fn log_pairing_failure(ws_id: &str, e: &ratv::Error) {
    if not_read(pairing_cause(e)) {
        warn!("[{ws_id}] pairing failed: {NOT_SENT} ({e})");
    } else {
        warn!("[{ws_id}] pairing failed: {e}");
    }
}

/// The error inside [`ratv::Error::PairingStep`], else `e`.
fn pairing_cause(e: &ratv::Error) -> &ratv::Error {
    match e {
        ratv::Error::PairingStep { source, .. } => pairing_cause(source),
        e => e,
    }
}

fn setup_error(e: &ratv::Error) -> IntegrationSetupError {
    match e {
        e if pairing_not_valid(e) => IntegrationSetupError::AuthorizationError,
        ratv::Error::PairingStep { source, .. } => setup_error(source),
        ratv::Error::Hap(_) => IntegrationSetupError::AuthorizationError,
        ratv::Error::Timeout | ratv::Error::NoAnswer(_) => IntegrationSetupError::Timeout,
        // Not sent: like the `ServiceUnavailable` of a command, which has no setup error code.
        e if not_read(e) => IntegrationSetupError::Other,
        ratv::Error::Io(_) | ratv::Error::NoEndpoints => IntegrationSetupError::ConnectionRefused,
        _ => IntegrationSetupError::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_error_timeouts_and_not_read() {
        for e in [ratv::Error::Timeout, ratv::Error::NoAnswer("x")] {
            assert_eq!(IntegrationSetupError::Timeout, setup_error(&e), "{e}");
        }
        for e in [ratv::Error::SendTimeout("x"), ratv::Error::QueueFull] {
            assert_eq!(IntegrationSetupError::Other, setup_error(&e), "{e}");
        }
        let step = ratv::Error::PairingStep {
            step: "pair-setup M2",
            skipped: Vec::new(),
            source: Box::new(ratv::Error::SendTimeout("x")),
        };
        assert!(not_read(pairing_cause(&step)), "same cause as setup_error");
        assert_eq!(IntegrationSetupError::Other, setup_error(&step));
    }

    #[test]
    fn addresses() {
        assert_eq!(None, parse_address(None).expect("ok"));
        assert_eq!(None, parse_address(Some(" ")).expect("ok"));
        assert_eq!(
            Some(IpAddr::from([192, 0, 2, 2])),
            parse_address(Some("192.0.2.2")).expect("ok")
        );
        assert!(parse_address(Some("tv.local")).is_err());
    }

    #[test]
    fn matching_config_prefers_the_pairing_id() {
        let exact = crate::config_store::tests::device();
        let mut linked = exact.clone();
        linked.id = "11:22:33:44:55:66".into();
        linked.key = device_key(&linked.id);
        let mut other = exact.clone();
        other.id = "99:99:99:99:99:99".into();
        other.known_ids = vec!["OTHER".into()];
        let found = Found {
            id: exact.id.clone(),
            name: "Living Room".into(),
            model: String::new(),
            address: None,
            endpoints: Endpoints::default(),
            known_ids: vec![exact.id.clone(), "UUID-1".into()],
        };
        let configs = [linked.clone(), other.clone(), exact.clone()];
        assert_eq!(Some(&exact), matching_config(configs.iter(), &found));
        let configs = [other.clone(), linked.clone()];
        assert_eq!(Some(&linked), matching_config(configs.iter(), &found));
        let mut linked2 = linked.clone();
        linked2.id = "00:11:22:33:44:55".into();
        linked2.key = device_key(&linked2.id);
        let configs = [linked.clone(), linked2.clone()];
        assert_eq!(Some(&linked2), matching_config(configs.iter(), &found));
        let configs = [linked2.clone(), linked];
        assert_eq!(Some(&linked2), matching_config(configs.iter(), &found));
        assert_eq!(None, matching_config(std::iter::once(&other), &found));
    }

    mod flow {
        use crate::config_store::tests::device;
        use crate::config_store::{ConfigStore, DeviceConfig, backup_json, device_key};
        use crate::controller::handler::AbortDriverSetup;
        use crate::controller::handler::setup::{Found, SetupState};
        use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
        use crate::controller::operation_mode::{self, State};
        use crate::controller::test_util::{
            DeviceAddr, FlushStore, connect, controller, settle, test_dir, wait_stopped,
        };
        use crate::controller::{Controller, R2RequestMsg, R2SessionDisconnect};
        use crate::device::DeviceEvent;
        use crate::entities::EntityKind;
        use actix::{Addr, AsyncContext, Context, Handler, Message, MessageResult, WrapFuture};
        use ratv::Endpoints;
        use serde_json::{Value, json};
        use std::net::IpAddr;
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;
        use uc_api::intg::IntegrationSetup;
        use uc_api::intg::ws::R2Request;

        /// Returns the attributes that the controller keeps for an entity.
        #[derive(Message)]
        #[rtype(result = "Option<Value>")]
        struct EntityAttrs(String);

        impl Handler<EntityAttrs> for Controller {
            type Result = Option<Value>;
            fn handle(&mut self, msg: EntityAttrs, _: &mut Context<Self>) -> Option<Value> {
                self.entities.get(&msg.0).cloned().map(Value::Object)
            }
        }

        /// Returns `true` if Remote `.0` subscribed entity `.1`.
        #[derive(Message)]
        #[rtype(result = "bool")]
        struct Subscribed(&'static str, String);

        impl Handler<Subscribed> for Controller {
            type Result = bool;
            fn handle(&mut self, msg: Subscribed, _: &mut Context<Self>) -> bool {
                self.sessions.get(msg.0).is_some_and(|s| s.wants(&msg.1))
            }
        }

        /// Returns the Siri mode id of the driver settings.
        #[derive(Message)]
        #[rtype(result = "String")]
        struct SiriModeId;

        impl Handler<SiriModeId> for Controller {
            type Result = String;
            fn handle(&mut self, _: SiriModeId, _: &mut Context<Self>) -> String {
                self.driver_settings.siri_mode.id().into()
            }
        }

        #[actix::test]
        async fn invalid_config_action_changes_no_settings() {
            let controller = controller("config-action");
            let events = connect(&controller, "a").await;
            start_setup(&controller, &events, "a", true).await;
            let before = controller.send(SiriModeId).await.expect("send");
            let other = if before == "mrp" { "auto" } else { "mrp" };
            let before_pages = setup_events(&events);
            controller
                .send(values(
                    "a",
                    &[
                        ("action", "remove"),
                        ("choice", "none"),
                        ("siri_mode", other),
                    ],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_page(&events, before_pages).await;
            assert_eq!(
                json!("Unknown device"),
                last(&events)["error_message"]["en"]
            );
            assert_eq!(before, controller.send(SiriModeId).await.expect("send"));
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
                ack: None,
            }
        }

        /// `msg_data` of the last `driver_setup_change` event.
        fn last(events: &Mutex<Vec<Value>>) -> Value {
            events
                .lock()
                .expect("lock")
                .iter()
                .rfind(|m| m["msg"] == "driver_setup_change")
                .map_or_default(|m| m["msg_data"].clone())
        }

        #[actix::test]
        async fn setup_ack_comes_before_the_setup_events() {
            let controller = controller("setup-ack");
            let events = connect(&controller, "a").await;
            let data = serde_json::from_value(json!({ "setup_data": {} })).expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "a".into(),
                    data,
                    ack: Some((5, "result")),
                })
                .await
                .expect("send")
                .expect("setup started");
            tokio::time::sleep(Duration::from_millis(300)).await;
            let messages = events.lock().expect("lock");
            let ack = messages
                .iter()
                .position(|m| m["req_id"] == 5 && m["code"] == 200);
            let event = messages
                .iter()
                .position(|m| m["msg"] == "driver_setup_change");
            assert!(ack.is_some() && event.is_some());
            assert!(ack < event, "ack first");
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
                    ack: None,
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
                    ack: None,
                })
                .await
                .expect("send")
                .expect("setup started");
        }

        fn config_len(name: &str) -> usize {
            ConfigStore::new(test_dir(name)).load_devices().len()
        }

        /// Create an empty test directory with a configuration of `cfg`.
        async fn config_dir(name: &str, cfg: &DeviceConfig) -> std::path::PathBuf {
            let dir = test_dir(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            ConfigStore::new(&dir)
                .save_devices(std::slice::from_ref(cfg))
                .expect("save")
                .wait()
                .await
                .expect("write");
            dir
        }

        /// Start a setup and wait for its first page.
        async fn start_setup(
            controller: &Addr<Controller>,
            events: &Mutex<Vec<Value>>,
            ws_id: &str,
            reconfigure: bool,
        ) {
            let before = setup_events(events);
            let data =
                serde_json::from_value(json!({ "reconfigure": reconfigure, "setup_data": {} }))
                    .expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: ws_id.into(),
                    data,
                    ack: None,
                })
                .await
                .expect("send")
                .expect("setup started");
            wait_page(events, before).await;
        }

        /// Number of `driver_setup_change` events.
        fn setup_events(events: &Mutex<Vec<Value>>) -> usize {
            events
                .lock()
                .expect("lock")
                .iter()
                .filter(|m| m["msg"] == "driver_setup_change")
                .count()
        }

        /// Wait for a new setup page after `before` setup events.
        async fn wait_page(events: &Mutex<Vec<Value>>, before: usize) {
            for _ in 0..100 {
                if setup_events(events) > before && last(events)["state"] == "WAIT_USER_ACTION" {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("no new setup page");
        }

        /// Wait for the event that ends the setup and return its data.
        async fn wait_stop(events: &Mutex<Vec<Value>>) -> Value {
            for _ in 0..100 {
                let event = last(events);
                if event["event_type"] == "STOP" {
                    return event;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("the setup did not end");
        }

        /// Wait until the controller keeps the media player, and return its `source_list`.
        async fn source_list(controller: &Addr<Controller>, key: &str) -> Value {
            let id = EntityKind::MediaPlayer.entity_id(key);
            for _ in 0..100 {
                if let Some(attrs) = controller
                    .send(EntityAttrs(id.clone()))
                    .await
                    .expect("send")
                {
                    return attrs["source_list"].clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("no media player entity");
        }

        /// A directory with the name of the temporary file makes each write fail.
        fn break_writes(dir: &Path) {
            std::fs::create_dir(dir.join("config.json.tmp")).expect("dir");
        }

        #[actix::test]
        async fn failing_save_ends_the_setup_with_an_error() {
            let name = "save-error";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let controller = controller(name);
            let events = connect(&controller, "a").await;

            start_setup(&controller, &events, "a", true).await;
            break_writes(&dir);
            // The request is acknowledged. The setup ends after the write.
            controller
                .send(values("a", &[("action", "remove"), ("choice", &cfg.key)]))
                .await
                .expect("send")
                .expect("ok");
            let event = wait_stop(&events).await;
            assert_eq!(json!("ERROR"), event["state"]);
            assert_eq!(json!("OTHER"), event["error"]);
            // Nothing changed: the device is still configured.
            let addr = controller.send(DeviceAddr(cfg.key.clone()));
            assert!(addr.await.expect("send").is_some());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn failing_restore_write_changes_nothing() {
            let name = "restore-error";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            let mut other = device();
            other.id = "02:00:00:00:00:01".into();
            other.key = device_key(&other.id);
            let backup = backup_json(std::slice::from_ref(&other)).expect("backup");

            start_setup(&controller, &events, "a", false).await;
            let before = setup_events(&events);
            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            wait_page(&events, before).await;
            break_writes(&dir);
            controller
                .send(values("a", &[("restore_data", &backup)]))
                .await
                .expect("send")
                .expect("ok");
            let event = wait_stop(&events).await;
            assert_eq!(json!("ERROR"), event["state"]);
            let addr = |key: &str| controller.send(DeviceAddr(key.into()));
            assert!(addr(&cfg.key).await.expect("send").is_some(), "kept");
            assert!(addr(&other.key).await.expect("send").is_none(), "not added");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn startup_loads_the_cache_and_restore_starts_empty() {
            let name = "load-cache";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let cache = dir.join(format!("cache_{}.json", cfg.key));
            std::fs::write(
                &cache,
                r#"{"apps":[{"bundle_id":"com.example.app","name":"Cached App"}],"airplay_devices":[]}"#,
            )
            .expect("cache");
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            assert_eq!(
                json!(["Cached App"]),
                source_list(&controller, &cfg.key).await
            );

            // A restore removes the cache file and does not use it. The subscriptions stay.
            let entity_id = EntityKind::MediaPlayer.entity_id(&cfg.key);
            controller
                .send(R2RequestMsg {
                    ws_id: "a".into(),
                    req_id: 1,
                    request: R2Request::SubscribeEvents,
                    msg_data: Some(json!({ "entity_ids": [entity_id] })),
                })
                .await
                .expect("send")
                .expect("ok");
            let subscribed = || controller.send(Subscribed("a", entity_id.clone()));
            assert!(subscribed().await.expect("send"));
            let old = controller
                .send(DeviceAddr(cfg.key.clone()))
                .await
                .expect("send")
                .expect("actor");
            start_setup(&controller, &events, "a", false).await;
            let before = setup_events(&events);
            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            wait_page(&events, before).await;
            let backup = backup_json(std::slice::from_ref(&cfg)).expect("backup");
            controller
                .send(values("a", &[("restore_data", &backup)]))
                .await
                .expect("send")
                .expect("ok");
            let event = wait_stop(&events).await;
            assert_eq!(json!("OK"), event["state"]);
            assert_eq!(json!([]), source_list(&controller, &cfg.key).await);
            assert!(subscribed().await.expect("send"));
            // The old actor deletes the file when it stops.
            wait_stopped(&controller, &old).await;
            assert!(!cache.exists());
            assert_eq!(1, config_len(name));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn first_setup_and_reset_keep_the_devices_until_a_device_is_paired() {
            let name = "keep-devices";
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
            let events = connect(&controller, "a").await;

            // A first setup that is aborted keeps the device.
            let data = serde_json::from_value(json!({ "setup_data": {} })).expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "a".into(),
                    data,
                    ack: None,
                })
                .await
                .expect("send")
                .expect("setup started");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(json!("WAIT_USER_ACTION"), last(&events)["state"]);
            assert_eq!(1, config_len(name));
            controller
                .send(AbortDriverSetup {
                    ws_id: "a".into(),
                    timeout: false,
                })
                .await
                .expect("send");
            assert_eq!(1, config_len(name));

            // A reset keeps the device until a device is paired.
            let data = serde_json::from_value(json!({ "reconfigure": true, "setup_data": {} }))
                .expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "a".into(),
                    data,
                    ack: None,
                })
                .await
                .expect("send")
                .expect("setup started");
            tokio::time::sleep(Duration::from_millis(300)).await;
            controller
                .send(values("a", &[("action", "reset"), ("choice", &cfg.key)]))
                .await
                .expect("send")
                .expect("ok");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                json!("address"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );
            assert_eq!(1, config_len(name));
            controller
                .send(AbortDriverSetup {
                    ws_id: "a".into(),
                    timeout: false,
                })
                .await
                .expect("send");
            assert_eq!(1, config_len(name));

            // The device is still configured.
            let data = serde_json::from_value(json!({ "reconfigure": true, "setup_data": {} }))
                .expect("setup");
            controller
                .send(SetupDriverMsg {
                    ws_id: "a".into(),
                    data,
                    ack: None,
                })
                .await
                .expect("send")
                .expect("setup started");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                json!(cfg.key),
                last(&events)["require_user_action"]["input"]["settings"][0]["field"]["dropdown"]["value"]
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Returns the state of the driver state machine.
        #[derive(Message)]
        #[rtype(result = "State")]
        struct MachineState;

        impl Handler<MachineState> for Controller {
            type Result = MessageResult<MachineState>;
            fn handle(&mut self, _: MachineState, _: &mut Context<Self>) -> Self::Result {
                MessageResult(self.machine.state().clone())
            }
        }

        /// Stops the config writer until the returned sender is dropped.
        #[derive(Message)]
        #[rtype(result = "mpsc::Sender<()>")]
        struct HoldWrites;

        impl Handler<HoldWrites> for Controller {
            type Result = MessageResult<HoldWrites>;
            fn handle(&mut self, _: HoldWrites, _: &mut Context<Self>) -> Self::Result {
                MessageResult(self.store.hold())
            }
        }

        /// Returns the key that the controller keeps, the configuration and the config version
        /// of a device.
        #[derive(Message)]
        #[rtype(result = "Option<(Arc<str>, DeviceConfig, u64)>")]
        struct DeviceCfg(String);

        impl Handler<DeviceCfg> for Controller {
            type Result = Option<(Arc<str>, DeviceConfig, u64)>;
            fn handle(&mut self, msg: DeviceCfg, _: &mut Context<Self>) -> Self::Result {
                let (key, d) = self.devices.get_key_value(msg.0.as_str())?;
                Some((Arc::clone(key), d.cfg.clone(), d.cfg_version))
            }
        }

        /// Shows the options page of a device, without a scan.
        #[derive(Message)]
        #[rtype(result = "()")]
        struct ShowConfigure(String);

        impl Handler<ShowConfigure> for Controller {
            type Result = ();
            fn handle(&mut self, msg: ShowConfigure, _: &mut Context<Self>) {
                self.setup = Some(SetupState::Configure {
                    key: msg.0,
                    found: Vec::new(),
                });
            }
        }

        /// The Apple TV accepted the PIN: like the PIN page input, then the pairing result.
        #[derive(Message)]
        #[rtype(result = "()")]
        struct Paired(&'static str, Found, Option<String>);

        impl Handler<Paired> for Controller {
            type Result = ();
            fn handle(&mut self, msg: Paired, ctx: &mut Context<Self>) {
                let _ = self.sm_consume(msg.0, &operation_mode::Input::SetupUserData, ctx);
                self.setup = None;
                let credentials = device().credentials;
                self.save_pairing(msg.0.into(), credentials, msg.1, false, false, msg.2, ctx);
            }
        }

        /// Sets the flag when it is dropped.
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        /// Starts a pairing step that does not end. The flag is set when it is stopped.
        #[derive(Message)]
        #[rtype(result = "()")]
        struct PendingPairing(Arc<AtomicBool>);

        impl Handler<PendingPairing> for Controller {
            type Result = ();
            fn handle(&mut self, msg: PendingPairing, ctx: &mut Context<Self>) {
                let flag = DropFlag(msg.0);
                let handle = ctx.spawn(
                    async move {
                        let _flag = flag;
                        std::future::pending::<()>().await;
                    }
                    .into_actor(self),
                );
                self.pairing = Some(handle);
            }
        }

        /// An Apple TV that is not configured.
        fn new_found() -> Found {
            Found {
                id: "02:00:00:00:00:02".into(),
                name: "Bedroom".into(),
                model: String::new(),
                address: Some(IpAddr::from([192, 0, 2, 8])),
                endpoints: Endpoints::default(),
                known_ids: vec!["02:00:00:00:00:02".into()],
            }
        }

        /// Number of setup end events.
        fn stop_events(events: &Mutex<Vec<Value>>) -> usize {
            events
                .lock()
                .expect("lock")
                .iter()
                .filter(|m| {
                    m["msg"] == "driver_setup_change" && m["msg_data"]["event_type"] == "STOP"
                })
                .count()
        }

        /// Returns `true` if the controller has the device.
        async fn has_device(controller: &Addr<Controller>, key: &str) -> bool {
            controller
                .send(DeviceAddr(key.into()))
                .await
                .expect("send")
                .is_some()
        }

        #[actix::test]
        async fn failing_pairing_write_changes_nothing() {
            let name = "pair-error";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            // A first setup replaces the devices when the pairing is saved.
            start_setup(&controller, &events, "a", false).await;
            break_writes(&dir);
            let found = new_found();
            let new_key = device_key(&found.id);
            controller
                .send(Paired("a", found, None))
                .await
                .expect("send");
            let event = wait_stop(&events).await;
            assert_eq!(json!("ERROR"), event["state"]);
            assert_eq!(json!("OTHER"), event["error"]);
            assert!(has_device(&controller, &cfg.key).await, "kept");
            assert!(!has_device(&controller, &new_key).await, "not added");
            assert_eq!(
                State::Running,
                controller.send(MachineState).await.expect("send")
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn pairing_changes_the_devices_after_the_write() {
            let name = "pair-write";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            start_setup(&controller, &events, "a", false).await;
            let hold = controller.send(HoldWrites).await.expect("send");
            let found = new_found();
            let new_key = device_key(&found.id);
            controller
                .send(Paired("a", found, None))
                .await
                .expect("send");
            settle(&controller, &[&events]).await;
            assert!(
                has_device(&controller, &cfg.key).await,
                "no change before the write"
            );
            assert!(!has_device(&controller, &new_key).await);
            drop(hold);
            let event = wait_stop(&events).await;
            assert_eq!(json!("OK"), event["state"]);
            assert!(!has_device(&controller, &cfg.key).await, "replaced");
            assert!(has_device(&controller, &new_key).await);
            controller.send(FlushStore).await.expect("send");
            let saved = ConfigStore::new(&dir).load_devices();
            assert_eq!(
                vec![new_key],
                saved.iter().map(|c| c.key.clone()).collect::<Vec<_>>()
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn pairing_again_keeps_the_key_and_name() {
            let name = "pair-again";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            let saved = || ConfigStore::new(&dir).load_devices();
            let mut found = new_found();
            found.known_ids = vec!["NEW-ID".into()];
            let other_id = found.id.clone();
            for step in 0..3 {
                let (reconfigure, replace) = match step {
                    // "pair" on the configuration page: the device is found with new ids.
                    0 => (true, Some(cfg.key.clone())),
                    1 => {
                        // A first setup finds a configured device by its id.
                        found.id.clone_from(&cfg.id);
                        // Not in the configuration now: only the id can match.
                        found.known_ids = vec![cfg.known_ids[1].clone()];
                        (false, None)
                    }
                    _ => {
                        // A first setup finds a configured device by an id that step 1 saved.
                        found.id.clone_from(&other_id);
                        found.known_ids = vec![cfg.known_ids[1].clone()];
                        (false, None)
                    }
                };
                start_setup(&controller, &events, "a", reconfigure).await;
                controller
                    .send(Paired("a", found.clone(), replace))
                    .await
                    .expect("send");
                let event = wait_stop(&events).await;
                assert_eq!(json!("OK"), event["state"]);
                let (_, now, _) = controller
                    .send(DeviceCfg(cfg.key.clone()))
                    .await
                    .expect("send")
                    .unwrap_or_else(|| panic!("step {step}: same key"));
                assert_eq!(cfg.name, now.name, "step {step}");
                assert_eq!(cfg.id, now.id, "step {step}");
                assert_eq!(found.known_ids, now.known_ids, "step {step}");
                assert_eq!(found.address, now.address, "step {step}");
                assert!(!now.global_volume, "step {step}");
                if found.id != cfg.id {
                    assert!(
                        !has_device(&controller, &device_key(&found.id)).await,
                        "no new key"
                    );
                }
                controller.send(FlushStore).await.expect("send");
                assert_eq!(vec![now], saved());
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn configure_keeps_device_changes_from_during_the_write() {
            let name = "configure-write";
            let cfg = device();
            let dir = config_dir(name, &cfg).await;
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            start_setup(&controller, &events, "a", true).await;
            controller
                .send(ShowConfigure(cfg.key.clone()))
                .await
                .expect("send");
            let hold = controller.send(HoldWrites).await.expect("send");
            controller
                .send(values("a", &[("global_volume", "false")]))
                .await
                .expect("send")
                .expect("ok");
            // The device finds a new name and address while the write waits.
            let (key, mut changed, version) = controller
                .send(DeviceCfg(cfg.key.clone()))
                .await
                .expect("send")
                .expect("device");
            assert!(changed.global_volume, "no change before the write");
            changed.name = "Den".into();
            changed.address = Some(IpAddr::from([192, 0, 2, 7]));
            controller
                .send(DeviceEvent::Config {
                    key,
                    cfg: changed,
                    version,
                })
                .await
                .expect("send");
            drop(hold);
            let event = wait_stop(&events).await;
            assert_eq!(json!("OK"), event["state"]);
            let check = |c: &DeviceConfig| {
                assert_eq!("Den", c.name);
                assert_eq!(Some(IpAddr::from([192, 0, 2, 7])), c.address);
                assert!(!c.global_volume);
            };
            let (_, now, _) = controller
                .send(DeviceCfg(cfg.key.clone()))
                .await
                .expect("send")
                .expect("device");
            check(&now);
            controller.send(FlushStore).await.expect("send");
            let saved = ConfigStore::new(&dir).load_devices();
            assert_eq!(1, saved.len());
            check(&saved[0]);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn abort_during_the_write_still_uses_the_devices() {
            let name = "abort-write";
            let dir = test_dir(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            let cfg = device();
            let controller = controller(name);
            let events = connect(&controller, "a").await;
            start_setup(&controller, &events, "a", false).await;
            let before = setup_events(&events);
            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            wait_page(&events, before).await;
            let hold = controller.send(HoldWrites).await.expect("send");
            let backup = backup_json(std::slice::from_ref(&cfg)).expect("backup");
            controller
                .send(values("a", &[("restore_data", &backup)]))
                .await
                .expect("send")
                .expect("ok");
            controller
                .send(AbortDriverSetup {
                    ws_id: "a".into(),
                    timeout: false,
                })
                .await
                .expect("send");
            let state = || controller.send(MachineState);
            assert_eq!(State::RequireSetup, state().await.expect("send"));
            drop(hold);
            for _ in 0..100 {
                if has_device(&controller, &cfg.key).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(has_device(&controller, &cfg.key).await, "restored");
            assert_eq!(State::Running, state().await.expect("send"));
            // The aborted flow sends no end event.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(0, stop_events(&events));
            assert_eq!(1, config_len(name));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[actix::test]
        async fn abort_stops_the_pairing() {
            let controller = controller("pair-abort");
            let events = connect(&controller, "a").await;
            for timeout in [false, true] {
                start_setup(&controller, &events, "a", false).await;
                let stopped = Arc::new(AtomicBool::new(false));
                controller
                    .send(PendingPairing(Arc::clone(&stopped)))
                    .await
                    .expect("send");
                controller
                    .send(AbortDriverSetup {
                        ws_id: "a".into(),
                        timeout,
                    })
                    .await
                    .expect("send");
                settle(&controller, &[&events]).await;
                assert!(stopped.load(Ordering::Relaxed), "timeout: {timeout}");
            }
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
                    ack: None,
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
