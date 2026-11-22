// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

//! Handle response messages from the Remote.

use crate::controller::{Controller, R2ResponseMsg};
use crate::errors::ServiceError;
use crate::server::ws::WsConn;
use actix::Addr;
use log::{debug, warn};
use std::str::FromStr;
use std::sync::Arc;
use uc_api::intg::ws::R2Response;
use uc_api::ws::WsMessage;

impl WsConn {
    /// Forward a response message to the controller.
    pub(crate) fn on_response(
        session_id: &Arc<str>,
        response: WsMessage,
        controller_addr: &Addr<Controller>,
    ) -> Result<(), ServiceError> {
        let msg = response
            .msg
            .as_deref()
            .ok_or_else(|| ServiceError::BadRequest("Missing property: msg".into()))?;

        debug!("[{session_id}] Got response: {msg}");

        if let Ok(resp_msg) = R2Response::from_str(msg) {
            controller_addr.do_send(R2ResponseMsg {
                ws_id: session_id.clone(),
                msg: resp_msg,
            });
        } else {
            warn!("[{session_id}] Unknown response: {msg}");
        }

        Ok(())
    }
}
