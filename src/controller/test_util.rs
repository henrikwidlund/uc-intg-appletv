// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Test helpers: a controller with a temporary configuration directory, and Remotes that collect
//! the messages they get.

use crate::config_store::DriverSettings;
use crate::configuration::{HeartbeatSettings, Settings};
use crate::controller::{Controller, NewR2Session, SendWsMessage};
use crate::device::AppleTvActor;
use crate::server::ArtworkStore;
use actix::{Actor, Addr, Context, Handler, Message};
use serde_json::{Value, json};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Collects all messages sent to a Remote, as JSON.
struct Remote(Arc<Mutex<Vec<Value>>>);

impl Actor for Remote {
    type Context = Context<Self>;
}

impl Handler<SendWsMessage> for Remote {
    type Result = ();
    fn handle(&mut self, msg: SendWsMessage, _: &mut Context<Self>) {
        let value = match msg {
            SendWsMessage::Msg(m) => serde_json::to_value(m).expect("json"),
            SendWsMessage::Text(t) => serde_json::from_str(&t).expect("json"),
        };
        self.0.lock().expect("lock").push(value);
    }
}

/// Does nothing. The answer comes after all messages that were queued before it.
#[derive(Message)]
#[rtype(result = "()")]
struct Probe;

impl Handler<Probe> for Remote {
    type Result = ();
    fn handle(&mut self, _: Probe, _: &mut Context<Self>) {}
}

impl Handler<Probe> for Controller {
    type Result = ();
    fn handle(&mut self, _: Probe, _: &mut Context<Self>) {}
}

/// The actor of the device with this key.
#[derive(Message)]
#[rtype(result = "Option<Addr<AppleTvActor>>")]
pub(crate) struct DeviceAddr(pub String);

impl Handler<DeviceAddr> for Controller {
    type Result = Option<Addr<AppleTvActor>>;
    fn handle(&mut self, msg: DeviceAddr, _: &mut Context<Self>) -> Self::Result {
        self.devices.get(msg.0.as_str()).map(|d| d.addr.clone())
    }
}

/// The stored address of the last Remote connection.
#[derive(Message)]
#[rtype(result = "Option<std::net::IpAddr>")]
pub(crate) struct RemoteIp;

impl Handler<RemoteIp> for Controller {
    type Result = Option<std::net::IpAddr>;
    fn handle(&mut self, _: RemoteIp, _: &mut Context<Self>) -> Self::Result {
        self.device_ctx.remote_ip()
    }
}

/// Answer when the store has done all queued file jobs.
#[derive(Message)]
#[rtype(result = "()")]
pub(crate) struct FlushStore;

impl Handler<FlushStore> for Controller {
    type Result = ();
    fn handle(&mut self, _: FlushStore, _: &mut Context<Self>) {
        self.store.flush();
    }
}

/// Wait until the actor has stopped, then until the store has done all queued file jobs.
pub(crate) async fn wait_stopped(controller: &Addr<Controller>, actor: &Addr<AppleTvActor>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while actor.connected() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor stopped");
    controller.send(FlushStore).await.expect("send");
}

/// A connected test Remote. Derefs to the list of received messages.
pub(crate) struct TestRemote {
    messages: Arc<Mutex<Vec<Value>>>,
    addr: Addr<Remote>,
}

impl Deref for TestRemote {
    type Target = Mutex<Vec<Value>>;
    fn deref(&self) -> &Self::Target {
        &self.messages
    }
}

/// Temporary configuration directory of a test.
pub(crate) fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("uc-intg-appletv-{name}-{}", std::process::id()))
}

/// Start a controller that uses [`test_dir`].
pub(crate) fn controller(name: &str) -> Addr<Controller> {
    let dir = test_dir(name);
    let settings = Settings {
        interface: "127.0.0.1".into(),
        http_port: 0,
        config_home: dir.clone(),
        client_name: "test".into(),
        heartbeat: HeartbeatSettings::default(),
    };
    let metadata = serde_json::from_value(json!({})).expect("metadata");
    Controller::new(
        settings,
        DriverSettings::default(),
        metadata,
        ArtworkStore::new(dir.join("artwork")),
    )
    .start()
}

/// Connect a Remote with the session id `id`.
pub(crate) async fn connect(controller: &Addr<Controller>, id: &str) -> TestRemote {
    let messages = Arc::default();
    let addr = Remote(Arc::clone(&messages)).start();
    controller
        .send(NewR2Session {
            addr: addr.clone().recipient(),
            id: id.into(),
            local_ip: None,
        })
        .await
        .expect("send");
    TestRemote { messages, addr }
}

/// Wait until the controller handled all queued messages, and then until the Remotes handled
/// all messages from the controller. Timers and spawned tasks are not waited for.
pub(crate) async fn settle(controller: &Addr<Controller>, remotes: &[&TestRemote]) {
    controller.send(Probe).await.expect("send");
    for remote in remotes {
        remote.addr.send(Probe).await.expect("send");
    }
}
