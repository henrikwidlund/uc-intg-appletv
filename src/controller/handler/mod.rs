// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Actix message handlers of the controller.

mod r2_connection;
mod r2_event;
mod r2_request;
mod r2_response;
mod setup;

pub(crate) use r2_request::driver_version;
pub(crate) use setup::SetupState;

use actix::Message;

/// Internal message to abort the setup flow, because of a timeout or an abort event from the Remote.
#[derive(Message)]
#[rtype(result = "()")]
pub(crate) struct AbortDriverSetup {
    pub ws_id: String,
    /// Internal timeout.
    pub timeout: bool,
}
