// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Actor for one Apple TV: connection, state updates and commands.

use crate::config_store::{DeviceConfig, SiriMode, fnv1a};
use crate::device::commands::{Call, Planner, Seek, exec_all};
use crate::device::discovery::network_not_ready;
use crate::device::state::{
    ARTWORK_SIZE, AccountInfo, AppInfo, Dirty, ItemArtwork, Power, Snapshot,
};
use crate::device::{
    AudioChunk, Browse, Command, Connect, DeviceContext, DeviceEvent, DeviceStatus, Disconnect,
    RemoteAddressChanged, SiriModeChanged, Stop, UpdateConfig, browse, discovery, now,
    pairing_not_valid,
};
use crate::entities::{self, EntityKind, media_player, remote, select};
use crate::errors::ServiceError;
use actix::prelude::*;
use bytes::Bytes;
use futures::future::Either;
use futures::{Stream, StreamExt};
use log::{Level, debug, info, log_enabled, warn};
use ratv::companion::{AttentionState, Event};
use ratv::mrp::{NowPlaying, PlaybackState, Volume};
use ratv::{AppleTv, ClientOptions, Endpoints, Protocol};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::BroadcastStream;
use uc_api::intg::{AssistantError, AssistantErrorCode, AssistantEvent, IntgRemoteCommand};

/// Interval for the app, account and AirPlay polls.
const POLL_INTERVAL: Duration = Duration::from_secs(300);
/// The list time is taken at the reply, so the next poll comes a bit early.
const POLL_MARGIN: Duration = Duration::from_secs(30);
/// Interval to connect a missing protocol. Each try uses the known endpoints first. The device can
/// change the port of a service when it restarts, so a refused connect or a missing endpoint scans
/// for the new one.
const MISSING_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Waits before asking again when a request fails. Right after wake the device is not ready
/// and answers with an error or not at all.
const NOT_READY_RETRY: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
/// A command is dropped when it waited longer than this for the connect or for the command
/// before it. The user does not expect it any more.
const COMMAND_MAX_WAIT: Duration = Duration::from_secs(10);
/// Wait before a command that was not sent is sent again, so that the actor has seen the closed
/// connection first.
const SEND_AGAIN_DELAY: Duration = Duration::from_millis(200);
/// Longest wait for a running connect before a command runs. Also limits the wait for the MRP
/// join.
const CONNECT_WAIT: Duration = COMMAND_MAX_WAIT;
/// A connect step without answer in this time is started again once.
///
/// After wake-up, the Wi-Fi link of the Remote can go down for a short time during the connect.
/// Lost packets then wait for the TCP resend (1 s or more). A connect step takes much less.
const STALL_TIMEOUT: Duration = Duration::from_millis(500);
/// Time that Companion connects alone. If it takes longer, MRP alone is tried at the same time.
/// Companion connects in about 30 ms, so a longer time means that a step stalled.
const COMPANION_HEAD_START: Duration = STALL_TIMEOUT;
/// Longest wait of a command for MRP after a connect with Companion only. Siri over MRP waits for
/// the end of the join, see [`JoinWait::Full`].
const MRP_JOIN_WAIT: Duration = STALL_TIMEOUT;
/// Longest wait for the first now playing and volume state after a connect.
const FIRST_STATE_WAIT: Duration = Duration::from_secs(1);
/// Time between connects while the network is not ready, for example right after wake-up.
const NETWORK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// Longest time to retry fast while the network is not ready. Then the normal delay is used.
const NETWORK_RETRY_TIMEOUT: Duration = Duration::from_secs(3);
/// Command timeouts in [`COMMAND_TIMEOUT_WINDOW`] after which the connection is made again.
const MAX_COMMAND_TIMEOUTS: usize = 3;
const COMMAND_TIMEOUT_WINDOW: Duration = Duration::from_secs(60);
/// A connection that stayed up this long resets the reconnect delay.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Siri ends if no audio arrives in this time.
const SIRI_AUDIO_TIMEOUT: Duration = Duration::from_secs(10);
/// Audio chunks kept while Siri starts.
const SIRI_QUEUE: usize = 256;
/// Longest wait for the session before to end.
const SIRI_WAIT: Duration = Duration::from_secs(15);
/// Screen events come in bursts. Act after the last one.
const SCREEN_SETTLE: Duration = Duration::from_millis(1500);
/// Now playing changes right after the Home Screen shows belong to the switch.
const HOME_GRACE: Duration = Duration::from_secs(1);
/// A chapter timer is kept if the new time is this close.
const CHAPTER_TIMER_MARGIN: Duration = Duration::from_millis(250);
/// Screen changes in this time after an app launch come from the launch.
const LAUNCH_GRACE: Duration = Duration::from_secs(5);

/// Lists that are kept between restarts. A missing list is empty.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceCache {
    apps: Vec<AppInfo>,
    airplay_devices: Vec<(String, String)>,
}

/// A watch channel has a new value.
#[derive(Debug, Clone, Copy, Message)]
#[rtype(result = "()")]
enum WatchChanged {
    NowPlaying(u64),
    Volume(u64),
}

/// Companion event, or `None` if events were lost.
#[derive(Debug, Message)]
#[rtype(result = "()")]
struct CompanionEvent {
    /// Value of [`AppleTvActor::companion_gen`] when the stream started.
    companion_gen: u64,
    event: Option<Event>,
}

/// How [`AppleTvActor::on_connected`] got its handle.
#[derive(Clone, Copy)]
enum Link {
    /// A new connect. `via` tells how the address was found. With `join`, the handle has only
    /// Companion, and MRP is connected next at these endpoints.
    New {
        via: &'static str,
        join: Option<Endpoints>,
    },
    /// From `connect_missing`. `added` names the new protocols for the log. With
    /// `keep_companion`, Companion is the same as in the handle before, so its requests and
    /// events go on.
    Missing {
        added: &'static str,
        keep_companion: bool,
    },
}

/// Receivers that a command waits on while a connect or the MRP join runs.
struct ConnectWait {
    status: watch::Receiver<DeviceStatus>,
    mrp_join: watch::Receiver<Option<tokio::time::Instant>>,
}

struct SiriSession {
    session_id: u32,
    audio: mpsc::Sender<Bytes>,
    /// Shared with the future that runs the session.
    flags: Arc<SiriFlags>,
    /// Audio came from the Remote.
    got_audio: bool,
}

/// State of a Siri session that the actor sets and the session future reads.
#[derive(Default)]
struct SiriFlags {
    /// Set when a newer session replaces this one.
    replaced: AtomicBool,
    /// Set when the end marker came before any audio. Siri is then not started.
    aborted: AtomicBool,
}

impl SiriSession {
    /// Notes a chunk from the Remote. An end marker before any audio aborts the session.
    fn note_chunk(&mut self, end: bool) {
        if !end {
            self.got_audio = true;
        } else if !self.got_audio {
            self.flags.aborted.store(true, Ordering::Relaxed);
        }
    }
}

/// Text for a command or Siri session while the pairing is not valid.
const PAIRING_INVALID: &str = "Pairing is not valid, pair the Apple TV again";

#[derive(Default)]
struct Timers {
    reconnect: Option<SpawnHandle>,
    poll: Option<SpawnHandle>,
    chapter: Option<SpawnHandle>,
    /// Interval to connect a missing protocol.
    missing: Option<SpawnHandle>,
    /// Waits until one of the open protocols closes.
    closed: Option<SpawnHandle>,
    screen: Option<SpawnHandle>,
    /// Running AirPlay scan.
    scan: Option<SpawnHandle>,
    /// Streams of now playing, volume and Companion events.
    now_playing: Option<SpawnHandle>,
    volume: Option<SpawnHandle>,
    companion: Option<SpawnHandle>,
}

/// Like `add_message_stream`, but the returned handle can stop the stream.
fn spawn_stream<M, S>(ctx: &mut Context<AppleTvActor>, stream: S) -> SpawnHandle
where
    S: futures::Stream<Item = M> + 'static,
    M: Message + 'static,
    AppleTvActor: Handler<M>,
{
    ctx.spawn(
        fut::wrap_stream(stream)
            .map(|msg, act: &mut AppleTvActor, ctx| {
                let _ = act.handle(msg, ctx);
            })
            .finish(),
    )
}

