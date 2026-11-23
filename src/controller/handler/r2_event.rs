// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Handler for [`R2EventMsg`].

use crate::controller::devices::Trigger;
use crate::controller::handler::AbortDriverSetup;
use crate::controller::{Controller, R2EventMsg};
use actix::{AsyncContext, Handler};
use log::error;
use uc_api::intg::ws::R2Event;

impl Handler<R2EventMsg> for Controller {
    type Result = ();

    fn handle(&mut self, msg: R2EventMsg, ctx: &mut Self::Context) -> Self::Result {
        let Some(session) = self.sessions.get_mut(&msg.ws_id) else {
            error!("Session not found: {}", msg.ws_id);
            return;
        };

        // Connections stay while another awake Remote uses the device.
        match msg.event {
            // A Remote that asks for connections is awake.
            R2Event::Connect | R2Event::ExitStandby => {
                self.wake(&msg.ws_id);
                self.update_connections(Trigger::Wake);
                // The Remote might be out of sync.
                self.send_device_state(&msg.ws_id);
                self.send_missed_states(&msg.ws_id);
            }
            R2Event::Disconnect => {
                session.devices_off = true;
                self.update_connections(Trigger::Sleep);
            }
            R2Event::EnterStandby => {
                session.standby = true;
                self.update_connections(Trigger::Sleep);
            }
            R2Event::AbortDriverSetup => {
                ctx.notify(AbortDriverSetup {
                    ws_id: msg.ws_id.to_string(),
                    timeout: false,
                });
            }
            R2Event::Oauth2Authorization | R2Event::Oauth2Refreshed => {
                // Not used by this driver.
            }
        }
    }
}
