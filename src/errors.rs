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

/// For a message to a device actor, use [`ServiceError::from_device_mailbox`].
impl From<MailboxError> for ServiceError {
    fn from(e: MailboxError) -> Self {
        Self::InternalServerError(format!("Internal message error: {e}"))
    }
}

impl ServiceError {
    /// Error for a message to a device actor that was not handled.
    pub fn from_device_mailbox(e: MailboxError) -> Self {
        match e {
            // The device actor is stopped: the device was removed or replaced.
            MailboxError::Closed => {
                Self::NotFound("The Apple TV was removed or reconfigured".into())
            }
            MailboxError::Timeout => Self::Timeout,
        }
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
            ratv::Error::Disconnected | ratv::Error::Closed => Self::NotConnected,
            // `SendTimeout`: the device does not read, so it is counted like no answer.
            ratv::Error::Timeout | ratv::Error::NoAnswer(_) | ratv::Error::SendTimeout(_) => {
                Self::Timeout
            }
            ratv::Error::InvalidArgument(a) => Self::BadRequest(a.into()),
            ratv::Error::NoTextField | ratv::Error::NoChapter => Self::BadRequest(e.to_string()),
            // Refusals, or no volume control on the device. Mapped to `NotSupported`, which is
            // logged at info.
            ratv::Error::VolumeNotAvailable => {
                Self::NotSupported("Volume control is not available on this Apple TV".into())
            }
            ratv::Error::CommandFailed { .. } => {
                Self::NotSupported("The app did not run the command".into())
            }
            e if e.is_rejected() => {
                Self::NotSupported(format!("The Apple TV refused the command: {e}"))
            }
            // From a call: nothing was sent, the input is too large for one message.
            e if e.is_too_large() => {
                Self::BadRequest("The input is too large to send to the Apple TV".into())
            }
            // Only connect and pair return `Io`. Never a bad request.
            ratv::Error::Io(io) if io.kind() == ErrorKind::TimedOut => Self::Timeout,
            ratv::Error::Io(io) => Self::ServiceUnavailable(io.to_string()),
            e => Self::ServiceUnavailable(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(kind: ErrorKind) -> ratv::Error {
        ratv::Error::Io(std::io::Error::from(kind))
    }

    #[test]
    fn closed_and_send_timeout() {
        for e in [ratv::Error::Closed, ratv::Error::Disconnected] {
            assert_eq!(ServiceError::NotConnected, e.into());
        }
        for e in [
            ratv::Error::SendTimeout("press"),
            ratv::Error::NoAnswer("x"),
        ] {
            assert_eq!(ServiceError::Timeout, e.into());
        }
    }

    #[test]
    fn ratv_io_is_timeout_or_unavailable() {
        assert_eq!(ServiceError::Timeout, io(ErrorKind::TimedOut).into());
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::InvalidInput,
            ErrorKind::InvalidData,
            ErrorKind::NotFound,
            ErrorKind::Other,
        ] {
            assert!(
                matches!(io(kind).into(), ServiceError::ServiceUnavailable(_)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn mailbox_errors() {
        assert!(matches!(
            ServiceError::from(MailboxError::Closed),
            ServiceError::InternalServerError(_)
        ));
        assert_eq!(
            ServiceError::NotFound("The Apple TV was removed or reconfigured".into()),
            ServiceError::from_device_mailbox(MailboxError::Closed)
        );
        assert_eq!(
            ServiceError::Timeout,
            ServiceError::from_device_mailbox(MailboxError::Timeout)
        );
    }

    #[test]
    fn refusals_are_not_supported() {
        assert!(matches!(
            ServiceError::from(ratv::Error::VolumeNotAvailable),
            ServiceError::NotSupported(_)
        ));
        let failed = ratv::Error::CommandFailed {
            send_error: 0,
            handler_status: 404,
        };
        assert!(failed.is_rejected());
        assert_eq!(
            ServiceError::NotSupported("The app did not run the command".into()),
            failed.into()
        );
    }
}