/// Keep `new` in `slot` and stop the stream that was there.
fn replace_handle(
    ctx: &mut Context<AppleTvActor>,
    slot: &mut Option<SpawnHandle>,
    new: SpawnHandle,
) {
    if let Some(old) = slot.replace(new) {
        ctx.cancel_future(old);
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct AppleTvActor {
    cfg: DeviceConfig,
    /// Incremented on each configuration change.
    cfg_version: u64,
    key: Arc<str>,
    ctx: Arc<DeviceContext>,
    controller: Recipient<DeviceEvent>,
    tv: Option<AppleTv>,
    s: Snapshot,
    status: DeviceStatus,
    /// Sends each status change. Commands wait on it while a connect runs.
    status_tx: watch::Sender<DeviceStatus>,
    /// The controller wants a connection.
    want: bool,
    /// Delete the cache file in [`Actor::stopped`]. Set by [`Stop`].
    remove_cache: bool,
    attempts: u32,
    /// Incremented on each connect attempt and on close. Results of older attempts are ignored.
    attempt: u64,
    /// Time of the last successful connect.
    connected_at: Option<Instant>,
    /// Incremented on each connect and disconnect. Results of older connections are ignored.
    generation: u64,
    /// Incremented when Companion connects or closes. Results of older Companion requests are
    /// ignored. A handle from `connect_missing` that keeps Companion keeps this value.
    companion_gen: u64,
    /// Set while MRP connects after a connect with Companion only. Holds the end of the
    /// command wait for it, see [`MRP_JOIN_WAIT`].
    mrp_join: watch::Sender<Option<tokio::time::Instant>>,
    /// The artwork needs MRP. It is fetched when MRP connects.
    artwork_after_mrp: bool,
    np: Option<watch::Receiver<NowPlaying>>,
    volume: Option<watch::Receiver<Volume>>,
    attention: Option<AttentionState>,
    seek: Option<Seek>,
    siri: Option<SiriSession>,
    /// Held while a Siri session exists on the device.
    siri_lock: Arc<tokio::sync::Mutex<()>>,
    /// Commands run one after the other.
    cmd_lock: Arc<tokio::sync::Mutex<()>>,
    /// Counter of `stop_send` commands. A change stops the running and the waiting commands.
    stop: watch::Sender<u64>,
    timers: Timers,
    apps_fetched: Option<Instant>,
    outputs_scanned: Option<Instant>,
    /// Last sent current chapter.
    chapter: Option<usize>,
    /// Incremented on each artwork update. Results of older fetches are ignored.
    artwork_seq: u64,
    /// The Home Screen shows: now playing is cleared since then.
    home_since: Option<Instant>,
    /// A now playing update came while `home_since` was new. It is read again later.
    home_dropped: bool,
    /// When the chapter timer fires.
    chapter_due: Option<Instant>,
    /// Time of the last app launch through the driver.
    launched_at: Option<Instant>,
    /// Last media control flags from Companion. 0 when no app has media controls.
    media_flags: Option<u32>,
    /// Name of the shown artwork file.
    shown_file: Option<Box<str>>,
    item_art: ItemArtwork,
    /// Cached result of [`Self::find_http_base`], with the Remote address it used. Reset on
    /// close.
    http_base: Option<(Option<IpAddr>, String)>,
    /// Endpoints of the last successful connect, or of the last scan while a protocol is missing.
    /// Tried before discovery on reconnect.
    endpoints: Option<Endpoints>,
    /// A try to connect a missing protocol runs.
    missing_running: bool,
    /// End of the fast retries of a missing protocol while the network is not ready.
    missing_network_end: Option<Instant>,
    /// Mute and power set by the last commands. Used for toggles until the device reports them.
    expected_mute: Option<bool>,
    expected_power: Option<Power>,
    /// End of the fast retries while the network is not ready.
    network_retry_end: Option<Instant>,
    /// Times of the last command timeouts.
    command_timeouts: VecDeque<Instant>,
}

impl AppleTvActor {
    /// `key` is shared with the controller, which uses it to tell this actor from an older one.
    pub fn new(
        key: Arc<str>,
        cfg: DeviceConfig,
        cache: DeviceCache,
        ctx: Arc<DeviceContext>,
        controller: Recipient<DeviceEvent>,
    ) -> Self {
        let mut s = Snapshot {
            name: cfg.name.clone(),
            global_volume: cfg.global_volume,
            ..Snapshot::default()
        };
        s.set_apps(cache.apps);
        s.set_airplay_devices(cache.airplay_devices);
        Self {
            key,
            cfg,
            cfg_version: 0,
            ctx,
            controller,
            tv: None,
            s,
            status: DeviceStatus::Disconnected,
            status_tx: watch::Sender::new(DeviceStatus::Disconnected),
            want: false,
            remove_cache: false,
            attempts: 0,
            attempt: 0,
            connected_at: None,
            generation: 0,
            companion_gen: 0,
            mrp_join: watch::Sender::new(None),
            artwork_after_mrp: false,
            np: None,
            volume: None,
            attention: None,
            seek: None,
            siri: None,
            siri_lock: Arc::default(),
            artwork_seq: 0,
            home_since: None,
            home_dropped: false,
            chapter_due: None,
            launched_at: None,
            media_flags: None,
            shown_file: None,
            item_art: ItemArtwork::default(),
            http_base: None,
            endpoints: None,
            missing_running: false,
            missing_network_end: None,
            expected_mute: None,
            expected_power: None,
            network_retry_end: None,
            command_timeouts: VecDeque::with_capacity(MAX_COMMAND_TIMEOUTS),
            cmd_lock: Arc::default(),
            stop: watch::Sender::new(0),
            timers: Timers::default(),
            apps_fetched: None,
            outputs_scanned: None,
            chapter: None,
        }
    }

    /// Send the attributes of the dirty groups to the controller.
    fn emit(&mut self, mut dirty: Dirty) {
        let now = now();
        if dirty.any(
            Dirty::CONNECTION
                | Dirty::POSITION
                | Dirty::PLAYBACK
                | Dirty::CHAPTERS
                | Dirty::CHAPTER,
        ) {
            let chapter = self.s.current_chapter(now);
            if chapter != self.chapter {
                self.chapter = chapter;
                dirty |= Dirty::CHAPTER;
            }
        }
        if dirty.is_empty() {
            return;
        }
        let changes: Vec<_> = EntityKind::all()
            .filter_map(|kind| {
                let mut attrs = Map::new();
                entities::attributes(&self.s, kind, dirty, now, &mut attrs);
                (!attrs.is_empty()).then_some((kind, attrs))
            })
            .collect();
        if !changes.is_empty() {
            self.controller.do_send(DeviceEvent::Changes {
                key: self.key.clone(),
                changes,
            });
        }
    }

    /// What to wait on while a connect runs and there is no connection yet, or while MRP
    /// connects after a connect with Companion only.
    /// Uses the shown status: it stays `Connecting` between quick retries while the network is
    /// not ready.
    fn running_connect(&self) -> Option<ConnectWait> {
        let connecting = self.tv.is_none() && *self.status_tx.borrow() == DeviceStatus::Connecting;
        (connecting || self.joining()).then(|| ConnectWait {
            status: self.status_tx.subscribe(),
            mrp_join: self.mrp_join.subscribe(),
        })
    }

    /// MRP connects after a connect with Companion only.
    fn joining(&self) -> bool {
        self.mrp_join.borrow().is_some()
    }

    /// Start the MRP join. Commands wait for it until [`MRP_JOIN_WAIT`] from now.
    fn start_mrp_join(&self) {
        self.mrp_join
            .send_replace(Some(tokio::time::Instant::now() + MRP_JOIN_WAIT));
    }

    /// End the MRP join. Returns `true` if it was running.
    fn end_mrp_join(&self) -> bool {
        self.mrp_join.send_if_modified(|j| j.take().is_some())
    }

    /// Set the status and show it, if the shown status is different.
    fn set_status(&mut self, status: DeviceStatus) {
        self.status = status;
        if *self.status_tx.borrow() != status {
            self.status_tx.send_replace(status);
            self.controller.do_send(DeviceEvent::Status {
                key: self.key.clone(),
                status,
            });
        }
    }

    /// Use a new configuration. A new name is shown in the output names.
    fn set_config(&mut self, cfg: DeviceConfig) {
        let mut dirty = Dirty::NONE;
        if cfg.name != self.cfg.name {
            dirty |= self.s.set_name(&cfg.name);
        }
        if cfg.global_volume != self.cfg.global_volume {
            self.s.global_volume = cfg.global_volume;
            dirty |= Dirty::VOLUME;
        }
        if cfg.address != self.cfg.address {
            self.http_base = None;
        }
        self.cfg = cfg;
        self.emit(dirty);
    }

    fn client_options(&self) -> ClientOptions {
        let mut options = ClientOptions::default();
        options.name.clone_from(&self.ctx.client_name);
        options.stall_timeout = Some(STALL_TIMEOUT);
        options
    }

    fn connect(&mut self, ctx: &mut Context<Self>) {
        if !self.want
            || matches!(
                self.status,
                DeviceStatus::Connecting | DeviceStatus::Connected
            )
        {
            return;
        }
        self.set_status(DeviceStatus::Connecting);
        self.attempt += 1;
        let attempt = self.attempt;
        let cfg = self.cfg.clone();
        let version = self.cfg_version;
        let options = self.client_options();
        let known = self.endpoints;
        let start = Instant::now();
        let fut = connect_device(known, cfg, options);
        ctx.spawn(fut.into_actor(self).map(move |(cfg, result), act, ctx| {
            // What discovery found is valid also for an old attempt.
            if let Some(cfg) = cfg
                && act.cfg_version == version
            {
                act.set_config(cfg.clone());
                act.controller.do_send(DeviceEvent::Config {
                    key: act.key.clone(),
                    cfg,
                    version,
                });
            }
            if act.attempt != attempt {
                debug!(
                    "[{}] result of an old connect attempt ignored",
                    act.cfg.name
                );
                if let Ok(Connected { tv, .. }) = result {
                    actix::spawn(async move {
                        let _ = tv.close().await;
                    });
                }
                return;
            }
            match result {
                Ok(Connected { tv, ep, via, join })
                    if act.want && act.status == DeviceStatus::Connecting =>
                {
                    if act.cfg_version == version {
                        act.endpoints = Some(ep);
                    }
                    act.on_connected(tv, Link::New { via, join }, start, ctx);
                }
                Ok(Connected { tv, .. }) => {
                    actix::spawn(async move {
                        let _ = tv.close().await;
                    });
                }
                Err(ConnectError::Failed(e)) if pairing_not_valid(&e) => {
                    act.endpoints = None;
                    warn!(
                        "[{}] pairing is not valid anymore, pair again: {e}",
                        act.cfg.name
                    );
                    act.lost_connection(ctx);
                    act.set_status(DeviceStatus::AuthError);
                }
                Err(ConnectError::Failed(e)) if network_not_ready(&e) && act.network_retry(ctx) => {
                    debug!("[{}] network not ready: {e}", act.cfg.name);
                }
                Err(e) => {
                    match e {
                        ConnectError::Failed(e) => {
                            warn!("[{}] connect failed: {e}", act.cfg.name);
                        }
                        ConnectError::NotFound { pairing_failed } => {
                            info!("[{}] not found on the network", act.cfg.name);
                            // The device at the last endpoints may be another one.
                            if pairing_failed {
                                act.endpoints = None;
                            }
                        }
                    }
                    act.lost_connection(ctx);
                    act.schedule_reconnect(ctx);
                }
            }
        }));
    }

    /// Log the new handle. Returns the endpoints for the MRP join and `keep_companion`.
    fn log_link(&self, link: Link, ms: u128) -> (Option<Endpoints>, bool) {
        match link {
            Link::New { via, join: None } => {
                info!("[{}] connected in {ms} ms ({via})", self.cfg.name);
                (None, false)
            }
            Link::New {
                via,
                join: Some(ep),
            } => {
                info!(
                    "[{}] Companion connected in {ms} ms ({via}), MRP next",
                    self.cfg.name
                );
                (Some(ep), false)
            }
            Link::Missing {
                added,
                keep_companion,
            } => {
                info!("[{}] {added} connected in {ms} ms", self.cfg.name);
                (None, keep_companion)
            }
        }
    }

    /// Use a new handle. `start` is the start of the connect. While MRP joins, the MRP state
    /// stays as it is.
    fn on_connected(&mut self, tv: AppleTv, link: Link, start: Instant, ctx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let now = Instant::now();
        let (join, keep_companion) = self.log_link(link, (now - start).as_millis());
        let join_mrp = join.is_some();
        self.connected_at = Some(now);
        self.network_retry_end = None;
        self.command_timeouts.clear();

        let mut dirty = Dirty::CONNECTION;
        let mut any_missing = false;
        for protocol in missing(&tv) {
            any_missing = true;
            if join_mrp && protocol == Protocol::Mrp {
                continue;
            }
            warn!(
                "[{}] not connected: {}",
                self.cfg.name,
                lost_features(protocol)
            );
            dirty |= self.clear_protocol_state(protocol);
        }

        self.s.connected = true;
        // Companion is open, so only the MRP mode can need the join.
        self.s.siri = siri_available(&tv, self.ctx.siri_mode()) || join_mrp;
        if !join_mrp {
            let own_uid = tv.mrp().and_then(|m| m.device_info().device_uid.clone());
            if self.s.own_output_uid != own_uid {
                self.s.own_output_uid = own_uid;
                dirty |= Dirty::OUTPUTS;
            }
        }

        let rx = tv.now_playing();
        let h = spawn_stream(
            ctx,
            changes(rx.clone(), WatchChanged::NowPlaying(generation)),
        );
        replace_handle(ctx, &mut self.timers.now_playing, h);
        self.np = Some(rx);
        // `volume()` also returns the channel of a closed MRP client.
        if let Some(rx) = tv.volume().filter(|_| tv.is_open(Protocol::Mrp)) {
            let h = spawn_stream(ctx, changes(rx.clone(), WatchChanged::Volume(generation)));
            replace_handle(ctx, &mut self.timers.volume, h);
            self.volume = Some(rx);
        }
        // The stream of a kept Companion still runs.
        if !keep_companion && let Some(companion) = tv.companion() {
            self.companion_gen += 1;
            let companion_gen = self.companion_gen;
            let events = BroadcastStream::new(companion.events()).map(move |e| CompanionEvent {
                companion_gen,
                event: e.ok(),
            });
            let h = spawn_stream(ctx, events);
            replace_handle(ctx, &mut self.timers.companion, h);
        }

        let mrp_open = tv.is_open(Protocol::Mrp);
        self.tv = Some(tv);
        // Before the status: a command that waits for the connect then waits for MRP too.
        if join_mrp {
            self.start_mrp_join();
        } else {
            self.end_mrp_join();
        }
        self.watch_closed(ctx);
        self.set_status(DeviceStatus::Connected);
        // A kept Companion has these from the handle before.
        if !keep_companion {
            // One request at a time: ratv closes Companion after two silent timeouts in a row.
            self.query_power(ctx, 0, true);
        }
        // Until the device sends its state, the channels hold empty values. Using them would clear
        // what the Remote shows, so they are used only when they have data, after the first
        // update or after a wait. The first value can already have data without a change mark.
        if self.np_has_data() {
            dirty |= self.update_now_playing(ctx);
        }
        if self
            .volume
            .as_ref()
            .is_some_and(|rx| rx.has_changed().unwrap_or(false))
        {
            dirty |= self.update_volume();
        }
        if mrp_open && self.artwork_after_mrp {
            dirty |= self.update_artwork(ctx);
        }
        ctx.run_later(FIRST_STATE_WAIT, move |act, ctx| {
            if act.generation == generation {
                let dirty = act.update_now_playing(ctx) | act.update_volume();
                act.emit(dirty);
            }
        });
        self.emit(dirty);

        self.scan_outputs(ctx);
        self.timers.poll = Some(ctx.run_interval(POLL_INTERVAL, Self::poll));
        // `watch_closed` sees only the open protocols, so the missing ones are tried again here.
        if any_missing {
            self.start_missing(join, ctx);
        }
    }

    /// The now playing channel has data from the device.
    fn np_has_data(&self) -> bool {
        self.np.as_ref().is_some_and(|rx| {
            rx.has_changed().unwrap_or(false) || *rx.borrow() != NowPlaying::default()
        })
    }

    /// Wait until one of the open protocols closes. When the other protocol is still open, it
    /// stays in use and the closed one is connected again.
    fn watch_closed(&mut self, ctx: &mut Context<Self>) {
        let Some(tv) = &self.tv else { return };
        // `AppleTv::closed` returns at once after one protocol closed, so wait on the open ones.
        let companion = tv.companion().filter(|c| c.is_connected()).cloned();
        let mrp = tv.mrp().filter(|m| m.is_connected()).cloned();
        let generation = self.generation;
        let fut = async move {
            // Both closed before this check: the callback sees no open protocol and reconnects.
            if companion.is_none() && mrp.is_none() {
                return (Protocol::Companion, None);
            }
            let companion = async {
                match &companion {
                    Some(c) => c.closed().await,
                    None => std::future::pending().await,
                }
            };
            let mrp = async {
                match &mrp {
                    Some(m) => m.closed().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                reason = companion => (Protocol::Companion, reason),
                reason = mrp => (Protocol::Mrp, reason),
            }
        };
        let handle = ctx.spawn(
            fut.into_actor(self)
                .map(move |(protocol, reason), act, ctx| {
                    if act.generation != generation {
                        return;
                    }
                    act.timers.closed = None;
                    let open = act.tv.as_ref().is_some_and(|tv| {
                        tv.is_open(Protocol::Companion) || tv.is_open(Protocol::Mrp)
                    });
                    match (&reason, open) {
                        (Some(e), true) => warn!(
                            "[{}] {protocol:?} closed: {e}. {}",
                            act.cfg.name,
                            lost_features(protocol)
                        ),
                        (None, true) => warn!(
                            "[{}] {protocol:?} closed. {}",
                            act.cfg.name,
                            lost_features(protocol)
                        ),
                        (Some(e), false) => warn!("[{}] connection closed: {e}", act.cfg.name),
                        (None, false) => info!("[{}] connection closed", act.cfg.name),
                    }
                    if open {
                        act.protocol_lost(protocol, ctx);
                    } else {
                        act.lost_connection(ctx);
                        act.schedule_reconnect(ctx);
                    }
                }),
        );
        self.timers.closed = Some(handle);
    }

    /// One protocol closed and the other is still open. Show what is gone and connect it again.
    /// This is not an MRP join: commands do not wait, and the full update clears the MRP fields,
    /// because a lost MRP can stay lost.
    fn protocol_lost(&mut self, protocol: Protocol, ctx: &mut Context<Self>) {
        let Some(tv) = &self.tv else { return };
        let mut dirty = Dirty::NONE;
        let siri = siri_available(tv, self.ctx.siri_mode());
        if self.s.siri != siri {
            self.s.siri = siri;
            dirty |= Dirty::CONNECTION;
        }
        dirty |= self.clear_protocol_state(protocol);
        self.emit(dirty);
        self.watch_closed(ctx);
        self.start_missing(None, ctx);
    }

    /// Clear the state that comes from `protocol`, which is not connected.
    fn clear_protocol_state(&mut self, protocol: Protocol) -> Dirty {
        match protocol {
            Protocol::Mrp => {
                self.expected_mute = None;
                // A running fast forward or rewind would block the retry of MRP.
                self.seek = None;
                // The volume channel ends with MRP. Now playing goes on with Companion data.
                self.volume = None;
                self.s.update_volume(&Volume::default())
            }
            Protocol::Companion => {
                // Results of requests to the closed client are not used.
                self.companion_gen += 1;
                self.attention = None;
                self.media_flags = None;
                self.expected_power = None;
                self.s.set_power(Power::Unknown)
            }
            _ => Dirty::NONE,
        }
    }

    /// Try the missing protocols now and then at each [`MISSING_RETRY_INTERVAL`].
    /// Now, because after a restart of the device the connect is refused and a scan finds the new
    /// port in about 2 s.
    /// `ep` is used for this try instead of the known endpoints.
    fn start_missing(&mut self, ep: Option<Endpoints>, ctx: &mut Context<Self>) {
        if self.timers.missing.is_none() {
            self.timers.missing =
                Some(ctx.run_interval(MISSING_RETRY_INTERVAL, Self::retry_missing));
        }
        self.retry_missing_at(ep, ctx);
    }

    /// Drop the connection and mark the entities unavailable.
    fn lost_connection(&mut self, ctx: &mut Context<Self>) {
        self.close(ctx);
        self.set_status(DeviceStatus::Disconnected);
        if self.s.connected {
            let dirty = self.s.disconnected();
            self.emit(dirty);
        }
    }

    /// Close the connection and stop all tasks of it.
    fn close(&mut self, ctx: &mut Context<Self>) {
        self.generation += 1;
        self.companion_gen += 1;
        self.attempt += 1;
        // A connection that drops at once keeps the reconnect delay growing.
        if self
            .connected_at
            .take()
            .is_some_and(|t| t.elapsed() >= STABLE_CONNECTION)
        {
            self.attempts = 0;
        }
        self.np = None;
        self.volume = None;
        self.attention = None;
        self.seek = None;
        self.siri = None;
        if self.home_since.take().is_some() {
            debug!(
                "[{}] Home Screen clear ended: connection closed",
                self.cfg.name
            );
        }
        self.media_flags = None;
        self.http_base = None;
        self.expected_mute = None;
        self.expected_power = None;
        self.missing_running = false;
        self.missing_network_end = None;
        self.end_mrp_join();
        self.artwork_after_mrp = false;
        for handle in [
            self.timers.poll.take(),
            self.timers.chapter.take(),
            self.timers.missing.take(),
            self.timers.closed.take(),
            self.timers.screen.take(),
            self.timers.scan.take(),
            self.timers.now_playing.take(),
            self.timers.volume.take(),
            self.timers.companion.take(),
        ]
        .into_iter()
        .flatten()
        {
            ctx.cancel_future(handle);
        }
        if let Some(tv) = self.tv.take() {
            actix::spawn(async move {
                let _ = tv.close().await;
            });
        }
    }

    fn cancel_timers(&mut self, ctx: &mut Context<Self>) {
        let t = std::mem::take(&mut self.timers);
        for handle in [
            t.reconnect,
            t.poll,
            t.chapter,
            t.missing,
            t.closed,
            t.screen,
            t.scan,
            t.now_playing,
            t.volume,
            t.companion,
        ]
        .into_iter()
        .flatten()
        {
            ctx.cancel_future(handle);
        }
    }

    fn schedule_reconnect(&mut self, ctx: &mut Context<Self>) {
        if !self.want {
            return;
        }
        self.cancel_timers(ctx);
        self.attempts = self.attempts.saturating_add(1);
        let delay = Duration::from_secs(u64::from(self.attempts) * 2).min(MAX_BACKOFF);
        debug!("[{}] reconnect in {}s", self.cfg.name, delay.as_secs());
        self.timers.reconnect = Some(ctx.run_later(delay, |act, ctx| {
            act.timers.reconnect = None;
            act.connect(ctx);
        }));
    }

    /// Connect again soon while the network is not ready. Returns `false` when the time for this is over.
    fn network_retry(&mut self, ctx: &mut Context<Self>) -> bool {
        if !retry_window_open(&mut self.network_retry_end, Instant::now()) {
            warn!("[{}] network not ready", self.cfg.name);
            return false;
        }
        self.status = DeviceStatus::Disconnected;
        self.timers.reconnect = Some(ctx.run_later(NETWORK_RETRY_INTERVAL, |act, ctx| {
            act.timers.reconnect = None;
            act.connect(ctx);
        }));
        true
    }

    /// Connect again if too many commands timed out in a short time.
    ///
    /// The connection can look open while one protocol does not answer.
    /// Commands that work do not reset the count, because they can use the other protocol.
    fn command_timed_out(&mut self, ctx: &mut Context<Self>) {
        let now = Instant::now();
        if self.command_timeouts.len() == MAX_COMMAND_TIMEOUTS {
            self.command_timeouts.pop_front();
        }
        self.command_timeouts.push_back(now);
        if self.command_timeouts.len() < MAX_COMMAND_TIMEOUTS
            || self
                .command_timeouts
                .front()
                .is_some_and(|t| now - *t > COMMAND_TIMEOUT_WINDOW)
        {
            return;
        }
        warn!(
            "[{}] {MAX_COMMAND_TIMEOUTS} command timeouts in {} s, reconnecting",
            self.cfg.name,
            COMMAND_TIMEOUT_WINDOW.as_secs()
        );
        self.command_timeouts.clear();
        self.lost_connection(ctx);
        self.connect(ctx);
    }

    /// Start a connect if none runs. A failed pairing is tried again.
    fn start_connect(&mut self, ctx: &mut Context<Self>) {
        self.want = true;
        if self.status == DeviceStatus::AuthError {
            self.set_status(DeviceStatus::Disconnected);
        }
        if self.status == DeviceStatus::Disconnected {
            if let Some(handle) = self.timers.reconnect.take() {
                ctx.cancel_future(handle);
            }
            self.attempts = 0;
            // A running fast network retry keeps its end, so key presses cannot extend it.
            self.network_retry_end = running_window(self.network_retry_end, Instant::now());
            self.connect(ctx);
        }
    }

    /// Connect the missing protocols: first at the known endpoints, then after a scan if the
    /// device refused the connect or an endpoint is not known. The open protocols stay connected.
    /// Skipped while Siri or a fast forward or rewind is active: a new handle restarts the tasks
    /// of the connection, and a new MRP connection does not know about a running fast forward.
    /// Not skipped when the MRP join starts: commands wait for it a short time, Siri over MRP
    /// until it ends, and no fast forward runs yet. The check holds only at the start: a fast
    /// forward that starts after the short wait can end through the new handle after the join.
    /// Later tries, after the join ended, are skipped as above.
    fn retry_missing(&mut self, ctx: &mut Context<Self>) {
        self.retry_missing_at(None, ctx);
    }

    /// [`Self::retry_missing`] with `ep` instead of the known endpoints, if set.
    fn retry_missing_at(&mut self, ep: Option<Endpoints>, ctx: &mut Context<Self>) {
        let Some(tv) = self.tv.clone() else { return };
        if self.missing_running {
            return;
        }
        if !self.joining() && (self.siri.is_some() || self.seek.is_some()) {
            debug!(
                "[{}] missing protocol retry skipped: device is busy",
                self.cfg.name
            );
            self.missing_network_end = None;
            return;
        }
        self.missing_running = true;
        let start = Instant::now();
        let generation = self.generation;
        let version = self.cfg_version;
        let mut cfg = self.cfg.clone();
        let ep = ep.or(self.endpoints).unwrap_or_default();
        let options = self.client_options();
        let fut = async move {
            if has_missing_endpoints(&tv, ep) {
                match tv.connect_missing(&ep, &cfg.credentials, &options).await {
                    // The service may listen on another port now.
                    Err(e) if e.is_refused() => {
                        debug!("[{}] missing protocols refused, scanning: {e}", cfg.name);
                    }
                    result => return (None, ep, result),
                }
            }
            let found = match discovery::find(&cfg).await {
                Ok(found) => found,
                Err(e) => {
                    debug!("[{}] scan failed: {e}", cfg.name);
                    None
                }
            };
            let changed = found
                .as_ref()
                .is_some_and(|d| discovery::update_config(&mut cfg, d));
            let ep = found
                .as_ref()
                .map_or(ep, |d| merge_endpoints(ep, Endpoints::from(d)));
            // Also with the same ports: a refused connect costs little, and the service can start
            // again on the same port.
            let result = tv.connect_missing(&ep, &cfg.credentials, &options).await;
            (changed.then_some(cfg), ep, result)
        };
        ctx.spawn(
            fut.into_actor(self)
                .map(move |(cfg, ep, result), act, ctx| {
                    if let Some(cfg) = cfg
                        && act.cfg_version == version
                    {
                        act.set_config(cfg.clone());
                        act.controller.do_send(DeviceEvent::Config {
                            key: act.key.clone(),
                            cfg,
                            version,
                        });
                    }
                    if act.generation != generation {
                        if let Ok(Some(tv)) = result {
                            actix::spawn(async move {
                                let _ = tv.close().await;
                            });
                        }
                        return;
                    }
                    act.missing_running = false;
                    if act.cfg_version == version {
                        act.endpoints = Some(ep);
                    }
                    let joined = act.end_mrp_join();
                    if joined && let Err(e) = &result {
                        act.mrp_join_failed(e, ctx);
                    }
                    match result {
                        Err(e) if network_not_ready(&e) => act.missing_network_retry(ctx),
                        // Retries cannot fix the pairing. The next connect tries again.
                        Err(e) if pairing_not_valid(&e) => act.stop_missing(&e, joined, ctx),
                        Err(e) => {
                            act.missing_network_end = None;
                            // `mrp_join_failed` has logged it.
                            if !joined {
                                debug!("[{}] missing protocols did not connect: {e}", act.cfg.name);
                            }
                        }
                        Ok(Some(tv)) => {
                            act.missing_network_end = None;
                            act.use_handle(tv, start, ctx);
                        }
                        Ok(None) => act.missing_network_end = None,
                    }
                }),
        );
    }

    /// MRP did not connect after a connect with Companion only. Show what is missing and clear
    /// the MRP fields of now playing, as a connect without MRP does.
    fn mrp_join_failed(&mut self, e: &ratv::Error, ctx: &mut Context<Self>) {
        let Some(tv) = &self.tv else { return };
        warn!(
            "[{}] MRP did not connect: {e}. {}",
            self.cfg.name,
            lost_features(Protocol::Mrp)
        );
        let mut dirty = Dirty::NONE;
        let siri = siri_available(tv, self.ctx.siri_mode());
        if self.s.siri != siri {
            self.s.siri = siri;
            dirty |= Dirty::CONNECTION;
        }
        if self.s.own_output_uid.take().is_some() {
            dirty |= Dirty::OUTPUTS;
        }
        dirty |= self.clear_protocol_state(Protocol::Mrp);
        // After the first state wait, a connect without MRP has also cleared the MRP fields.
        if self.np_has_data()
            || self
                .connected_at
                .is_some_and(|t| t.elapsed() >= FIRST_STATE_WAIT)
        {
            dirty |= self.update_now_playing(ctx);
        }
        self.emit(dirty);
    }

    /// Try the missing protocols again soon while the network is not ready, for example right
    /// after wake-up. When the time for this is over, the next try comes from the interval.
    fn missing_network_retry(&mut self, ctx: &mut Context<Self>) {
        if !retry_window_open(&mut self.missing_network_end, Instant::now()) {
            debug!(
                "[{}] network not ready, missing protocols are tried again in {} s",
                self.cfg.name,
                MISSING_RETRY_INTERVAL.as_secs()
            );
            self.missing_network_end = None;
            return;
        }
        let generation = self.generation;
        ctx.run_later(NETWORK_RETRY_INTERVAL, move |act, ctx| {
            if act.generation == generation {
                act.retry_missing(ctx);
            }
        });
    }

    /// Stop connecting the missing protocols after a pairing error. Retries cannot fix the
    /// pairing. The next connect tries again. With `joined`, `mrp_join_failed` has logged it.
    fn stop_missing(&mut self, e: &ratv::Error, joined: bool, ctx: &mut Context<Self>) {
        self.missing_network_end = None;
        if let Some(handle) = self.timers.missing.take() {
            ctx.cancel_future(handle);
        }
        if joined {
            info!("[{}] no more tries until the next connect", self.cfg.name);
        } else {
            warn!(
                "[{}] missing protocols not connected, pairing not accepted: {e}. No more tries \
                 until the next connect",
                self.cfg.name
            );
        }
    }

    /// Use a handle that has more protocols than the current one. `start` is the start of the
    /// connect. Both handles share the open protocols, so the old one is dropped without close.
    /// Requests and events of a Companion that stays open go on.
    fn use_handle(&mut self, tv: AppleTv, start: Instant, ctx: &mut Context<Self>) {
        let open = |tv: Option<&AppleTv>, p| tv.is_some_and(|tv| tv.is_open(p));
        let old = self.tv.as_ref();
        let keep_companion = open(old, Protocol::Companion) && open(Some(&tv), Protocol::Companion);
        let added = match (
            !open(old, Protocol::Companion) && open(Some(&tv), Protocol::Companion),
            !open(old, Protocol::Mrp) && open(Some(&tv), Protocol::Mrp),
        ) {
            (true, true) => "Companion and MRP",
            (true, false) => "Companion",
            (false, true) => "MRP",
            (false, false) => "no new protocol",
        };
        for handle in [
            self.timers.poll.take(),
            self.timers.missing.take(),
            self.timers.closed.take(),
            self.timers.scan.take(),
            // The Companion stream goes on if Companion stays open.
            self.timers.now_playing.take(),
            self.timers.volume.take(),
        ]
        .into_iter()
        .flatten()
        {
            ctx.cancel_future(handle);
        }
        self.tv = None;
        self.np = None;
        self.volume = None;
        let connected_at = self.connected_at;
        let link = Link::Missing {
            added,
            keep_companion,
        };
        self.on_connected(tv, link, start, ctx);
        // The connection did not drop, so the time for the reconnect delay stays.
        self.connected_at = connected_at;
    }

    /// Ask for the power state. `attempt` counts the tries, see [`NOT_READY_RETRY`]. With
    /// `then_poll`, the apps and accounts are fetched after it.
    fn query_power(&self, ctx: &mut Context<Self>, attempt: usize, then_poll: bool) {
        let Some(tv) = self.tv.clone() else { return };
        if !tv.is_open(Protocol::Companion) {
            return;
        }
        let companion_gen = self.companion_gen;
        ctx.spawn(
            async move {
                match tv.companion() {
                    Some(c) => c.power_state().await,
                    None => Err(ratv::Error::NotConnected("Companion")),
                }
            }
            .into_actor(self)
            .map(move |result, act, ctx| {
                if act.companion_gen != companion_gen {
                    return;
                }
                match result {
                    Ok(state) => act.set_attention(state),
                    Err(e) => {
                        if let Some(&delay) = NOT_READY_RETRY.get(attempt) {
                            debug!(
                                "[{}] power state failed: {e}, asking again in {}s",
                                act.cfg.name,
                                delay.as_secs()
                            );
                            ctx.run_later(delay, move |act, ctx| {
                                if act.companion_gen == companion_gen {
                                    act.query_power(ctx, attempt + 1, then_poll);
                                }
                            });
                            return;
                        }
                        warn!("[{}] power state failed: {e}", act.cfg.name);
                    }
                }
                if then_poll {
                    act.poll_companion(ctx, 0);
                }
            }),
        );
    }

    fn set_attention(&mut self, state: AttentionState) {
        self.attention = Some(state);
        self.expected_power = None;
        let dirty = self.s.set_power(state.into());
        self.emit(dirty);
    }

    fn update_now_playing(&mut self, ctx: &mut Context<Self>) -> Dirty {
        if let Some(since) = self.home_since.filter(|t| t.elapsed() < HOME_GRACE) {
            // Read again when the wait ends, but only if something plays: the update can also
            // still be the app from before the Home Screen.
            if !self.home_dropped {
                self.home_dropped = true;
                // The Home Screen comes from Companion, so an MRP join does not stop the timer.
                let companion_gen = self.companion_gen;
                ctx.run_later(
                    HOME_GRACE.saturating_sub(since.elapsed()),
                    move |act, ctx| {
                        act.home_dropped = false;
                        let playing = act
                            .np
                            .as_ref()
                            .is_some_and(|rx| rx.borrow().state == Some(PlaybackState::Playing));
                        if act.companion_gen == companion_gen && act.home_since.is_some() && playing
                        {
                            let dirty = act.update_now_playing(ctx);
                            act.emit(dirty);
                        }
                    },
                );
            }
            return Dirty::NONE;
        }
        let Some(rx) = &self.np else {
            return Dirty::NONE;
        };
        if self.home_since.take().is_some() {
            debug!(
                "[{}] now playing again after the Home Screen",
                self.cfg.name
            );
        }
        let (state, rate) = (self.s.state, self.s.rate);
        let dirty = if self.joining() {
            self.s.update_companion_now_playing(&rx.borrow())
        } else {
            self.s.update_now_playing(&rx.borrow())
        };
        self.log_playback(state, rate);
        self.apply_now_playing(dirty, ctx)
    }

    /// Log a change of the playback state or rate. `state` and `rate` are the values before.
    fn log_playback(&self, state: Option<PlaybackState>, rate: Option<f32>) {
        if state != self.s.state || rate != self.s.rate {
            debug!(
                "[{}] playback: {state:?} at rate {rate:?} -> {:?} at rate {:?}",
                self.cfg.name, self.s.state, self.s.rate
            );
        }
    }

    fn apply_now_playing(&mut self, dirty: Dirty, ctx: &mut Context<Self>) -> Dirty {
        // A fast forward or rewind ended on the device, for example at the end of the item.
        if self.seek.is_some()
            && dirty.any(Dirty::RATE)
            && self
                .s
                .rate
                .is_some_and(|r| r.abs() < 0.01 || (r - 1.0).abs() < 0.01)
        {
            debug!("[{}] fast forward or rewind ended", self.cfg.name);
            self.seek = None;
        }
        if dirty.any(Dirty::LANGUAGES | Dirty::LANGUAGE) && log_enabled!(Level::Debug) {
            self.log_languages();
        }
        if dirty.any(Dirty::METADATA | Dirty::ARTWORK) {
            debug!(
                "[{}] item: {:?} / {:?} / {:?}, artwork id {:?}",
                self.cfg.name, self.s.title, self.s.artist, self.s.album, self.s.artwork_id
            );
        }
        if dirty.any(Dirty::ARTWORK) {
            // `dirty` already has the flag that `update_artwork` returns.
            self.update_artwork(ctx);
        }
        if dirty.any(Dirty::CHAPTERS | Dirty::POSITION | Dirty::PLAYBACK) {
            self.schedule_chapter(ctx);
        }
        dirty
    }

    /// The screen changed. If nothing plays and no app has media controls, the Home Screen
    /// (or an app without media) shows: clear now playing.
    fn screen_changed(&mut self, ctx: &mut Context<Self>) {
        if let Some(handle) = self.timers.screen.take() {
            ctx.cancel_future(handle);
        }
        // Screen events come from Companion, so an MRP join does not stop the timer.
        let companion_gen = self.companion_gen;
        self.timers.screen = Some(ctx.run_later(SCREEN_SETTLE, move |act, ctx| {
            act.timers.screen = None;
            // Another screen opened after the launch: the launched app may not be in front.
            if act.launched_at.is_some_and(|t| t.elapsed() > LAUNCH_GRACE)
                && act.s.launched_app.take().is_some()
            {
                act.launched_at = None;
                act.emit(Dirty::APP);
            }
            if act.companion_gen != companion_gen
                || act.home_since.is_some()
                || act.s.state == Some(PlaybackState::Playing)
                || act.media_flags.is_some_and(|f| f != 0)
            {
                return;
            }
            debug!(
                "[{}] Home Screen: clearing now playing (state {:?}, rate {:?})",
                act.cfg.name, act.s.state, act.s.rate
            );
            act.home_since = Some(Instant::now());
            let (state, rate) = (act.s.state, act.s.rate);
            let dirty = act.s.update_now_playing(&NowPlaying::default());
            act.log_playback(state, rate);
            let dirty = act.apply_now_playing(dirty, ctx);
            act.emit(dirty);
        }));
    }

    /// An app has media controls again: show now playing.
    fn leave_home(&mut self, ctx: &mut Context<Self>) {
        if self.home_since.is_some() {
            let dirty = self.update_now_playing(ctx);
            self.emit(dirty);
        }
    }

    fn update_volume(&mut self) -> Dirty {
        let Some(rx) = &self.volume else {
            return Dirty::NONE;
        };
        let dirty = self.s.update_volume(&rx.borrow());
        if dirty.any(Dirty::OUTPUTS) && log_enabled!(Level::Debug) {
            let outputs: Vec<(&str, &str)> = self
                .s
                .outputs
                .iter()
                .map(|o| (o.uid.as_str(), o.name.as_str()))
                .collect();
            debug!(
                "[{}] outputs: {outputs:?}, own uid: {:?}",
                self.cfg.name, self.s.own_output_uid
            );
        }
        dirty
    }

    /// Current and available languages, to compare the ids.
    fn log_languages(&self) {
        let ids = |list: &[ratv::mrp::LanguageOption]| -> Vec<(Option<String>, Option<String>)> {
            list.iter()
                .map(|o| (o.identifier.clone(), o.language_tag.clone()))
                .collect()
        };
        let id = |o: Option<&ratv::mrp::LanguageOption>| {
            o.map(|o| (o.identifier.clone(), o.language_tag.clone()))
        };
        debug!(
            "[{}] audio: {:?} of {:?}, subtitles: {:?} of {:?}, auto: {}, shown: {:?}",
            self.cfg.name,
            id(self.s.audio_language.as_ref()),
            ids(&self.s.audio_languages),
            id(self.s.subtitle_language.as_ref()),
            ids(&self.s.subtitle_languages),
            self.s.subtitles_auto,
            self.s.captions_enabled
        );
    }

    /// Stop showing the image of an older item, unless `Snapshot::keep_artwork` keeps it.
    fn clear_older_artwork(&mut self) -> Dirty {
        let dirty = self.s.clear_older_artwork_url();
        if self.s.artwork_url.is_none() {
            self.shown_file = None;
        }
        dirty
    }

    /// Use the artwork URL, or serve the image from a file. The image comes from Companion, or is
    /// fetched over MRP.
    ///
    /// The image is also fetched without an artwork id: some apps send none, but the Apple TV
    /// has the image. It is not fetched for an item without title, subtitle, artist, album,
    /// series name and artwork id.
    ///
    /// Returns [`Dirty::ARTWORK`] if the shown image was cleared here.
    fn update_artwork(&mut self, ctx: &mut Context<Self>) -> Dirty {
        self.artwork_seq += 1;
        self.artwork_after_mrp = false;
        // No URL means no file is shown: the file before must not pass to this item.
        if self.s.artwork_url.is_none() {
            self.shown_file = None;
        }
        if !self.s.artwork_fetch {
            if let Some(url) = &self.s.artwork_url {
                debug!("[{}] artwork url: {url}", self.cfg.name);
                // A CDN URL is not a file.
                self.shown_file = None;
            }
            return Dirty::NONE;
        }
        let Some(tv) = self.tv.clone().filter(|_| self.s.state.is_some()) else {
            debug!("[{}] no artwork: nothing plays", self.cfg.name);
            return Dirty::NONE;
        };
        let id = self.s.artwork_id.clone();
        let seq = self.artwork_seq;
        let item = self.s.item_seq;
        let item_key = self.s.item_key();
        // Items without title, artist and album share one key: no image from the cache.
        let keyed = self.s.has_item_key();
        if let Some(data) = self.s.artwork_data.clone() {
            debug!("[{}] artwork {id:?} from Companion", self.cfg.name);
            let id = id.unwrap_or_else(|| format!("{:016x}", fnv1a(&data)));
            return self.save_artwork(&id, None, data, (item, item_key));
        }
        // A fetch fails while the app has no player. The image of the item before must not stay.
        if self.s.item_has_no_artwork() {
            debug!(
                "[{}] no artwork: no item metadata and no artwork id",
                self.cfg.name
            );
            self.shown_file = None;
            return self.s.clear_artwork_url();
        }
        if !tv.is_open(Protocol::Mrp) {
            debug!(
                "[{}] artwork {id:?} is fetched when MRP connects",
                self.cfg.name
            );
            return self.wait_for_mrp_artwork(keyed, item, item_key);
        }
        debug!("[{}] fetching artwork {id:?}", self.cfg.name);
        let generation = self.generation;
        // The Apple TV scales the image.
        ctx.spawn(
            async move { tv.artwork(ARTWORK_SIZE, 0).await }
                .into_actor(self)
                .map(move |result, act, _| {
                    if act.generation != generation || act.artwork_seq != seq {
                        return;
                    }
                    match result {
                        Ok(Some(art)) => {
                            // Without an artwork id, the image data gives a stable file name.
                            let id = id.unwrap_or_else(|| format!("{:016x}", fnv1a(&art.data)));
                            let dirty = act.save_artwork(
                                &id,
                                art.mime_type.as_deref(),
                                art.data,
                                (item, item_key),
                            );
                            act.emit(dirty);
                        }
                        // Some apps drop the image of the same item. Keep it until the item changes.
                        Ok(None) if act.s.artwork_item == item && act.s.artwork_url.is_some() => {
                            debug!(
                                "[{}] no artwork, keeping the image of this item",
                                act.cfg.name
                            );
                        }
                        Ok(None) if act.s.keep_artwork(item) => {
                            debug!(
                                "[{}] no artwork, keeping the image of the short item before ({:?})",
                                act.cfg.name, act.s.prev_item_lasted
                            );
                            act.s.artwork_item = item;
                            act.s.artwork_kept = true;
                            if let Some(name) = act.shown_file.clone() {
                                act.item_art.remember(item_key, name);
                            }
                        }
                        Ok(None) => {
                            if keyed
                                && let Some(name) = act.cached_art(item_key)
                                && act.show_file(name, item)
                            {
                                debug!("[{}] no artwork, using the image seen before for this item", act.cfg.name);
                                return;
                            }
                            debug!(
                                "[{}] no artwork for this item, item before lasted {:?}",
                                act.cfg.name, act.s.prev_item_lasted
                            );
                            act.shown_file = None;
                            if act.s.clear_artwork_url().any(Dirty::ARTWORK) {
                                act.emit(Dirty::ARTWORK);
                            }
                        }
                        Err(e) => warn!("[{}] artwork failed: {e}", act.cfg.name),
                    }
                }),
        );
        Dirty::NONE
    }

    /// Fetch the artwork when MRP connects. Until then, show the image seen before for this
    /// item, else stop showing the image of an older item. `item` and `item_key` are the item
    /// sequence and the item key. Items without key get no image from the cache. The image of
    /// this item comes before the image of a short item before (see `Snapshot::keep_artwork`).
    /// Returns [`Dirty::ARTWORK`] if the image was cleared here.
    fn wait_for_mrp_artwork(&mut self, keyed: bool, item: u32, item_key: u64) -> Dirty {
        self.artwork_after_mrp = true;
        let shown = self.s.artwork_item == item && self.s.artwork_url.is_some();
        if keyed
            && !shown
            && let Some(name) = self.cached_art(item_key)
            && self.show_file(name, item)
        {
            debug!(
                "[{}] showing the image seen before for this item",
                self.cfg.name
            );
            return Dirty::NONE;
        }
        self.clear_older_artwork()
    }

    /// Keep the image and show it. `item` is the item sequence and the item key at fetch time.
    /// Without a URL for the file, the image of an older item is not shown any more. Returns
    /// [`Dirty::ARTWORK`] if it was cleared here.
    fn save_artwork(
        &mut self,
        id: &str,
        mime_type: Option<&str>,
        data: Bytes,
        (item, item_key): (u32, u64),
    ) -> Dirty {
        let name = self.ctx.artwork.save(&self.key, id, mime_type, data);
        self.item_art.remember(item_key, name.clone());
        if self.show_file(name, item) {
            Dirty::NONE
        } else {
            self.clear_older_artwork()
        }
    }

    /// Show a kept artwork file. Returns `false` if there is no local address for the URL.
    #[must_use]
    fn show_file(&mut self, name: Box<str>, item: u32) -> bool {
        let remote = self.ctx.remote_ip();
        if self.http_base.as_ref().is_none_or(|(ip, _)| *ip != remote) {
            self.http_base = self.find_http_base(remote).map(|base| (remote, base));
        }
        let Some((_, base)) = &self.http_base else {
            warn!("[{}] no local address for the artwork URL", self.cfg.name);
            return false;
        };
        let url = format!("{base}/artwork/{}/{name}", self.key);
        debug!("[{}] artwork file: {url}", self.cfg.name);
        self.s.artwork_url = Some(url);
        self.s.artwork_item = item;
        self.s.artwork_kept = false;
        self.shown_file = Some(name);
        self.emit(Dirty::ARTWORK);
        true
    }

    /// The file seen before for the item, if it is still kept.
    fn cached_art(&self, item_key: u64) -> Option<Box<str>> {
        let name = self.item_art.get(item_key)?;
        self.ctx.artwork.touch(&self.key, name).then(|| name.into())
    }

    /// Base URL of the HTTP server. `remote`: local address of the last Remote connection.
    fn find_http_base(&self, remote: Option<IpAddr>) -> Option<String> {
        http_base(
            self.ctx.http_address,
            remote,
            self.cfg.address,
            self.ctx.http_ipv6,
            self.ctx.http_port,
        )
    }

    /// Update the chapter select when the next chapter starts.
    fn schedule_chapter(&mut self, ctx: &mut Context<Self>) {
        let delay = self.next_chapter_delay();
        // Most updates do not move the next chapter start: keep the timer.
        if let (Some(delay), Some(due), Some(_)) = (delay, self.chapter_due, &self.timers.chapter) {
            let new_due = Instant::now() + delay;
            let diff = new_due.max(due) - new_due.min(due);
            if diff < CHAPTER_TIMER_MARGIN {
                return;
            }
        }
        if let Some(handle) = self.timers.chapter.take() {
            ctx.cancel_future(handle);
        }
        self.chapter_due = None;
        let Some(delay) = delay else { return };
        self.chapter_due = Some(Instant::now() + delay);
        self.timers.chapter = Some(ctx.run_later(delay, |act, ctx| {
            act.timers.chapter = None;
            act.chapter_due = None;
            act.emit(Dirty::CHAPTER);
            act.schedule_chapter(ctx);
        }));
    }

    /// Time until the next chapter starts, if one starts while playing.
    fn next_chapter_delay(&self) -> Option<Duration> {
        let rate = f64::from(self.s.rate.unwrap_or(0.0));
        if rate <= 0.0 || self.s.chapters.is_empty() {
            return None;
        }
        let pos = self.s.position(now())?;
        let next = self
            .s
            .chapters
            .iter()
            .filter_map(|c| c.start)
            .find(|&start| start > pos + 0.5)?;
        chapter_delay(next - pos, rate)
    }

    /// Fetch the apps, accounts and AirPlay devices.
    fn poll(&mut self, ctx: &mut Context<Self>) {
        self.poll_companion(ctx, 0);
        self.scan_outputs(ctx);
    }

    /// Fetch the apps and accounts. `attempt` counts the tries, see [`NOT_READY_RETRY`].
    fn poll_companion(&self, ctx: &mut Context<Self>, attempt: usize) {
        let Some(tv) = self.tv.clone().filter(|tv| tv.is_open(Protocol::Companion)) else {
            return;
        };
        let companion_gen = self.companion_gen;
        let fetch_apps = poll_due(self.apps_fetched);
        // One after the other: ratv closes Companion after two silent timeouts in a row.
        let fut = async move {
            let apps = if fetch_apps {
                Some(tv.apps().await)
            } else {
                None
            };
            (apps, tv.accounts().await)
        };
        ctx.spawn(fut.into_actor(self).map(move |(apps, accounts), act, ctx| {
            if act.companion_gen != companion_gen {
                return;
            }
            let apps_error = act.apply_apps(apps);
            let accounts_error = match accounts {
                Ok(accounts) => {
                    let accounts = accounts
                        .into_iter()
                        .map(|a| AccountInfo {
                            id: a.id,
                            name: a.name,
                        })
                        .collect();
                    let dirty = act.s.set_accounts(accounts);
                    act.emit(dirty);
                    None
                }
                Err(e) => Some(e),
            };
            if apps_error.is_none() && accounts_error.is_none() {
                return;
            }
            if let Some(&delay) = NOT_READY_RETRY.get(attempt) {
                debug!(
                    "[{}] app list: {apps_error:?}, account list: {accounts_error:?}, \
                     asking again in {}s",
                    act.cfg.name,
                    delay.as_secs()
                );
                ctx.run_later(delay, move |act, ctx| {
                    if act.companion_gen == companion_gen {
                        act.poll_companion(ctx, attempt + 1);
                    }
                });
                return;
            }
            if let Some(e) = apps_error {
                warn!("[{}] app list failed: {e}", act.cfg.name);
            }
            if let Some(e) = accounts_error {
                warn!("[{}] account list failed: {e}", act.cfg.name);
            }
        }));
    }

    /// Find the AirPlay devices that can join the group, if the last scan is too old.
    fn scan_outputs(&mut self, ctx: &mut Context<Self>) {
        let Some(tv) = self.tv.as_ref() else { return };
        if self.timers.scan.is_some() || !poll_due(self.outputs_scanned) {
            return;
        }
        let generation = self.generation;
        if tv.is_open(Protocol::Mrp) {
            let own_uid = self.s.own_output_uid.clone();
            self.timers.scan = Some(
                ctx.spawn(
                    async move { discovery::scan_airplay_outputs(own_uid.as_deref()).await }
                        .into_actor(self)
                        .map(move |result, act, _| {
                            act.timers.scan = None;
                            match result {
                                Ok(devices) if act.generation == generation => {
                                    act.outputs_scanned = Some(Instant::now());
                                    let dirty = act.s.set_airplay_devices(devices);
                                    if !dirty.is_empty() {
                                        act.save_cache();
                                        act.emit(dirty);
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => warn!("[{}] AirPlay scan failed: {e}", act.cfg.name),
                            }
                        }),
                ),
            );
        }
    }

    /// Store the fetched apps. Returns the error, if the fetch failed.
    fn apply_apps(
        &mut self,
        result: Option<Result<Vec<ratv::companion::App>, ratv::Error>>,
    ) -> Option<ratv::Error> {
        match result {
            Some(Ok(apps)) => {
                self.apps_fetched = Some(Instant::now());
                let apps = apps
                    .into_iter()
                    .map(|a| AppInfo {
                        bundle_id: a.bundle_id,
                        name: a.name,
                    })
                    .collect();
                let dirty = self.s.set_apps(apps);
                if !dirty.is_empty() {
                    self.save_cache();
                    self.emit(dirty);
                }
                None
            }
            Some(Err(e)) => Some(e),
            None => None,
        }
    }

    fn save_cache(&self) {
        #[derive(Serialize)]
        struct CacheRef<'a> {
            apps: &'a [AppInfo],
            airplay_devices: &'a [(String, String)],
        }
        let cache = CacheRef {
            apps: &self.s.apps,
            airplay_devices: &self.s.airplay_devices,
        };
        if let Err(e) = self.ctx.store.save_cache(&self.key, &cache) {
            warn!("[{}] cache save failed: {e}", self.cfg.name);
        }
    }

    /// Check the parameters of an entity command. Needs no device state.
    fn parse(msg: &Command) -> Result<remote::RemoteRun, ServiceError> {
        let params = msg.params.as_ref();
        Ok(match msg.kind {
            EntityKind::MediaPlayer => remote::RemoteRun {
                actions: vec![media_player::command(&msg.cmd_id, params)?],
                repeat: 1,
                delay: Duration::ZERO,
            },
            EntityKind::Remote => remote::command(&msg.cmd_id, params)?,
            EntityKind::Select(kind) => remote::RemoteRun {
                actions: vec![media_player::Action::Select(
                    kind,
                    select::command(&msg.cmd_id, params)?,
                )],
                repeat: 1,
                delay: Duration::ZERO,
            },
            EntityKind::Sensor(_) | EntityKind::VoiceAssistant => {
                return Err(ServiceError::NotSupported(msg.cmd_id.clone()));
            }
        })
    }

    /// Turn checked command parameters into calls, with the current device state.
    fn plan(&mut self, run: &remote::RemoteRun) -> Result<Vec<Call>, ServiceError> {
        // Kept only if the plan succeeds.
        let mut seek = self.seek;
        let mut planner = Planner {
            s: &self.s,
            seek: &mut seek,
            power: self.expected_power.unwrap_or(self.s.power),
            muted: self.expected_mute.or(self.s.muted),
            screensaver: self.attention == Some(AttentionState::Screensaver),
        };
        let mut calls = Vec::with_capacity(run.actions.len() * run.repeat as usize);
        for i in 0..run.repeat {
            for (j, action) in run.actions.iter().enumerate() {
                if !run.delay.is_zero() && (i > 0 || j > 0) {
                    calls.push(Call::Sleep(run.delay));
                }
                planner.plan(action, &mut calls)?;
            }
        }
        self.seek = seek;
        Ok(calls)
    }

    /// Assistant event for the end of a Siri session. See [`siri_event`].
    fn siri_end_event(
        &self,
        entity_id: String,
        session_id: u32,
        chunks: Result<Option<u32>, ratv::Error>,
        replaced: bool,
    ) -> AssistantEvent {
        let text = auth_error_text(self.status, &chunks);
        let mut event = siri_event(&self.cfg.name, entity_id, session_id, chunks, replaced);
        if let Some(text) = text
            && let AssistantEvent::Error { data, .. } = &mut event
        {
            data.message = text.into();
        }
        event
    }

    /// Replaces the Siri session. Returns its audio queue and flags.
    fn new_siri_session(&mut self, session_id: u32) -> (mpsc::Receiver<Bytes>, Arc<SiriFlags>) {
        if let Some(old) = self.siri.take() {
            info!(
                "[{}] Siri session {} replaced by {session_id}",
                self.cfg.name, old.session_id
            );
            old.flags.replaced.store(true, Ordering::Relaxed);
        }
        let (audio, rx) = mpsc::channel::<Bytes>(SIRI_QUEUE);
        let flags = Arc::new(SiriFlags::default());
        self.siri = Some(SiriSession {
            session_id,
            audio,
            flags: flags.clone(),
            got_audio: false,
        });
        (rx, flags)
    }

    /// Starts a Siri session.
    fn voice_start(
        &mut self,
        params: Option<&Map<String, Value>>,
        ctx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        let session_id = params
            .and_then(|p| p.get("session_id"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| ServiceError::BadRequest("Missing parameter: session_id".into()))?;
        // The voice button can wake the Remote. Siri then starts when the connect has ended.
        let connecting = self.running_connect();
        if connecting.is_none() {
            let tv = self.tv.as_ref().ok_or_else(|| not_connected(self.status))?;
            if !siri_available(tv, self.ctx.siri_mode()) {
                return Err(ServiceError::ServiceUnavailable(
                    "Siri needs Companion or MRP".into(),
                ));
            }
        }
        let (mut rx, flags) = self.new_siri_session(session_id);

        let mode = self.ctx.siri_mode();
        debug!(
            "[{}] Siri session {session_id}: mode {}",
            self.cfg.name,
            mode.label()
        );
        let entity_id = EntityKind::VoiceAssistant.entity_id(&self.key);
        let controller = self.controller.clone();
        let ready_id = entity_id.clone();
        let lock = self.siri_lock.clone();
        let ended = flags.clone();
        // Returns the number of audio chunks sent to the Apple TV.
        let fut = move |tv: Option<AppleTv>| async move {
            let tv = tv.ok_or(ratv::Error::NotConnected("Apple TV"))?;
            // Ended without audio: do not start Siri. No chunks gives the "no audio" error.
            if flags.aborted.load(Ordering::Relaxed) {
                return Ok(Some(0));
            }
            // ratv allows one session. The session before ends after its audio is sent.
            let _active = tokio::time::timeout(SIRI_WAIT, lock.lock_owned())
                .await
                .map_err(|_| ratv::Error::SiriActive)?;
            // Replaced while it waited: do not start it.
            if flags.replaced.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if flags.aborted.load(Ordering::Relaxed) {
                return Ok(Some(0));
            }
            let mut siri = start_siri(&tv, mode).await?;
            controller.do_send(DeviceEvent::Assistant(AssistantEvent::Ready {
                entity_id: ready_id,
                session_id,
            }));
            let mut pcm = Vec::new();
            let mut chunks = 0_u32;
            loop {
                match tokio::time::timeout(SIRI_AUDIO_TIMEOUT, rx.recv()).await {
                    Ok(Some(chunk)) if !chunk.is_empty() => {
                        pcm.clear();
                        pcm.extend(
                            chunk
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|b| i16::from_le_bytes(*b)),
                        );
                        siri.push_pcm(&pcm).await?;
                        chunks += 1;
                        // The Apple TV ended Siri by itself.
                        if siri.ended() {
                            break;
                        }
                    }
                    // End of audio, session replaced or timeout.
                    _ => break,
                }
            }
            siri.finish().await?;
            Ok(Some(chunks))
        };
        // Siri over MRP needs MRP. Over Companion it starts after the short join wait.
        let join_wait = if mode == SiriMode::Mrp {
            JoinWait::Full
        } else {
            JoinWait::Deadline
        };
        let start = connect_ended(connecting, join_wait)
            .into_actor(self)
            .then(move |(), act, _| fut(act.tv.clone()).into_actor(act));
        ctx.spawn(start.map(move |result, act, _| {
            // Compare the session, not the id: the Remote can use the same id again.
            let current = act.siri.as_ref().map(|s| &s.flags);
            if current.is_some_and(|r| Arc::ptr_eq(r, &ended)) {
                act.siri = None;
            } else if act
                .siri
                .as_ref()
                .is_some_and(|s| s.session_id == session_id)
            {
                // The new session with this id sends the end event.
                return;
            }
            let replaced = ended.replaced.load(Ordering::Relaxed);
            let event = act.siri_end_event(entity_id, session_id, result, replaced);
            act.controller.do_send(DeviceEvent::Assistant(event));
        }));
        Ok(())
    }
}

/// Why a connect failed.
enum ConnectError {
    /// The scan did not find the device. `pairing_failed`: the last endpoints gave a pairing error.
    NotFound {
        pairing_failed: bool,
    },
    Failed(ratv::Error),
}

/// A successful connect.
struct Connected {
    tv: AppleTv,
    ep: Endpoints,
    /// How the address was found, for the log.
    via: &'static str,
    /// Set when the handle has only Companion: MRP is connected next at these endpoints.
    join: Option<Endpoints>,
}

type ConnectResult = Result<Connected, ConnectError>;

/// Connect at the last endpoints, else find the device and connect there. Also returns the
/// configuration if discovery changed it.
async fn connect_device(
    known: Option<Endpoints>,
    mut cfg: DeviceConfig,
    options: ClientOptions,
) -> (Option<DeviceConfig>, ConnectResult) {
    // A pairing error at the last address can come from another device that has this
    // address now. Then the scan below finds the device by its ids.
    let mut pairing_failed_at = None;
    if let Some(ep) = known {
        match connect_endpoints(ep, &cfg.credentials, &options).await {
            Ok((tv, join_mrp)) => {
                let via = "last address";
                let join = join_mrp.then_some(ep);
                return (None, Ok(Connected { tv, ep, via, join }));
            }
            Err(e) if pairing_not_valid(&e) => {
                debug!("[{}] last endpoints failed, scanning: {e}", cfg.name);
                pairing_failed_at = Some((ep, e));
            }
            // A scan fails the same way.
            Err(e) if network_not_ready(&e) => return (None, Err(ConnectError::Failed(e))),
            Err(e) => debug!("[{}] last endpoints failed, scanning: {e}", cfg.name),
        }
    }
    let device = match discovery::find(&cfg).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            let pairing_failed = pairing_failed_at.is_some();
            return (None, Err(ConnectError::NotFound { pairing_failed }));
        }
        Err(e) => return (None, Err(ConnectError::Failed(e))),
    };
    let changed = discovery::update_config(&mut cfg, &device);
    let ep = Endpoints::from(&device);
    if let Some((old, e)) = pairing_failed_at {
        // Same address: the device itself refused the pairing. A second connect fails the same way.
        if ip(ep) == ip(old) {
            return (changed.then_some(cfg), Err(ConnectError::Failed(e)));
        }
        if let Some(old) = ip(old) {
            info!(
                "[{}] another device is at {old}, using the new address",
                cfg.name
            );
        }
    }
    let result = connect_endpoints(ep, &cfg.credentials, &options).await;
    (
        changed.then_some(cfg),
        result
            .map(|(tv, join_mrp)| Connected {
                tv,
                ep,
                via: "scan",
                join: join_mrp.then_some(ep),
            })
            .map_err(ConnectError::Failed),
    )
}

/// Connect Companion first: it is faster and has the power state, so a slow MRP does not hold
/// it back. MRP is then connected with `connect_missing`. Returns the handle and `true` if MRP
/// is still to be connected.
///
/// MRP alone is tried when Companion fails, and also when Companion takes longer than
/// [`COMPANION_HEAD_START`]. So a device with only MRP still connects. A fast Companion error
/// adds the MRP connect time, a slow one at most [`COMPANION_HEAD_START`]. MRP is not tried when
/// the network is not ready: it fails the same way.
async fn connect_endpoints(
    ep: Endpoints,
    credentials: &ratv::Credentials,
    options: &ClientOptions,
) -> Result<(AppleTv, bool), ratv::Error> {
    let (Some(companion), Some(airplay)) = (ep.companion, ep.airplay) else {
        return AppleTv::connect(&ep, credentials, options)
            .await
            .map(|tv| (tv, false));
    };
    let companion_ep = Endpoints {
        companion: Some(companion),
        airplay: None,
    };
    let mrp_ep = Endpoints {
        companion: None,
        airplay: Some(airplay),
    };
    // Boxed: a connect future is large, and two run here.
    let (result, extra) = companion_first(
        Box::pin(AppleTv::connect(&companion_ep, credentials, options)),
        || Box::pin(AppleTv::connect(&mrp_ep, credentials, options)),
        COMPANION_HEAD_START,
        |e| !network_not_ready(e),
    )
    .await;
    // Companion won: MRP is connected again with `connect_missing`, which needs one handle.
    if let Some(tv) = extra {
        actix::spawn(async move {
            let _ = tv.close().await;
        });
    }
    result
}

/// Run `companion`. Start `mrp` when `companion` fails and `try_mrp` allows it, or when
/// `companion` takes longer than `head_start`. A Companion result wins. When both fail, the
/// Companion error is returned, as `AppleTv::connect` does.
///
/// Returns the result with `true` if it came from `companion`, and an MRP result that is not
/// used and must be closed.
async fn companion_first<T, E, C, M>(
    companion: C,
    mrp: impl FnOnce() -> M,
    head_start: Duration,
    try_mrp: impl Fn(&E) -> bool,
) -> (Result<(T, bool), E>, Option<T>)
where
    C: Future<Output = Result<T, E>>,
    M: Future<Output = Result<T, E>>,
{
    let mut companion = std::pin::pin!(companion);
    let (companion, mrp) = match tokio::time::timeout(head_start, &mut companion).await {
        Ok(Ok(tv)) => (Ok(tv), None),
        Ok(Err(e)) if !try_mrp(&e) => (Err(e), None),
        Ok(Err(e)) => (Err(e), Some(mrp().await)),
        Err(_) => {
            let mut mrp = std::pin::pin!(mrp());
            tokio::select! {
                biased;
                result = &mut companion => match result {
                    // Dropping `mrp` stops its connect.
                    Ok(tv) => (Ok(tv), None),
                    Err(e) if !try_mrp(&e) => (Err(e), None),
                    Err(e) => (Err(e), Some(mrp.await)),
                },
                result = &mut mrp => (companion.await, Some(result)),
            }
        }
    };
    match (companion, mrp) {
        (Ok(tv), Some(Ok(extra))) => (Ok((tv, true)), Some(extra)),
        (Ok(tv), _) => (Ok((tv, true)), None),
        (Err(_), Some(Ok(tv))) => (Ok((tv, false)), None),
        (Err(e), _) => (Err(e), None),
    }
}

/// Protocols of `tv` that are not open.
fn missing(tv: &AppleTv) -> impl Iterator<Item = Protocol> + '_ {
    [Protocol::Companion, Protocol::Mrp]
        .into_iter()
        .filter(|&p| !tv.is_open(p))
}

/// IP address of the endpoints.
fn ip(ep: Endpoints) -> Option<IpAddr> {
    ep.companion.or(ep.airplay).map(|a| a.ip())
}

/// `ep` has an endpoint for each protocol that `tv` is missing. `true` when none is missing.
fn has_missing_endpoints(tv: &AppleTv, ep: Endpoints) -> bool {
    missing(tv).all(|p| match p {
        Protocol::Companion => ep.companion.is_some(),
        Protocol::Mrp => ep.airplay.is_some(),
        _ => true,
    })
}

/// Endpoints from a scan. While the device starts, it can list only some services. A service
/// that the scan did not find keeps its old endpoint if the IP address is the same.
fn merge_endpoints(old: Endpoints, found: Endpoints) -> Endpoints {
    if ip(found) != ip(old) {
        return found;
    }
    Endpoints {
        companion: found.companion.or(old.companion),
        airplay: found.airplay.or(old.airplay),
    }
}

/// The protocol that `mode` needs for Siri is connected.
fn siri_available(tv: &AppleTv, mode: SiriMode) -> bool {
    match mode {
        SiriMode::Auto => tv.is_open(Protocol::Companion) || tv.is_open(Protocol::Mrp),
        SiriMode::Mrp => tv.is_open(Protocol::Mrp),
    }
}

/// What is not available without `protocol`.
const fn lost_features(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Companion => {
            "Companion: power state, apps, accounts and swipes are not available"
        }
        Protocol::Mrp => "MRP: volume, outputs, languages, chapters and Up Next are not available",
        _ => "",
    }
}

