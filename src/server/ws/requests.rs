// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

//! Handle request messages from the Remote.

use crate::controller::{Controller, R2RequestMsg};
use crate::errors::ServiceError;
use crate::server::ws::WsConn;
use actix::Addr;
use actix::dev::Request;
use log::{debug, warn};
use std::str::FromStr;
use std::sync::Arc;
use uc_api::intg::ws::R2Request;
use uc_api::ws::WsMessage;

impl WsConn {
    /// Forward a request message to the controller.
    ///
    /// The message is in the controller queue when this returns, so it keeps its order with
    /// later events, unless an earlier send from this address found the controller mailbox
    /// full; then it waits. The returned request gives the result:
    /// - A [`ServiceError`] is sent back as an error response.
    /// - `Ok(None)` means the controller sends the response itself.
    ///
    /// `msg_data` is taken from the request.
    pub(crate) fn on_request(
        session_id: &Arc<str>,
        req_id: u32,
        request: &mut WsMessage,
        controller_addr: &Addr<Controller>,
    ) -> Result<Request<Controller, R2RequestMsg>, ServiceError> {
        let msg = request
            .msg
            .as_deref()
            .ok_or_else(|| ServiceError::BadRequest("Missing property: msg".into()))?;

        debug!("[{session_id}] Got request: {msg}");

        if let Ok(req_msg) = R2Request::from_str(msg) {
            Ok(controller_addr.send(R2RequestMsg {
                ws_id: session_id.clone(),
                req_id,
                request: req_msg,
                msg_data: request.msg_data.take(),
            }))
        } else {
            warn!("[{session_id}] Unknown message: {msg}");
            Err(ServiceError::BadRequest(format!("Unknown message: {msg}")))
        }
    }
}
