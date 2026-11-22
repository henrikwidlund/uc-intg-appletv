// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Handler for [`R2RequestMsg`].

use crate::controller::devices::Trigger;
use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
use crate::controller::{Controller, R2RequestMsg};
use crate::errors::ServiceError;
use crate::util::{DeserializeMsgData, return_fut_err, return_fut_ok};
use crate::{APP_VERSION, built_info};
use actix::{Handler, ResponseFuture};
use log::debug;
use serde_json::Value;
use strum::EnumMessage;
use uc_api::intg::IntegrationVersion;
use uc_api::intg::ws::{AvailableEntitiesMsgData, DriverVersionMsgData, R2Request};
use uc_api::ws::WsMessage;

/// Integration-API version from the `uc_api` dependency.
fn api_version() -> &'static str {
    built_info::DEPENDENCIES
        .iter()
        .find(|p| p.0 == "uc_api")
        .map_or("?", |v| v.1)
}

impl Handler<R2RequestMsg> for Controller {
    type Result = ResponseFuture<Result<Option<WsMessage>, ServiceError>>;

    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, msg: R2RequestMsg, ctx: &mut Self::Context) -> Self::Result {
        debug!("R2RequestMsg: {:?}", msg.request);
        // A Remote that sends a request is awake: not in standby and not after `disconnect`.
        let Some(session) = self.sessions.get_mut(&msg.ws_id) else {
            return_fut_err!(ServiceError::NotFound("No session found".into()));
        };
        let woke = !session.awake();
        session.standby = false;
        session.devices_off = false;
        if woke {
            self.update_connections(Trigger::Wake);
        }

        let req_id = msg.req_id;
        // Answered with an event, so it has no response message.
        if msg.request == R2Request::GetDeviceState {
            return_fut_ok!(Some(super::super::device_state_event(self.device_state)));
        }
        let Some(resp_msg) = msg.request.get_message() else {
            return_fut_err!(ServiceError::InternalServerError(format!(
                "No response message for {}",
                msg.request
            )));
        };

        // Driver and setup requests.
        match msg.request {
            R2Request::GetDriverVersion => {
                return_fut_ok!(Some(WsMessage::response(
                    req_id,
                    resp_msg,
                    DriverVersionMsgData {
                        name: self
                            .drv_metadata
                            .name
                            .as_ref()
                            .and_then(|n| n.get("en").cloned()),
                        version: Some(IntegrationVersion {
                            api: Some(api_version().into()),
                            driver: Some(APP_VERSION.into()),
                        }),
                    },
                )));
            }
            R2Request::GetDriverMetadata => {
                return_fut_ok!(Some(WsMessage::response(
                    req_id,
                    resp_msg,
                    &self.drv_metadata
                )));
            }
            // The setup messages are handled here and not sent to the mailbox again, so a later
            // event (for example abort_driver_setup) cannot be handled first.
            R2Request::SetupDriver => {
                let ws_id = msg.ws_id.to_string();
                let result = msg
                    .deserialize()
                    .map_err(ServiceError::from)
                    .and_then(|data| self.handle(SetupDriverMsg { ws_id, data }, ctx));
                return Box::pin(async move {
                    result?;
                    Ok(Some(WsMessage::response(req_id, resp_msg, Value::Null)))
                });
            }
            R2Request::SetDriverUserData => {
                let ws_id = msg.ws_id.to_string();
                let result = msg
                    .deserialize()
                    .map_err(ServiceError::from)
                    .and_then(|data| self.handle(SetDriverUserDataMsg { ws_id, data }, ctx));
                return Box::pin(async move {
                    result?;
                    Ok(Some(WsMessage::response(req_id, resp_msg, Value::Null)))
                });
            }
            _ => {}
        }

        // Entity requests work in every state, also during a setup flow: the configured devices
        // keep running. Without devices the lists are empty and commands get 404.
        match msg.request {
            R2Request::GetAvailableEntities => {
                let msg_data = AvailableEntitiesMsgData {
                    filter: None,
                    available_entities: self.available_entities(),
                };
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, msg_data)));
            }
            R2Request::GetEntityStates => {
                self.mark_states_read(&msg.ws_id);
                let states = self.entity_states(&msg.ws_id);
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, states)));
            }
            R2Request::SubscribeEvents => {
                let ws_id = msg.ws_id.clone();
                match entity_ids(msg.msg_data) {
                    Ok(ids) => self.subscribe(&ws_id, ids),
                    Err(e) => return_fut_err!(e.into()),
                }
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, Value::Null)));
            }
            R2Request::UnsubscribeEvents => {
                let ws_id = msg.ws_id.clone();
                match entity_ids(msg.msg_data) {
                    Ok(ids) => self.unsubscribe(&ws_id, &ids),
                    Err(e) => return_fut_err!(e.into()),
                }
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, Value::Null)));
            }
            R2Request::EntityCommand => self.entity_command(msg, resp_msg, ctx),
            R2Request::BrowseMedia => self.browse_media(msg, resp_msg),
            R2Request::SearchMedia => {
                return_fut_err!(ServiceError::NotSupported("search_media".into()))
            }
            // Handled above. Listed so that the compiler reports new variants.
            R2Request::GetDriverVersion
            | R2Request::GetDriverMetadata
            | R2Request::GetDeviceState
            | R2Request::SetupDriver
            | R2Request::SetDriverUserData => {
                return_fut_err!(ServiceError::InternalServerError(format!(
                    "Request {} not handled",
                    msg.request
                )))
            }
        }
    }
}

/// `msg_data` of `subscribe_events` and `unsubscribe_events`.
#[derive(serde::Deserialize, Default)]
struct EntityIds {
    entity_ids: Option<Vec<String>>,
}

/// Entity ids of a (un)subscribe request. No `msg_data` or no ids means all entities.
fn entity_ids(msg_data: Option<Value>) -> Result<Vec<String>, serde_json::Error> {
    Ok(msg_data
        .filter(|v| !v.is_null())
        .map(serde_json::from_value::<EntityIds>)
        .transpose()?
        .unwrap_or_default()
        .entity_ids
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entity_ids_missing_means_all() {
        let none: Vec<String> = Vec::new();
        assert_eq!(none, entity_ids(None).expect("ok"));
        assert_eq!(none, entity_ids(Some(Value::Null)).expect("ok"));
        assert_eq!(none, entity_ids(Some(json!({}))).expect("ok"));
        assert_eq!(
            none,
            entity_ids(Some(json!({ "entity_ids": null }))).expect("ok")
        );
        assert_eq!(
            none,
            entity_ids(Some(json!({ "entity_ids": [] }))).expect("ok")
        );
        assert_eq!(
            vec!["a".to_string()],
            entity_ids(Some(json!({ "entity_ids": ["a"] }))).expect("ok")
        );
        assert!(entity_ids(Some(json!({ "entity_ids": "a" }))).is_err());
    }
}