/// Starts Siri on the path of `mode`.
async fn start_siri(tv: &AppleTv, mode: SiriMode) -> Result<ratv::Siri, ratv::Error> {
    match mode {
        SiriMode::Auto => tv.start_siri().await,
        SiriMode::Mrp => {
            let mrp = tv.mrp().ok_or(ratv::Error::NotConnected("MRP"))?;
            mrp.start_siri().await.map(ratv::Siri::Mrp)
        }
    }
}

/// Assistant event for the end of a Siri session. `chunks` is the number of audio chunks sent to
/// the Apple TV, `None` if Siri was not started.
fn siri_event(
    name: &str,
    entity_id: String,
    session_id: u32,
    chunks: Result<Option<u32>, ratv::Error>,
    replaced: bool,
) -> AssistantEvent {
    // ratv returns `SiriNoAudio` from `finish` when no audio was pushed.
    let no_audio = matches!(chunks, Ok(Some(0)) | Err(ratv::Error::SiriNoAudio));
    let message = match chunks {
        // No audio is an error, unless a newer session replaced this one.
        _ if no_audio && !replaced => {
            warn!("[{name}] Siri session {session_id}: no audio from the Remote");
            "no audio from the Remote".to_string()
        }
        Ok(_) | Err(ratv::Error::SiriNoAudio) => {
            return AssistantEvent::Finished {
                entity_id,
                session_id,
            };
        }
        Err(e @ ratv::Error::SiriDisabled) => {
            warn!("[{name}] Siri failed: {e}");
            return siri_unavailable(
                entity_id,
                session_id,
                "Siri is off on the Apple TV: Settings > General > Siri.",
            );
        }
        // Seen on MRP when Siri is off on the Apple TV.
        Err(e @ ratv::Error::SiriNotRecording) => {
            warn!("[{name}] Siri failed: {e}. Is Siri on in the Apple TV settings?");
            return siri_unavailable(
                entity_id,
                session_id,
                "Apple TV did not start listening. Check that Siri is on: Settings > General > Siri.",
            );
        }
        Err(e) => {
            warn!("[{name}] Siri failed: {e}");
            e.to_string()
        }
    };
    AssistantEvent::Error {
        entity_id,
        session_id,
        data: AssistantError {
            code: AssistantErrorCode::UnexpectedError,
            message,
        },
    }
}

