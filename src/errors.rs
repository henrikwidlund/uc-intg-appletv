// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Service error with conversions from common errors.

use actix::MailboxError;
use log::error;
use std::io::ErrorKind;

/// Errors returned to the Remote as WebSocket error responses.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("Internal server error: {0}")]
    InternalServerError(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Apple TV is not connected")]
    NotConnected,
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),
    #[error("Timeout")]
    Timeout,
    #[error("Not supported: {0}")]
    NotSupported(String),
}

impl From<std::io::Error> for ServiceError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound(error.to_string()),
            ErrorKind::AlreadyExists | ErrorKind::InvalidInput | ErrorKind::InvalidData => {
                Self::BadRequest(error.to_string())
            }
            ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::AddrInUse
            | ErrorKind::AddrNotAvailable => Self::ServiceUnavailable(error.to_string()),
            ErrorKind::TimedOut => Self::Timeout,
            _ => {
                error!("Internal error: {error:?}");
                Self::InternalServerError(error.to_string())
            }
        }
    }
}

impl From<MailboxError> for ServiceError {
    fn from(e: MailboxError) -> Self {
        Self::InternalServerError(format!("Internal message error: {e}"))
    }
}

impl From<serde_json::Error> for ServiceError {
    fn from(e: serde_json::Error) -> Self {
        Self::SerializationError(e.to_string())
    }
}

impl From<strum::ParseError> for ServiceError {
    fn from(e: strum::ParseError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl From<ratv::Error> for ServiceError {
    fn from(e: ratv::Error) -> Self {
        match e {
            ratv::Error::NotConnected(p) => Self::ServiceUnavailable(format!("{p} not connected")),
            ratv::Error::Disconnected => Self::NotConnected,
            ratv::Error::Timeout | ratv::Error::NoAnswer(_) => Self::Timeout,
            ratv::Error::InvalidArgument(a) => Self::BadRequest(a.into()),
            ratv::Error::NoTextField | ratv::Error::NoChapter => Self::BadRequest(e.to_string()),
            ratv::Error::VolumeNotAvailable => {
                Self::ServiceUnavailable("Volume control is not available on this Apple TV".into())
            }
            ratv::Error::CommandFailed { .. } => {
                Self::NotSupported("The app did not run the command".into())
            }
            ratv::Error::Io(io) => io.into(),
            e => Self::ServiceUnavailable(e.to_string()),
        }
    }
}
