// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

//! Handle events from the Remote.

use crate::controller::{Controller, R2EventMsg};
use crate::errors::ServiceError;
use crate::server::ws::WsConn;
use actix::Addr;
use log::{info, warn};
use std::str::FromStr;
use std::sync::Arc;
use uc_api::intg::ws::R2Event;
use uc_api::ws::WsMessage;

impl WsConn {
    /// Forward an event message to the controller.
    pub(crate) fn on_event(
        session_id: &Arc<str>,
        event: WsMessage,
        controller_addr: &Addr<Controller>,
    ) -> Result<(), ServiceError> {
        let msg = event
            .msg
            .as_deref()
            .ok_or_else(|| ServiceError::BadRequest("Missing property: msg".into()))?;

        info!("[{session_id}] Got event: {msg}");

        if let Ok(event_msg) = R2Event::from_str(msg) {
            // `do_send`: an event must not be lost when the mailbox is full.
            controller_addr.do_send(R2EventMsg {
                ws_id: session_id.clone(),
                event: event_msg,
            });
        } else {
            warn!("[{session_id}] Unknown event: {msg}");
        }

        Ok(())
    }
}