/// Error event for a Siri session that cannot run.
fn siri_unavailable(entity_id: String, session_id: u32, message: &str) -> AssistantEvent {
    AssistantEvent::Error {
        entity_id,
        session_id,
        data: AssistantError {
            code: AssistantErrorCode::ServiceUnavailable,
            message: message.into(),
        },
    }
}

/// How long to wait for the MRP join.
#[derive(Clone, Copy)]
enum JoinWait {
    /// Until the end of the join wait, see [`MRP_JOIN_WAIT`].
    Deadline,
    /// Until the join ends.
    Full,
}

/// Wait until a running connect has ended and then for the MRP join. The whole wait is at most
/// [`CONNECT_WAIT`].
async fn connect_ended(wait: Option<ConnectWait>, join_wait: JoinWait) {
    let Some(ConnectWait {
        mut status,
        mut mrp_join,
    }) = wait
    else {
        return;
    };
    let ended = async {
        let _ = status.wait_for(|s| *s != DeviceStatus::Connecting).await;
        let Some(until) = *mrp_join.borrow_and_update() else {
            return;
        };
        let joined = mrp_join.wait_for(Option::is_none);
        match join_wait {
            JoinWait::Full => {
                let _ = joined.await;
            }
            JoinWait::Deadline => {
                let _ = tokio::time::timeout_at(until, joined).await;
            }
        }
    };
    let _ = tokio::time::timeout(CONNECT_WAIT, ended).await;
}

