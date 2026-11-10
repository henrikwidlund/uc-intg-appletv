// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Handlers for Remote connection messages.

use crate::controller::{Controller, NewR2Session, R2Session, R2SessionDisconnect, SendWsMessage};
use actix::{Context, Handler};
use log::{error, info};
use uc_api::ws::WsMessage;

impl Handler<NewR2Session> for Controller {
    type Result = ();

    fn handle(&mut self, msg: NewR2Session, _: &mut Context<Self>) -> Self::Result {
        let mut session = R2Session::new(msg.addr);
        // Get the version information of the Remote for the log.
        let request = WsMessage::simple_request(session.new_msg_id(), "get_version");
        if let Err(e) = session.recipient.try_send(SendWsMessage::Msg(request)) {
            error!("[{}] Error sending get_version: {e}", msg.id);
        }
        info!("[{}] Remote connected", msg.id);
        self.sessions.insert(msg.id.clone(), session);
        self.send_device_state(&msg.id);
    }
}

impl Handler<R2SessionDisconnect> for Controller {
    type Result = ();

    fn handle(&mut self, msg: R2SessionDisconnect, ctx: &mut Context<Self>) {
        info!("[{}] Remote disconnected", msg.id);
        // End the setup flow of this Remote, so that other Remotes can start one.
        if self.setup_owner.as_deref() == Some(&*msg.id) {
            self.abort_setup(&msg.id, false, ctx);
        }
        self.sessions.remove(&msg.id);
    }
}