/// `true` if a list fetched at `last` is old enough to fetch again. `None` means not fetched yet.
fn poll_due(last: Option<Instant>) -> bool {
    last.is_none_or(|t| t.elapsed() + POLL_MARGIN >= POLL_INTERVAL)
}

/// Time until the next chapter, or `None` if it is not a valid duration.
fn chapter_delay(distance: f64, rate: f64) -> Option<Duration> {
    Duration::try_from_secs_f64((distance / rate).max(0.5)).ok()
}

/// Stream that yields `msg` each time the watch value changes.
fn changes<T: Send + Sync + 'static>(
    rx: watch::Receiver<T>,
    msg: WatchChanged,
) -> impl Stream<Item = WatchChanged> + 'static {
    futures::stream::unfold(rx, move |mut rx| async move {
        rx.changed().await.ok()?;
        Some((msg, rx))
    })
}

impl Actor for AppleTvActor {
    type Context = Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        self.emit(Dirty::ALL);
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        // The session future is dropped with the actor, so end the session here.
        if let Some(siri) = self.siri.take() {
            let entity_id = EntityKind::VoiceAssistant.entity_id(&self.key);
            self.controller
                .do_send(DeviceEvent::Assistant(siri_unavailable(
                    entity_id,
                    siri.session_id,
                    "The Apple TV was removed or changed.",
                )));
        }
        self.close(ctx);
        self.ctx.artwork.remove(&self.key);
        // Messages and spawned futures do not run after this, so no save can follow the delete in
        // the write queue.
        if self.remove_cache {
            self.ctx.store.remove_cache(&self.key);
        }
    }
}

impl Handler<WatchChanged> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, msg: WatchChanged, ctx: &mut Self::Context) {
        let dirty = match msg {
            WatchChanged::NowPlaying(g) if g == self.generation => self.update_now_playing(ctx),
            WatchChanged::Volume(g) if g == self.generation => {
                self.expected_mute = None;
                self.update_volume()
            }
            _ => return,
        };
        self.emit(dirty);
    }
}

impl Handler<CompanionEvent> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, msg: CompanionEvent, ctx: &mut Self::Context) {
        // An event can wait in the mailbox while Companion closes. It must not undo the reset in
        // `protocol_lost`.
        if msg.companion_gen != self.companion_gen
            || !self
                .tv
                .as_ref()
                .is_some_and(|tv| tv.is_open(Protocol::Companion))
        {
            return;
        }
        match msg.event {
            Some(Event::SystemStatus(state)) => self.set_attention(state),
            Some(Event::SiriStatus { enabled }) => {
                info!(
                    "[{}] Siri is {}",
                    self.cfg.name,
                    if enabled { "on" } else { "off" }
                );
            }
            Some(Event::ScreenChanged {
                guide_supported, ..
            }) => {
                debug!(
                    "[{}] screen changed, guide supported: {guide_supported}",
                    self.cfg.name
                );
                self.screen_changed(ctx);
            }
            Some(Event::MediaControl(flags)) => {
                self.media_flags = Some(flags.0);
                if flags.0 != 0 {
                    self.leave_home(ctx);
                }
            }
            Some(_) => {}
            // Events were lost: ask again.
            None => self.query_power(ctx, 0, false),
        }
    }
}

impl Handler<Connect> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, _: Connect, ctx: &mut Self::Context) {
        self.start_connect(ctx);
    }
}

impl Handler<Disconnect> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, _: Disconnect, ctx: &mut Self::Context) {
        self.want = false;
        self.cancel_timers(ctx);
        // Asked for, for example at standby: the entities keep their state. Only a lost
        // connection makes them unavailable.
        self.close(ctx);
        // A pairing error stays until the next connect request or a change of pairing, address
        // or device ids.
        if self.status != DeviceStatus::AuthError {
            self.set_status(DeviceStatus::Disconnected);
        }
    }
}

impl Handler<RemoteAddressChanged> for AppleTvActor {
    type Result = ();

    /// Show the shown image again with the new address. Without a URL before, use the image
    /// seen before for the item: its URL could not be built.
    fn handle(&mut self, _: RemoteAddressChanged, _: &mut Self::Context) {
        // With a listen address, the URL does not use the Remote address.
        if self.ctx.http_address.is_some() {
            return;
        }
        let shown = self.s.artwork_url.is_some();
        let (name, item, kept) = match &self.shown_file {
            Some(name) if shown => (name.clone(), self.s.artwork_item, self.s.artwork_kept),
            // Like `update_artwork`, but also without a connection, as in the case above: an
            // image from the cache only for a playing item with images that the driver fetches.
            None if !shown
                && self.s.artwork_fetch
                && self.s.state.is_some()
                && self.s.has_item_key() =>
            {
                let Some(name) = self.cached_art(self.s.item_key()) else {
                    return;
                };
                (name, self.s.item_seq, false)
            }
            _ => return,
        };
        if self.show_file(name, item) {
            self.s.artwork_kept = kept;
        }
    }
}

impl Handler<SiriModeChanged> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, _: SiriModeChanged, _: &mut Self::Context) {
        let Some(tv) = &self.tv else { return };
        // Like at connect: in MRP mode also true while MRP joins.
        let siri = siri_available(tv, self.ctx.siri_mode()) || self.joining();
        if self.s.siri != siri {
            self.s.siri = siri;
            self.emit(Dirty::CONNECTION);
        }
    }
}

impl Handler<Stop> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, msg: Stop, ctx: &mut Self::Context) {
        self.want = false;
        self.remove_cache = msg.remove_cache;
        ctx.stop();
    }
}

#[cfg(test)]
impl Handler<super::SaveCacheNow> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, _: super::SaveCacheNow, _: &mut Self::Context) {
        self.save_cache();
    }
}

impl Handler<UpdateConfig> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, msg: UpdateConfig, ctx: &mut Self::Context) {
        let cfg = msg.0;
        let reconnect = cfg.credentials != self.cfg.credentials
            || cfg.address != self.cfg.address
            || !discovery::same_ids(&cfg.known_ids, &self.cfg.known_ids);
        self.set_config(cfg);
        self.cfg_version += 1;
        if reconnect {
            self.endpoints = None;
        }
        if reconnect && self.want {
            info!("[{}] configuration changed, reconnecting", self.cfg.name);
            self.lost_connection(ctx);
            self.attempts = 0;
            self.connect(ctx);
        } else if reconnect && self.status == DeviceStatus::AuthError {
            // The new configuration may work. The next connect request tries it.
            self.set_status(DeviceStatus::Disconnected);
        }
    }
}

impl Handler<Command> for AppleTvActor {
    // Answered inside `handle`. A future answer can be lost when a `Stop` comes in the same
    // mailbox run.
    type Result = Result<(), ServiceError>;

    fn handle(&mut self, mut msg: Command, ctx: &mut Self::Context) -> Self::Result {
        let reply = msg.reply.take();
        let result = self.command(msg, ctx);
        // The futures that the command spawned run only after `handle` returns, so this comes
        // before any event of the command. The controller handles both in order.
        if result.is_ok()
            && let Some(reply) = reply
        {
            self.controller.do_send(DeviceEvent::Accepted(reply));
        }
        result
    }
}

impl AppleTvActor {
    /// Checks and starts an entity command. Most commands run in the background.
    fn command(&mut self, msg: Command, ctx: &mut Context<Self>) -> Result<(), ServiceError> {
        // `stop_send` and invalid parameters need no connection.
        if msg.kind == EntityKind::Remote && msg.cmd_id == IntgRemoteCommand::StopSend.as_ref() {
            self.stop.send_modify(|n| *n = n.wrapping_add(1));
            debug!("[{}] stop_send: commands stopped", self.cfg.name);
            return Ok(());
        }
        let run = if msg.kind == EntityKind::VoiceAssistant {
            None
        } else {
            Some(Self::parse(&msg)?)
        };
        self.connect_for_command(ctx);
        let Some(run) = run else {
            return match msg.cmd_id.as_str() {
                "voice_start" => self.voice_start(msg.params.as_ref(), ctx),
                _ => Err(ServiceError::NotSupported(msg.cmd_id)),
            };
        };
        // `stop_send` stops only long remote runs, so a single press is never lost.
        let epoch = (msg.kind == EntityKind::Remote && run.is_long()).then(|| *self.stop.borrow());
        let start = Start {
            queued: Instant::now(),
            epoch,
            again: false,
        };
        self.start_run(msg, run, start, ctx)
    }

    /// Start a connect for a command if there is no connection. The core can send a command
    /// before `exit_standby`.
    fn connect_for_command(&mut self, ctx: &mut Context<Self>) {
        if self.tv.is_none() && self.status == DeviceStatus::Disconnected {
            debug!("[{}] command while disconnected, connecting", self.cfg.name);
            self.start_connect(ctx);
        }
    }

    /// Run a checked command in the background, after the connect and the command before.
    fn start_run(
        &self,
        msg: Command,
        run: remote::RemoteRun,
        start: Start,
        ctx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        // A command can come right after the Remote wakes up, while the connect still runs.
        let connecting = self.running_connect();
        if self.tv.is_none() && connecting.is_none() {
            return Err(not_connected(self.status));
        }
        // The Remote core waits at most 10 s for the answer, so the command runs in the
        // background. Later errors are only logged.
        // Plan after the command before has ended, so toggles see its result.
        let lock = self.cmd_lock.clone();
        let fut = async move {
            connect_ended(connecting, JoinWait::Deadline).await;
            lock.lock_owned().await
        }
        .into_actor(self)
        .then(move |guard, act, _| {
            if let Some(reason) = act.skip_reason(start) {
                info!("[{}] {} not sent: {reason}", act.cfg.name, msg.cmd_id);
                return Either::Left(fut::ready(()));
            }
            let generation = act.generation;
            Either::Right(
                act.run_command(&msg, &run, guard, start)
                    .then(move |end, act, _| match end.first_error {
                        None => Either::Left(fut::ready(())),
                        // The lock is kept, so a later command cannot run before this one.
                        Some(e) => {
                            Either::Right(act.send_again(msg, run, start, generation, e, end.guard))
                        }
                    }),
            )
        });
        ctx.spawn(fut);
        Ok(())
    }

    /// Why a command is not sent now, if it is not.
    fn skip_reason(&self, start: Start) -> Option<Skip> {
        if start.epoch.is_some_and(|e| *self.stop.borrow() != e) {
            Some(Skip::Stopped)
        } else if start.queued.elapsed() > COMMAND_MAX_WAIT {
            Some(Skip::TooLate)
        } else {
            None
        }
    }

    /// Send a command once more that was not sent, holding `guard`. It waits
    /// [`SEND_AGAIN_DELAY`], so that the actor has seen the closed connection, and then for the
    /// reconnect. Only a new connection can help: without a connect and with the handle of the
    /// first try (`generation`), it is not sent again. `error` is the error of the first try,
    /// for the log.
    fn send_again(
        &self,
        msg: Command,
        run: remote::RemoteRun,
        start: Start,
        generation: u64,
        error: ServiceError,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> fut::LocalBoxActorFuture<Self, ()> {
        let start = Start {
            again: true,
            ..start
        };
        tokio::time::sleep(SEND_AGAIN_DELAY)
            .into_actor(self)
            .then(move |(), act, ctx| {
                // A `Disconnect` came: the Remote does not want the device now.
                if !act.want {
                    warn!(
                        "[{}] {} failed: {error} (device disconnected)",
                        act.cfg.name, msg.cmd_id
                    );
                    return fut::ready(()).boxed_local();
                }
                act.connect_for_command(ctx);
                let connecting = act.running_connect();
                if connecting.is_none() {
                    // No connect starts after a pairing error.
                    if act.tv.is_none() {
                        let e = not_connected(act.status);
                        warn!("[{}] {} failed: {e}", act.cfg.name, msg.cmd_id);
                        return fut::ready(()).boxed_local();
                    }
                    if act.generation == generation {
                        warn!(
                            "[{}] {} failed: {error} (no new connection)",
                            act.cfg.name, msg.cmd_id
                        );
                        return fut::ready(()).boxed_local();
                    }
                }
                info!("[{}] {} not sent, sending again", act.cfg.name, msg.cmd_id);
                async move { connect_ended(connecting, JoinWait::Deadline).await }
                    .into_actor(act)
                    .then(move |(), act, _| {
                        match act.skip_reason(start) {
                            // The user asked for the stop, so it is no failure.
                            Some(reason @ Skip::Stopped) => {
                                info!("[{}] {} not sent: {reason}", act.cfg.name, msg.cmd_id);
                                return fut::ready(()).boxed_local();
                            }
                            Some(reason @ Skip::TooLate) => {
                                warn!(
                                    "[{}] {} failed: {error} ({reason})",
                                    act.cfg.name, msg.cmd_id
                                );
                                return fut::ready(()).boxed_local();
                            }
                            None => {}
                        }
                        act.run_command(&msg, &run, guard, start)
                            .map(|_, _, _| ())
                            .boxed_local()
                    })
                    .boxed_local()
            })
            .boxed_local()
    }

    /// Log the error of a command. `send_again`: it is sent once more, so it is only info.
    fn log_failure(&self, cmd_id: &str, e: &ServiceError, send_again: bool) {
        match e {
            // The device refused it in its current state, or the item is gone, for example
            // from Up Next. The user cannot fix it on the Remote.
            ServiceError::NotSupported(_)
            | ServiceError::BadRequest(_)
            | ServiceError::NotFound(_) => {
                info!("[{}] {cmd_id} not run: {e}", self.cfg.name);
            }
            _ if send_again => info!("[{}] {cmd_id} failed: {e}", self.cfg.name),
            _ => warn!("[{}] {cmd_id} failed: {e}", self.cfg.name),
        }
    }
}

/// End of a command run.
struct RunEnd {
    /// Error of a first try of which nothing was sent, if there is time left to send it once
    /// more.
    first_error: Option<ServiceError>,
    /// The command lock. Kept while the command is sent again.
    guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Why a command is not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    Stopped,
    TooLate,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped by stop_send",
            Self::TooLate => "waited too long",
        })
    }
}

/// A protocol that the handle has no client for (`missing`) comes back only with a connect.
/// On the handle of the first try (`same_handle`) and without an MRP join, none runs now, so
/// sending again cannot help.
const fn stays_missing(missing: bool, same_handle: bool, joining: bool) -> bool {
    missing && same_handle && !joining
}

/// Send a first try of which nothing was sent (`unsent`) once more, if a new connection can
/// help and `waited` since the key press is less than [`COMMAND_MAX_WAIT`].
fn sends_again(unsent: bool, stays_missing: bool, start: Start, waited: Duration) -> bool {
    unsent && !stays_missing && !start.again && waited < COMMAND_MAX_WAIT
}

/// When a command was queued, its `stop_send` epoch, and whether it is sent again.
#[derive(Clone, Copy)]
struct Start {
    queued: Instant,
    epoch: Option<u64>,
    again: bool,
}

/// Error text for a Siri session whose connect failed on the pairing. `None` keeps the text of
/// [`siri_event`].
fn auth_error_text(
    status: DeviceStatus,
    chunks: &Result<Option<u32>, ratv::Error>,
) -> Option<&'static str> {
    (status == DeviceStatus::AuthError && matches!(chunks, Err(ratv::Error::NotConnected(_))))
        .then_some(PAIRING_INVALID)
}

/// Error for a command without a connection.
fn not_connected(status: DeviceStatus) -> ServiceError {
    if status == DeviceStatus::AuthError {
        ServiceError::ServiceUnavailable(PAIRING_INVALID.into())
    } else {
        ServiceError::NotConnected
    }
}

/// Resolves when a `stop_send` came after `epoch`. Without `epoch` it never resolves.
async fn stopped(mut rx: watch::Receiver<u64>, epoch: Option<u64>) {
    let Some(epoch) = epoch else {
        return std::future::pending().await;
    };
    if rx.wait_for(|n| *n != epoch).await.is_err() {
        // The actor is gone, and its futures with it.
        std::future::pending::<()>().await;
    }
}

/// State that a command changes, to apply when it succeeds.
#[derive(Default)]
struct Expect {
    account: Option<String>,
    launched: Option<String>,
    mute: Option<bool>,
    power: Option<Power>,
    begins_seek: bool,
}

impl Expect {
    fn from_calls(calls: &[Call]) -> Self {
        let mut e = Self::default();
        for call in calls {
            match call {
                // The current account is not reported by the Apple TV, so it is set after a switch.
                Call::SwitchAccount(_, name) => e.account = Some(name.clone()),
                // The app in front is not reported either.
                Call::LaunchApp(id) => e.launched = Some(id.clone()),
                Call::Mute(m) => e.mute = Some(*m),
                Call::TurnOn => e.power = Some(Power::On),
                Call::TurnOff => e.power = Some(Power::Off),
                Call::BeginFastForward | Call::BeginRewind => e.begins_seek = true,
                _ => {}
            }
        }
        e
    }
}

impl AppleTvActor {
    /// Plan and run a command. `guard` is held until the results are applied, and is returned.
    /// A `stop_send` after `start.epoch` stops the run; without it the run cannot be stopped.
    /// Sets [`RunEnd::first_error`] if nothing of a first try was sent (no connection, or see
    /// [`exec_all`]), a new connection can help and there is time left. That error is then
    /// logged at info, not warn.
    fn run_command(
        &mut self,
        msg: &Command,
        run: &remote::RemoteRun,
        guard: tokio::sync::OwnedMutexGuard<()>,
        start: Start,
    ) -> impl ActorFuture<Self, Output = RunEnd> + use<> {
        let epoch = start.epoch;
        let generation = self.generation;
        // The power state comes from Companion, so an MRP join keeps the expected power.
        let companion_gen = self.companion_gen;
        let stop = self.stop.subscribe();
        // Without a connection nothing was sent. A pairing error needs a new pairing.
        let no_tv = self.tv.is_none() && self.status != DeviceStatus::AuthError;
        // Plan only when connected: the plan changes the seek state.
        let prepared = self
            .tv
            .clone()
            .map_or(Err(not_connected(self.status)), |tv| {
                self.plan(run).map(|calls| (tv, calls))
            });
        let Expect {
            account,
            launched,
            mute,
            power,
            begins_seek,
        } = match &prepared {
            Ok((_, calls)) => {
                debug!("[{}] {} -> {calls:?}", self.cfg.name, msg.cmd_id);
                Expect::from_calls(calls)
            }
            Err(_) => Expect::default(),
        };
        let fut = async move {
            // `missing`: the handle has no client for the protocol of the call.
            let (result, unsent, missing) = match prepared {
                // Dropping the run releases held keys.
                Ok((tv, calls)) => tokio::select! {
                    r = exec_all(&tv, calls) => match r {
                        Ok(()) => (Ok(true), false, false),
                        Err(e) => {
                            let missing = matches!(e.error, ratv::Error::NotConnected(_));
                            (Err(ServiceError::from(e.error)), e.unsent, missing)
                        }
                    },
                    () = stopped(stop, epoch) => (Ok(false), false, false),
                },
                Err(e) => (Err(e), no_tv, false),
            };
            (result, unsent, missing, guard)
        };
        let cmd_id = msg.cmd_id.clone();
        fut.into_actor(self)
            .map(move |(result, unsent, missing, guard), act, ctx| {
                let stays_missing = stays_missing(
                    missing,
                    act.generation == generation && act.tv.is_some(),
                    act.joining(),
                );
                let send_again = sends_again(unsent, stays_missing, start, start.queued.elapsed());
                let result = match result {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        info!("[{}] {cmd_id} stopped by stop_send", act.cfg.name);
                        // The device state after a partial run is not known.
                        Err(ServiceError::ServiceUnavailable("stopped".into()))
                    }
                    Err(e) => {
                        act.log_failure(&cmd_id, &e, send_again);
                        Err(e)
                    }
                };
                let ok = result.is_ok();
                if act.generation == generation && matches!(result, Err(ServiceError::Timeout)) {
                    act.command_timed_out(ctx);
                }
                if act.generation == generation {
                    // After an error the device state is not known.
                    if begins_seek && !ok {
                        act.seek = None;
                    }
                    if mute.is_some() {
                        act.expected_mute = mute.filter(|_| ok);
                    }
                }
                if act.companion_gen == companion_gen && power.is_some() {
                    act.expected_power = power.filter(|_| ok);
                }
                if !ok {
                    let first_error = result.err().filter(|_| send_again);
                    return RunEnd { first_error, guard };
                }
                if let Some(name) = account {
                    act.s.current_account = Some(name);
                    act.emit(Dirty::ACCOUNTS);
                }
                if let Some(id) = launched {
                    act.s.launched_app = Some(id);
                    act.launched_at = Some(Instant::now());
                    act.emit(Dirty::APP);
                }
                RunEnd {
                    first_error: None,
                    guard,
                }
            })
    }
}

impl Handler<Browse> for AppleTvActor {
    type Result = Result<uc_api::intg::ws::BrowseMediaResponseMsgData, ServiceError>;

    fn handle(&mut self, msg: Browse, _: &mut Self::Context) -> Self::Result {
        browse::browse(&self.s, msg.media_id.as_deref(), &msg.paging)
    }
}

impl Handler<AudioChunk> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, msg: AudioChunk, _: &mut Self::Context) {
        let Some(siri) = &mut self.siri else {
            debug!(
                "[{}] audio for unknown session {}",
                self.cfg.name, msg.session_id
            );
            return;
        };
        if siri.session_id != msg.session_id {
            debug!(
                "[{}] audio for old session {}",
                self.cfg.name, msg.session_id
            );
            return;
        }
        let end = msg.data.is_empty();
        siri.note_chunk(end);
        if siri.audio.try_send(msg.data).is_err() {
            warn!("[{}] Siri audio queue full, chunk dropped", self.cfg.name);
        }
        if end {
            // Dropping the sender also ends the session if the end marker was dropped.
            self.siri = None;
        }
    }
}

/// Start the retry window at the first call. Returns `false` when the window is over.
fn retry_window_open(end: &mut Option<Instant>, now: Instant) -> bool {
    now < *end.get_or_insert(now + NETWORK_RETRY_TIMEOUT)
}

/// The end of a window that still runs, else `None`.
fn running_window(end: Option<Instant>, now: Instant) -> Option<Instant> {
    end.filter(|end| now < *end)
}

/// Base URL of the HTTP server at `port`. The address is the first of:
/// - `http_address`, the address the server listens on;
/// - `remote`, the local address that the Remote connected to;
/// - the local address on the network of `device`. No packet is sent: connect only selects it.
///
/// A Remote or local address that is unspecified or link-local IPv6 is not used: for link-local,
/// the URL would need the zone of the Remote.
fn http_base(
    http_address: Option<IpAddr>,
    remote: Option<IpAddr>,
    device: Option<IpAddr>,
    ipv6: bool,
    port: u16,
) -> Option<String> {
    let usable = |ip: &IpAddr| {
        !ip.is_unspecified() && !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local())
    };
    let remote = remote.map(|ip| ip.to_canonical()).filter(usable);
    let ip = http_address.or(remote).or_else(|| {
        let device = device?;
        // An IPv6 address only if the server listens on `::`.
        let any = match device {
            IpAddr::V6(_) if ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            _ => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };
        let socket = UdpSocket::bind((any, 0)).ok()?;
        socket.connect(SocketAddr::new(device, 9)).ok()?;
        Some(socket.local_addr().ok()?.ip()).filter(usable)
    })?;
    Some(format!("http://{}", SocketAddr::new(ip, port)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_store::ConfigStore;
    use crate::entities::SensorKind;
    use crate::server::ArtworkStore;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU8;

    #[test]
    fn cache_with_a_missing_list_loads() {
        let cache: DeviceCache =
            serde_json::from_str(r#"{"apps":[{"bundle_id":"com.example","name":"App"}]}"#)
                .expect("cache");
        assert_eq!(1, cache.apps.len());
        assert_eq!(0, cache.airplay_devices.len());
        let empty: DeviceCache = serde_json::from_str("{}").expect("cache");
        assert_eq!(0, empty.apps.len());
    }

    #[test]
    fn retry_window() {
        let t0 = Instant::now();
        let mut end = None;
        assert!(retry_window_open(&mut end, t0));
        let first = t0 + NETWORK_RETRY_TIMEOUT;
        assert_eq!(Some(first), end);
        // A key press keeps a running window.
        let press = t0 + Duration::from_millis(100);
        assert_eq!(Some(first), running_window(end, press));
        assert!(retry_window_open(&mut end, press));
        assert!(!retry_window_open(&mut end, first), "window over");
        assert_eq!(Some(first), end, "the end does not move");
        // A key press after the end starts a new window.
        let later = first + Duration::from_secs(1);
        end = running_window(end, later);
        assert_eq!(None, end);
        assert!(retry_window_open(&mut end, later));
        assert_eq!(Some(later + NETWORK_RETRY_TIMEOUT), end);
    }

    #[test]
    fn http_base_address() {
        let lo = IpAddr::from([127, 0, 0, 1]);
        let fixed = IpAddr::from([192, 0, 2, 9]);
        let base = |a, d, v6| http_base(a, None, d, v6, 8080);
        assert_eq!(
            Some("http://192.0.2.9:8080".into()),
            base(Some(fixed), Some(lo), false)
        );
        assert_eq!(
            Some("http://127.0.0.1:8080".into()),
            base(None, Some(lo), false)
        );
        assert_eq!(None, base(None, None, false));
        let lo6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(None, base(None, Some(lo6), false), "server not on ::");
        // Some test hosts have no IPv6.
        if UdpSocket::bind((lo6, 0)).is_ok() {
            assert_eq!(
                Some("http://[::1]:8080".into()),
                base(None, Some(lo6), true)
            );
        }
    }

    #[test]
    fn http_base_uses_the_remote_address() {
        let fixed = IpAddr::from([192, 0, 2, 9]);
        let remote = IpAddr::from([192, 0, 2, 20]);
        let v6_device = Some(IpAddr::from(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)));
        let base = |a, r| http_base(a, r, v6_device, false, 8080);
        assert_eq!(
            Some("http://192.0.2.9:8080".into()),
            base(Some(fixed), Some(remote)),
            "listen address first"
        );
        assert_eq!(
            Some("http://192.0.2.20:8080".into()),
            base(None, Some(remote))
        );
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 20).to_ipv6_mapped());
        assert_eq!(
            Some("http://192.0.2.20:8080".into()),
            base(None, Some(mapped))
        );
        let link_local = IpAddr::from(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
        // IPv6 device with the server on 0.0.0.0: no local address is found.
        assert_eq!(None, base(None, Some(link_local)), "link-local is skipped");
        assert_eq!(None, base(None, Some(IpAddr::from([0, 0, 0, 0]))));
        let global6 = IpAddr::from(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x20));
        assert_eq!(
            Some("http://[2001:db8::20]:8080".into()),
            base(None, Some(global6))
        );
    }

    #[actix::test]
    async fn new_remote_address_rebuilds_the_artwork_url() {
        let actor = idle_actor("art-remote", None);
        let url = |actor: &Addr<AppleTvActor>| {
            actor.send(Run(|act: &mut AppleTvActor| act.s.artwork_url.clone()))
        };
        actor
            .send(Run(|act: &mut AppleTvActor| {
                act.cfg.address = None;
                act.s.state = Some(PlaybackState::Playing);
                act.s.artwork_fetch = true;
                act.s.title = Some("Movie".into());
                act.s.item_seq = 1;
                let key = act.s.item_key();
                let name = act
                    .ctx
                    .artwork
                    .save(&act.key, "art", Some("image/png"), Bytes::new());
                act.item_art.remember(key, name.clone());
                // No address yet: the image could not be shown.
                assert!(!act.show_file(name, 1));
                assert!(act.ctx.set_remote_ip(IpAddr::from([192, 0, 2, 20])));
            }))
            .await
            .expect("send");
        actor.send(RemoteAddressChanged).await.expect("send");
        let first = url(&actor)
            .await
            .expect("send")
            .expect("cached image shown");
        assert!(first.starts_with("http://192.0.2.20:0/artwork/"), "{first}");

        actor
            .send(Run(|act: &mut AppleTvActor| {
                // The shown image keeps its item and its kept flag.
                act.s.artwork_item = 0;
                act.s.artwork_kept = true;
                assert!(act.ctx.set_remote_ip(IpAddr::from([192, 0, 2, 21])));
            }))
            .await
            .expect("send");
        actor.send(RemoteAddressChanged).await.expect("send");
        let (second, item, kept) = actor
            .send(Run(|act: &mut AppleTvActor| {
                let s = &act.s;
                (s.artwork_url.clone(), s.artwork_item, s.artwork_kept)
            }))
            .await
            .expect("send");
        let second = second.expect("image");
        assert!(
            second.starts_with("http://192.0.2.21:0/artwork/"),
            "{second}"
        );
        assert_eq!((0, true), (item, kept));

        // A device URL is not a file of the driver: it stays.
        actor
            .send(Run(|act: &mut AppleTvActor| {
                act.shown_file = None;
                act.s.artwork_url = Some("https://cdn.example/a.jpg".into());
                assert!(act.ctx.set_remote_ip(IpAddr::from([192, 0, 2, 22])));
            }))
            .await
            .expect("send");
        actor.send(RemoteAddressChanged).await.expect("send");
        let cdn = url(&actor).await.expect("send");
        assert_eq!(Some("https://cdn.example/a.jpg".into()), cdn);

        // An IPv4 address that comes as IPv6 is stored as IPv4.
        actor
            .send(Run(|act: &mut AppleTvActor| {
                let mapped = |ip: Ipv4Addr| IpAddr::V6(ip.to_ipv6_mapped());
                let lan = Ipv4Addr::new(192, 0, 2, 23);
                assert!(act.ctx.set_remote_ip(mapped(lan)));
                assert_eq!(Some(IpAddr::V4(lan)), act.ctx.remote_ip());
                assert!(!act.ctx.set_remote_ip(IpAddr::V4(lan)), "same address");
                assert!(
                    !act.ctx.set_remote_ip(mapped(Ipv4Addr::LOCALHOST)),
                    "loopback"
                );
            }))
            .await
            .expect("send");
    }

    #[actix::test]
    async fn listen_address_ignores_the_remote_address() {
        let actor = idle_actor("art-listen", Some(IpAddr::from([192, 0, 2, 9])));
        let url = actor
            .send(Run(|act: &mut AppleTvActor| {
                let name = act
                    .ctx
                    .artwork
                    .save(&act.key, "art", Some("image/png"), Bytes::new());
                assert!(act.show_file(name, 1));
                assert!(act.ctx.set_remote_ip(IpAddr::from([192, 0, 2, 20])));
                act.s.artwork_url.clone()
            }))
            .await
            .expect("send");
        actor.send(RemoteAddressChanged).await.expect("send");
        let after = actor
            .send(Run(|act: &mut AppleTvActor| act.s.artwork_url.clone()))
            .await
            .expect("send");
        assert_eq!(url, after);
        let url = url.expect("image");
        assert!(url.starts_with("http://192.0.2.9:0/artwork/"), "{url}");
    }

    /// Drops all device events.
    struct NoController;

    impl Actor for NoController {
        type Context = Context<Self>;
    }

    impl Handler<DeviceEvent> for NoController {
        type Result = ();
        fn handle(&mut self, _: DeviceEvent, _: &mut Context<Self>) {}
    }

    /// An actor that is not connected. It does not connect until it gets `Connect`.
    fn idle_actor(name: &str, http_address: Option<IpAddr>) -> Addr<AppleTvActor> {
        let dir = std::env::temp_dir().join(format!(
            "uc-intg-appletv-actor-{name}-{}",
            std::process::id()
        ));
        let ctx = Arc::new(DeviceContext {
            client_name: "test".into(),
            http_address,
            http_ipv6: false,
            http_port: 0,
            store: ConfigStore::new(&dir),
            artwork: ArtworkStore::new(dir.join("artwork")),
            siri_mode: AtomicU8::new(0),
            remote_ip: Mutex::default(),
        });
        let cfg = crate::config_store::tests::device();
        AppleTvActor::new(
            cfg.key.as_str().into(),
            cfg,
            DeviceCache::default(),
            ctx,
            NoController.start().recipient(),
        )
        .start()
    }

    /// Sets the flag when it is dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Where a test task is kept.
    #[derive(Clone, Copy)]
    enum Slot {
        NowPlaying,
        Missing,
    }

    /// Keeps a task that does not end in a slot. The flag is set when the task is stopped.
    #[derive(Message)]
    #[rtype(result = "()")]
    struct Task(Slot, Arc<AtomicBool>);

    impl Handler<Task> for AppleTvActor {
        type Result = ();
        fn handle(&mut self, msg: Task, ctx: &mut Context<Self>) {
            let flag = DropFlag(msg.1);
            match msg.0 {
                Slot::NowPlaying => {
                    let stream = futures::stream::poll_fn(move |_| {
                        let _ = &flag;
                        std::task::Poll::<Option<Connect>>::Pending
                    });
                    let h = spawn_stream(ctx, stream);
                    replace_handle(ctx, &mut self.timers.now_playing, h);
                }
                Slot::Missing => {
                    let task = async move {
                        let _flag = flag;
                        std::future::pending::<()>().await;
                    };
                    self.timers.missing = Some(ctx.spawn(task.into_actor(self)));
                    self.missing_network_end = Some(Instant::now() + LONG);
                }
            }
        }
    }

    /// Runs a step of the actor and returns `missing_network_end.is_some()`.
    #[derive(Message)]
    #[rtype(result = "bool")]
    enum Step {
        Close,
        CancelTimers,
        StopMissing,
        Nothing,
    }

    impl Handler<Step> for AppleTvActor {
        type Result = bool;
        fn handle(&mut self, msg: Step, ctx: &mut Context<Self>) -> bool {
            match msg {
                Step::Close => self.close(ctx),
                Step::CancelTimers => self.cancel_timers(ctx),
                Step::StopMissing => self.stop_missing(&ratv::Error::NoEndpoints, false, ctx),
                Step::Nothing => {}
            }
            self.missing_network_end.is_some()
        }
    }

    /// Start a task in `slot`. Returns its flag.
    async fn task(actor: &Addr<AppleTvActor>, slot: Slot) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        actor
            .send(Task(slot, Arc::clone(&flag)))
            .await
            .expect("send");
        flag
    }

    /// Run `step`, then let the actor drop the stopped tasks.
    async fn step(actor: &Addr<AppleTvActor>, step: Step) -> bool {
        let r = actor.send(step).await.expect("send");
        actor.send(Step::Nothing).await.expect("send");
        r
    }

    #[actix::test]
    async fn streams_stop_on_replace_close_and_cancel() {
        let actor = idle_actor("streams", None);
        let old = task(&actor, Slot::NowPlaying).await;
        let new = task(&actor, Slot::NowPlaying).await;
        let _ = step(&actor, Step::Nothing).await;
        assert!(old.load(Ordering::Relaxed), "replaced stream stopped");
        assert!(!new.load(Ordering::Relaxed));
        let _ = step(&actor, Step::Close).await;
        assert!(new.load(Ordering::Relaxed), "close stops the stream");
        let next = task(&actor, Slot::NowPlaying).await;
        let _ = step(&actor, Step::CancelTimers).await;
        assert!(
            next.load(Ordering::Relaxed),
            "cancel_timers stops the stream"
        );
    }

    /// Runs the function on the actor and returns its result.
    struct Run<F>(F);

    impl<F, R> Message for Run<F>
    where
        F: FnOnce(&mut AppleTvActor) -> R,
        R: 'static,
    {
        type Result = R;
    }

    impl<F, R> Handler<Run<F>> for AppleTvActor
    where
        F: FnOnce(&mut Self) -> R,
        R: 'static,
    {
        type Result = MessageResult<Run<F>>;
        fn handle(&mut self, msg: Run<F>, _: &mut Context<Self>) -> Self::Result {
            MessageResult(msg.0(self))
        }
    }

    #[actix::test]
    async fn cached_image_is_shown_until_mrp_connects() {
        let actor = idle_actor("art-wait", Some(IpAddr::from([127, 0, 0, 1])));
        let url = actor
            .send(Run(|act: &mut AppleTvActor| {
                act.s.title = Some("Movie".into());
                act.s.item_seq = 2;
                let key = act.s.item_key();
                let name = act
                    .ctx
                    .artwork
                    .save(&act.key, "art", Some("image/png"), Bytes::new());
                act.item_art.remember(key, name);
                // The image of a short item before is shown. The image of this item comes first.
                act.s.artwork_url = Some("http://old".into());
                act.s.artwork_item = 1;
                act.s.prev_item_lasted = Some(Duration::from_millis(500));
                assert!(act.s.keep_artwork(2));
                assert_eq!(Dirty::NONE, act.wait_for_mrp_artwork(true, 2, key));
                assert!(act.artwork_after_mrp);
                assert_eq!(2, act.s.artwork_item);
                assert!(!act.s.artwork_kept);
                assert!(act.shown_file.is_some());
                act.s.artwork_url.clone()
            }))
            .await
            .expect("send");
        let url = url.expect("cached image");
        assert!(url.starts_with("http://127.0.0.1:0/artwork/"), "{url}");

        let (no_key, not_kept, shown) = actor
            .send(Run(|act: &mut AppleTvActor| {
                let key = act.s.item_key();
                let mut run = |keyed, item_key, url: &str, artwork_item| {
                    act.s.artwork_url = Some(url.into());
                    act.s.artwork_item = artwork_item;
                    act.s.item_seq = 3;
                    let dirty = act.wait_for_mrp_artwork(keyed, 3, item_key);
                    assert!(act.artwork_after_mrp);
                    (dirty, act.s.artwork_url.clone())
                };
                (
                    // An item without key gets no image from the cache.
                    run(false, key, "http://old", 1),
                    // No image is kept for the item.
                    run(true, key ^ 1, "http://old", 1),
                    // The item shows an image already.
                    run(true, key, "http://this", 3),
                )
            }))
            .await
            .expect("send");
        assert_eq!((Dirty::ARTWORK, None), no_key);
        assert_eq!((Dirty::ARTWORK, None), not_kept);
        assert_eq!((Dirty::NONE, Some("http://this".into())), shown);
    }

    #[actix::test]
    async fn cached_image_without_url_clears_the_older_image() {
        let actor = idle_actor("art-no-url", None);
        let (dirty, url) = actor
            .send(Run(|act: &mut AppleTvActor| {
                // No local address for the artwork URL.
                act.cfg.address = None;
                act.s.title = Some("Movie".into());
                act.s.item_seq = 2;
                let key = act.s.item_key();
                let name = act
                    .ctx
                    .artwork
                    .save(&act.key, "art", Some("image/png"), Bytes::new());
                act.item_art.remember(key, name);
                act.s.artwork_url = Some("http://old".into());
                act.s.artwork_item = 1;
                let dirty = act.wait_for_mrp_artwork(true, 2, key);
                assert!(act.artwork_after_mrp);
                (dirty, act.s.artwork_url.clone())
            }))
            .await
            .expect("send");
        assert_eq!((Dirty::ARTWORK, None), (dirty, url));

        let (older, current) = actor
            .send(Run(|act: &mut AppleTvActor| {
                let mut run = |url: &str, artwork_item| {
                    act.s.artwork_url = Some(url.into());
                    act.s.artwork_item = artwork_item;
                    let dirty = act.save_artwork("art", None, Bytes::new(), (2, 7));
                    (dirty, act.s.artwork_url.clone())
                };
                (
                    // An image of an older item is cleared.
                    run("http://old", 1),
                    // A CDN URL of this item stays.
                    run("https://cdn", 2),
                )
            }))
            .await
            .expect("send");
        assert_eq!((Dirty::ARTWORK, None), older);
        assert_eq!((Dirty::NONE, Some("https://cdn".into())), current);
    }

    #[actix::test]
    async fn siri_mode_change_without_connection_changes_nothing() {
        let actor = idle_actor("siri-mode", None);
        // Without a connection, a new computation would give false.
        let set = actor.send(Run(|act: &mut AppleTvActor| act.s.siri = true));
        set.await.expect("send");
        actor.send(SiriModeChanged).await.expect("send");
        let siri = actor.send(Run(|act: &mut AppleTvActor| act.s.siri));
        assert!(siri.await.expect("send"), "not changed");
    }

    #[actix::test]
    async fn stop_missing_ends_the_retries() {
        let actor = idle_actor("stop-missing", None);
        let retry = task(&actor, Slot::Missing).await;
        assert!(!retry.load(Ordering::Relaxed));
        assert!(
            !step(&actor, Step::StopMissing).await,
            "missing network end cleared"
        );
        assert!(retry.load(Ordering::Relaxed), "retry timer stopped");
    }

    fn command(kind: EntityKind, cmd_id: &str, params: Value) -> Command {
        Command {
            kind,
            cmd_id: cmd_id.into(),
            params: params.as_object().cloned(),
            reply: None,
        }
    }

    #[test]
    fn siri_text_after_auth_error() {
        let not_connected = || Err(ratv::Error::NotConnected("Apple TV"));
        assert_eq!(
            Some(PAIRING_INVALID),
            auth_error_text(DeviceStatus::AuthError, &not_connected())
        );
        assert_eq!(
            None,
            auth_error_text(DeviceStatus::Disconnected, &not_connected())
        );
        assert_eq!(None, auth_error_text(DeviceStatus::AuthError, &Ok(Some(0))));
        assert_eq!(
            None,
            auth_error_text(DeviceStatus::AuthError, &Err(ratv::Error::Timeout))
        );
    }

    #[test]
    fn end_before_audio_aborts_siri() {
        let session = |got_audio| SiriSession {
            session_id: 1,
            audio: mpsc::channel(1).0,
            flags: Arc::default(),
            got_audio,
        };
        let mut s = session(false);
        s.note_chunk(true);
        assert!(s.flags.aborted.load(Ordering::Relaxed));

        let mut s = session(false);
        s.note_chunk(false);
        s.note_chunk(true);
        assert!(!s.flags.aborted.load(Ordering::Relaxed));
    }

    #[test]
    fn not_connected_tells_to_pair_after_auth_error() {
        assert_eq!(
            ServiceError::ServiceUnavailable(
                "Pairing is not valid, pair the Apple TV again".into()
            ),
            not_connected(DeviceStatus::AuthError)
        );
        assert_eq!(
            ServiceError::NotConnected,
            not_connected(DeviceStatus::Disconnected)
        );
    }

    #[test]
    fn retry_decision() {
        let start = Start {
            queued: Instant::now(),
            epoch: None,
            again: false,
        };
        let again = Start {
            again: true,
            ..start
        };
        let soon = Duration::from_secs(1);
        assert!(sends_again(true, false, start, soon));
        assert!(
            !sends_again(false, false, start, soon),
            "something was sent"
        );
        assert!(
            !sends_again(true, true, start, soon),
            "protocol stays missing"
        );
        assert!(!sends_again(true, false, again, soon), "only once");
        assert!(
            !sends_again(true, false, start, COMMAND_MAX_WAIT),
            "too late"
        );

        assert!(stays_missing(true, true, false));
        assert!(!stays_missing(false, true, false), "not a missing protocol");
        assert!(!stays_missing(true, false, false), "new handle");
        assert!(!stays_missing(true, true, true), "MRP join runs");
    }

    /// A command without a connection waits to be sent again and holds the command lock.
    /// `Disconnect` ends the wait and frees the lock.
    #[actix::test]
    async fn unsent_command_keeps_lock_until_disconnect_ends_it() {
        let actor = idle_actor("send-again", None);
        let free = || {
            actor.send(Run(|act: &mut AppleTvActor| {
                act.cmd_lock.clone().try_lock_owned().is_ok()
            }))
        };
        actor
            .send(Run(|act: &mut AppleTvActor| {
                act.want = true;
                act.set_status(DeviceStatus::Connecting);
            }))
            .await
            .unwrap();
        let cmd = command(
            EntityKind::Remote,
            "send_cmd",
            serde_json::json!({ "command": "CURSOR_UP" }),
        );
        actor.send(cmd).await.unwrap().expect("accepted");
        actor.send(Disconnect).await.unwrap();
        tokio::time::sleep(SEND_AGAIN_DELAY / 2).await;
        assert!(
            !free().await.unwrap(),
            "lock held while waiting to send again"
        );
        tokio::time::sleep(SEND_AGAIN_DELAY * 2).await;
        assert!(free().await.unwrap(), "lock released after Disconnect");
    }

    #[test]
    fn parse_checks_parameters_without_state() {
        let run = AppleTvActor::parse(&command(
            EntityKind::Remote,
            "send_cmd",
            serde_json::json!({ "command": "CURSOR_UP", "repeat": 3 }),
        ))
        .expect("valid");
        assert_eq!(3, run.repeat);
        assert!(matches!(
            AppleTvActor::parse(&command(
                EntityKind::Remote,
                "send_cmd",
                serde_json::json!({ "command": "NOT_A_COMMAND" }),
            )),
            Err(ServiceError::BadRequest(_))
        ));
        assert!(matches!(
            AppleTvActor::parse(&command(
                EntityKind::Sensor(SensorKind::App),
                "x",
                Value::Null
            )),
            Err(ServiceError::NotSupported(_))
        ));
    }

    #[tokio::test]
    async fn stopped_waits_for_a_new_stop() {
        let tx = watch::Sender::new(0u64);
        let wait =
            |epoch| tokio::time::timeout(Duration::from_millis(50), stopped(tx.subscribe(), epoch));
        assert!(wait(Some(0)).await.is_err(), "no stop yet");
        tx.send_modify(|n| *n += 1);
        assert!(wait(Some(0)).await.is_ok(), "stop after the epoch");
        assert!(wait(None).await.is_err(), "a short run cannot be stopped");
    }

    type ConnectOutcome = Result<&'static str, &'static str>;
    const LONG: Duration = Duration::from_secs(3600);

    /// MRP connect that notes its start and returns `result`.
    fn mrp(
        started: &std::cell::Cell<bool>,
        result: ConnectOutcome,
    ) -> impl FnOnce() -> std::future::Ready<ConnectOutcome> + '_ {
        move || {
            started.set(true);
            std::future::ready(result)
        }
    }

    #[tokio::test]
    async fn companion_first_without_mrp() {
        let started = std::cell::Cell::new(false);
        let r = companion_first(async { Ok("c") }, mrp(&started, Ok("m")), LONG, |_| true).await;
        assert_eq!(r, (Ok(("c", true)), None));
        assert!(!started.get(), "Companion connected: MRP is joined later");

        let r = companion_first(async { Err("net") }, mrp(&started, Ok("m")), LONG, |_| {
            false
        })
        .await;
        assert_eq!(r, (Err("net"), None));
        assert!(!started.get(), "MRP fails the same way");
    }

    #[tokio::test]
    async fn companion_error_tries_mrp() {
        let started = std::cell::Cell::new(false);
        let r = companion_first(async { Err("c") }, mrp(&started, Ok("m")), LONG, |_| true).await;
        assert_eq!(r, (Ok(("m", false)), None));
        assert!(started.get());
        started.set(false);
        let r = companion_first(async { Err("c") }, mrp(&started, Err("m")), LONG, |_| true).await;
        assert_eq!(r, (Err("c"), None), "the Companion error comes first");
        assert!(started.get());
    }

    #[tokio::test(start_paused = true)]
    async fn slow_companion_runs_with_mrp() {
        use tokio::sync::oneshot;
        let head = Duration::from_millis(1);
        // Companion ends only after MRP started.
        let (tx, rx) = oneshot::channel::<ConnectOutcome>();
        let mrp = move || {
            let _ = tx.send(Ok("c"));
            std::future::pending::<ConnectOutcome>()
        };
        let r = companion_first(async { rx.await.unwrap() }, mrp, head, |_| true).await;
        assert_eq!(r, (Ok(("c", true)), None), "Companion wins, MRP is stopped");

        // MRP ends first: Companion still wins, the MRP handle is returned to close.
        let (tx, rx) = oneshot::channel::<ConnectOutcome>();
        let mrp = move || async move {
            let _ = tx.send(Ok("c"));
            Ok("m")
        };
        let r = companion_first(async { rx.await.unwrap() }, mrp, head, |_| true).await;
        assert_eq!(r, (Ok(("c", true)), Some("m")));

        // Companion fails after MRP connected.
        let (tx, rx) = oneshot::channel::<ConnectOutcome>();
        let mrp = move || async move {
            let _ = tx.send(Err("c"));
            Ok("m")
        };
        let r = companion_first(async { rx.await.unwrap() }, mrp, head, |_| true).await;
        assert_eq!(r, (Ok(("m", false)), None));

        // MRP connected first: it is used also after a Companion error that MRP would have too.
        let (tx, rx) = oneshot::channel::<ConnectOutcome>();
        let mrp = move || async move {
            let _ = tx.send(Err("net"));
            Ok("m")
        };
        let r = companion_first(async { rx.await.unwrap() }, mrp, head, |_| false).await;
        assert_eq!(r, (Ok(("m", false)), None));

        // Companion fails first: MRP is waited for.
        let (tx, rx) = oneshot::channel::<ConnectOutcome>();
        let (mtx, mrx) = oneshot::channel::<ConnectOutcome>();
        let mrp = move || {
            let _ = tx.send(Err("c"));
            async move {
                tokio::task::yield_now().await;
                let _ = mtx.send(Ok("m"));
                Ok("m")
            }
        };
        let r = companion_first(async { rx.await.unwrap() }, mrp, head, |_| true).await;
        assert_eq!(r, (Ok(("m", false)), None));
        assert_eq!(mrx.await, Ok(Ok("m")));

        // Companion fails with an error that MRP has too: MRP is stopped.
        let (tx, rx) = oneshot::channel::<ConnectOutcome>();
        let mrp = move || {
            let _ = tx.send(Err("net"));
            std::future::pending::<ConnectOutcome>()
        };
        let r = companion_first(async { rx.await.unwrap() }, mrp, head, |_| false).await;
        assert_eq!(r, (Err("net"), None));
    }

    #[tokio::test(start_paused = true)]
    async fn command_waits_for_mrp_join() {
        use tokio::time::Instant as TokioInstant;
        let status = watch::Sender::new(DeviceStatus::Connecting);
        let join = watch::Sender::new(None);
        let wait = || {
            Some(ConnectWait {
                status: status.subscribe(),
                mrp_join: join.subscribe(),
            })
        };
        let spawn = |join_wait| tokio::spawn(connect_ended(wait(), join_wait));
        // Lets the spawned waiter run without moving the paused time.
        let settle = || async {
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        };

        let task = spawn(JoinWait::Deadline);
        settle().await;
        assert!(!task.is_finished(), "connect runs");
        status.send_replace(DeviceStatus::Connected);
        settle().await;
        assert!(task.is_finished(), "connected with both protocols");

        join.send_replace(Some(TokioInstant::now() + LONG));
        let task = spawn(JoinWait::Deadline);
        settle().await;
        assert!(!task.is_finished(), "MRP join runs");
        join.send_replace(None);
        settle().await;
        assert!(task.is_finished(), "MRP joined");

        // A stalled join holds commands until one end time, not a full wait per command.
        let until = TokioInstant::now() + MRP_JOIN_WAIT;
        join.send_replace(Some(until));
        let first = spawn(JoinWait::Deadline);
        tokio::time::sleep(MRP_JOIN_WAIT / 2).await;
        let second = spawn(JoinWait::Deadline);
        settle().await;
        assert!(!first.is_finished() && !second.is_finished());
        tokio::time::sleep_until(until).await;
        settle().await;
        assert!(first.is_finished() && second.is_finished(), "same end time");

        // Siri over MRP waits for the end of the join, past the end time.
        let full = spawn(JoinWait::Full);
        tokio::time::sleep(MRP_JOIN_WAIT).await;
        settle().await;
        assert!(!full.is_finished(), "Siri over MRP waits for the join");
        join.send_replace(None);
        settle().await;
        assert!(full.is_finished(), "join ended");

        // A join that does not end holds Siri over MRP at most `CONNECT_WAIT`.
        join.send_replace(Some(TokioInstant::now()));
        let full = spawn(JoinWait::Full);
        tokio::time::sleep(CONNECT_WAIT.saturating_sub(Duration::from_millis(1))).await;
        settle().await;
        assert!(!full.is_finished());
        tokio::time::sleep(Duration::from_millis(1)).await;
        settle().await;
        assert!(full.is_finished(), "capped by CONNECT_WAIT");
        join.send_replace(None);

        let none = tokio::spawn(connect_ended(None, JoinWait::Full));
        settle().await;
        assert!(none.is_finished(), "nothing to wait for");
    }

    #[test]
    fn ip_prefers_companion() {
        let addr = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        let ep = Endpoints {
            companion: addr("192.0.2.2:49153"),
            airplay: addr("192.0.2.3:7000"),
        };
        assert_eq!(ip(ep), Some("192.0.2.2".parse().unwrap()));
        let ep = Endpoints {
            companion: None,
            ..ep
        };
        assert_eq!(ip(ep), Some("192.0.2.3".parse().unwrap()));
        assert_eq!(ip(Endpoints::default()), None);
    }

    #[test]
    fn merge_endpoints_keeps_missing_service() {
        let addr = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        let old = Endpoints {
            companion: addr("192.0.2.2:49153"),
            airplay: addr("192.0.2.2:7000"),
        };
        // Companion moved to a new port.
        let found = Endpoints {
            companion: addr("192.0.2.2:49160"),
            airplay: addr("192.0.2.2:7000"),
        };
        assert_eq!(merge_endpoints(old, found), found);
        // Companion is not listed yet: keep the old port.
        let found = Endpoints {
            companion: None,
            airplay: addr("192.0.2.2:7000"),
        };
        assert_eq!(merge_endpoints(old, found), old);
        // New IP address: old ports are not valid there.
        let found = Endpoints {
            companion: None,
            airplay: addr("192.0.2.3:7000"),
        };
        assert_eq!(merge_endpoints(old, found), found);
        // Nothing found before.
        assert_eq!(merge_endpoints(Endpoints::default(), found), found);
    }

    #[test]
    fn siri_without_audio_is_an_error() {
        let event = |chunks, replaced| match siri_event("tv", "voice".into(), 1, chunks, replaced) {
            AssistantEvent::Finished { .. } => None,
            AssistantEvent::Error { data, .. } => Some(data.message),
            _ => Some(String::new()),
        };
        assert_eq!(
            Some("no audio from the Remote".into()),
            event(Ok(Some(0)), false)
        );
        assert_eq!(None, event(Ok(Some(0)), true));
        assert_eq!(
            Some("no audio from the Remote".into()),
            event(Err(ratv::Error::SiriNoAudio), false)
        );
        assert_eq!(None, event(Err(ratv::Error::SiriNoAudio), true));
        assert_eq!(None, event(Ok(Some(3)), false));
        assert_eq!(None, event(Ok(None), false));
        assert!(event(Err(ratv::Error::Timeout), false).is_some());
        assert!(
            event(Err(ratv::Error::SiriNotRecording), false)
                .is_some_and(|m| m.contains("Siri is on"))
        );
    }

    #[test]
    fn chapter_delay_is_valid() {
        assert_eq!(Some(Duration::from_secs(10)), chapter_delay(20.0, 2.0));
        assert_eq!(Some(Duration::from_millis(500)), chapter_delay(0.1, 1.0));
        assert_eq!(None, chapter_delay(f64::INFINITY, 1.0));
        assert_eq!(None, chapter_delay(1e300, 1e-10));
    }
}
