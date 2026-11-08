// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Actor for one Apple TV: connection, state updates and commands.

use crate::config_store::{DeviceConfig, SiriMode, fnv1a};
use crate::device::commands::{Call, Planner, Seek, exec_all};
use crate::device::state::{
    ARTWORK_SIZE, AccountInfo, AppInfo, Dirty, ItemArtwork, Power, Snapshot,
};
use crate::device::{
    AudioChunk, Browse, Command, Connect, DeviceContext, DeviceEvent, DeviceStatus, Disconnect,
    Stop, UpdateConfig, browse, discovery, now,
};
use crate::entities::{self, EntityKind, media_player, remote, select};
use crate::errors::ServiceError;
use actix::prelude::*;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::{Level, debug, info, log_enabled, warn};
use ratv::companion::{AttentionState, Event};
use ratv::mrp::{NowPlaying, PlaybackState, Volume};
use ratv::{AppleTv, ClientOptions, Endpoints};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::BroadcastStream;
use uc_api::intg::{AssistantError, AssistantErrorCode, AssistantEvent};

/// Interval for the app, account and AirPlay polls.
const POLL_INTERVAL: Duration = Duration::from_secs(300);
/// The app list time is taken at the reply, so the next poll comes a bit early.
const APPS_MARGIN: Duration = Duration::from_secs(30);
/// Interval for a full reconnect while a protocol is missing.
const FULL_RECONNECT_INTERVAL: Duration = Duration::from_secs(600);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest wait for a running connect before a command runs. The core waits 10 s for the answer.
const CONNECT_WAIT: Duration = Duration::from_secs(6);
/// A connect step without answer in this time is started again once.
///
/// After wake-up, the Wi-Fi link of the Remote can go down for a short time during the connect.
/// Lost packets then wait for the TCP resend (1 s or more). A connect step takes much less.
const STALL_TIMEOUT: Duration = Duration::from_millis(500);
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
/// Wait after the last audio before Siri is released, with [`SiriMode::MrpPacedDelay`].
const SIRI_RELEASE_DELAY: Duration = Duration::from_millis(250);
/// Screen events come in bursts. Act after the last one.
const SCREEN_SETTLE: Duration = Duration::from_millis(1500);
/// Now playing changes right after the Home Screen shows belong to the switch.
const HOME_GRACE: Duration = Duration::from_secs(1);
/// Screen changes in this time after an app launch come from the launch.
const LAUNCH_GRACE: Duration = Duration::from_secs(5);

/// Lists that are kept between restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceCache {
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
    generation: u64,
    event: Option<Event>,
}

struct SiriSession {
    session_id: u32,
    audio: mpsc::Sender<Bytes>,
    /// Set when a newer session replaces this one.
    replaced: Arc<AtomicBool>,
}

#[derive(Default)]
struct Timers {
    reconnect: Option<SpawnHandle>,
    poll: Option<SpawnHandle>,
    chapter: Option<SpawnHandle>,
    full_reconnect: Option<SpawnHandle>,
    screen: Option<SpawnHandle>,
}

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
    attempts: u32,
    /// Incremented on each connect attempt and on close. Results of older attempts are ignored.
    attempt: u64,
    /// Time of the last successful connect.
    connected_at: Option<Instant>,
    /// Incremented on each connect and disconnect. Results of older connections are ignored.
    generation: u64,
    np: Option<watch::Receiver<NowPlaying>>,
    volume: Option<watch::Receiver<Volume>>,
    attention: Option<AttentionState>,
    seek: Option<Seek>,
    siri: Option<SiriSession>,
    /// Held while a Siri session exists on the device.
    siri_lock: Arc<tokio::sync::Mutex<()>>,
    /// Commands run one after the other.
    cmd_lock: Arc<tokio::sync::Mutex<()>>,
    timers: Timers,
    apps_fetched: Option<Instant>,
    /// Last sent current chapter.
    chapter: Option<usize>,
    /// Incremented on each artwork update. Results of older fetches are ignored.
    artwork_seq: u64,
    /// The Home Screen shows: now playing is cleared since then.
    home_since: Option<Instant>,
    /// Time of the last app launch through the driver.
    launched_at: Option<Instant>,
    /// Last media control flags from Companion. 0 when no app has media controls.
    media_flags: Option<u32>,
    /// Name of the shown artwork file.
    shown_file: Option<Box<str>>,
    item_art: ItemArtwork,
    /// Cached result of [`Self::find_http_base`]. Reset on close.
    http_base: Option<String>,
    /// Endpoints of the last successful connect. Tried before discovery on reconnect.
    endpoints: Option<Endpoints>,
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
        ctx: Arc<DeviceContext>,
        controller: Recipient<DeviceEvent>,
    ) -> Self {
        let cache: DeviceCache = ctx.store.load_cache(&cfg.key).unwrap_or_default();
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
            attempts: 0,
            attempt: 0,
            connected_at: None,
            generation: 0,
            np: None,
            volume: None,
            attention: None,
            seek: None,
            siri: None,
            siri_lock: Arc::default(),
            artwork_seq: 0,
            home_since: None,
            launched_at: None,
            media_flags: None,
            shown_file: None,
            item_art: ItemArtwork::default(),
            http_base: None,
            endpoints: None,
            expected_mute: None,
            expected_power: None,
            network_retry_end: None,
            command_timeouts: VecDeque::with_capacity(MAX_COMMAND_TIMEOUTS),
            cmd_lock: Arc::default(),
            timers: Timers::default(),
            apps_fetched: None,
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

    /// Status updates while a connect runs and there is no connection yet.
    fn running_connect(&self) -> Option<watch::Receiver<DeviceStatus>> {
        (self.tv.is_none() && self.status == DeviceStatus::Connecting)
            .then(|| self.status_tx.subscribe())
    }

    fn set_status(&mut self, status: DeviceStatus) {
        if self.status != status {
            self.status = status;
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
        let mut cfg = self.cfg.clone();
        let version = self.cfg_version;
        let mut options = ClientOptions::default();
        options.name.clone_from(&self.ctx.client_name);
        options.stall_timeout = Some(STALL_TIMEOUT);

        let known = self.endpoints;
        let start = Instant::now();

        let fut = async move {
            if let Some(ep) = known {
                match AppleTv::connect(&ep, &cfg.credentials, &options).await {
                    Ok(tv) => return (None, Ok((tv, ep, "last address"))),
                    // A scan fails the same way.
                    Err(e) if e.is_auth() || network_not_ready(&e) => {
                        return (None, Err(Some(e)));
                    }
                    Err(e) => debug!("[{}] last endpoints failed, scanning: {e}", cfg.name),
                }
            }
            let device = match discovery::find(&cfg).await {
                Ok(Some(d)) => d,
                Ok(None) => return (None, Err(None)),
                Err(e) => return (None, Err(Some(e))),
            };
            let changed = discovery::update_config(&mut cfg, &device);
            let ep = Endpoints::from(&device);
            let result = AppleTv::connect(&ep, &cfg.credentials, &options).await;
            (
                changed.then_some(cfg),
                result.map(|tv| (tv, ep, "scan")).map_err(Some),
            )
        };
        ctx.spawn(fut.into_actor(self).map(move |(cfg, result), act, ctx| {
            // What discovery found is valid also for an old attempt.
            if let Some(cfg) = cfg
                && act.cfg_version == version
            {
                act.set_config(cfg.clone());
                act.controller.do_send(DeviceEvent::Config {
                    key: act.key.clone(),
                    cfg,
                });
            }
            if act.attempt != attempt {
                debug!(
                    "[{}] result of an old connect attempt ignored",
                    act.cfg.name
                );
                if let Ok((tv, ..)) = result {
                    actix::spawn(async move {
                        let _ = tv.close().await;
                    });
                }
                return;
            }
            match result {
                Ok((tv, ep, via)) if act.want && act.status == DeviceStatus::Connecting => {
                    if act.cfg_version == version {
                        act.endpoints = Some(ep);
                    }
                    act.on_connected(tv, via, start, ctx);
                }
                Ok((tv, ..)) => {
                    actix::spawn(async move {
                        let _ = tv.close().await;
                    });
                }
                Err(Some(e)) if e.is_auth() => {
                    act.endpoints = None;
                    warn!(
                        "[{}] pairing is not valid anymore, pair again: {e}",
                        act.cfg.name
                    );
                    act.lost_connection(ctx);
                    act.set_status(DeviceStatus::AuthError);
                }
                Err(Some(e)) if network_not_ready(&e) && act.network_retry(ctx) => {
                    debug!("[{}] network not ready: {e}", act.cfg.name);
                }
                Err(e) => {
                    match e {
                        Some(e) => warn!("[{}] connect failed: {e}", act.cfg.name),
                        None => info!("[{}] not found on the network", act.cfg.name),
                    }
                    act.lost_connection(ctx);
                    act.schedule_reconnect(ctx);
                }
            }
        }));
    }

    /// `via` tells how the address was found, `start` is the start of the connect.
    fn on_connected(&mut self, tv: AppleTv, via: &str, start: Instant, ctx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let now = Instant::now();
        info!(
            "[{}] connected in {} ms ({via})",
            self.cfg.name,
            (now - start).as_millis()
        );
        self.connected_at = Some(now);
        self.network_retry_end = None;
        self.command_timeouts.clear();

        let missing = match (tv.companion().is_some(), tv.mrp().is_some()) {
            (true, true) => None,
            (false, _) => {
                Some("Companion: power state, apps, accounts and swipes are not available")
            }
            (_, false) => Some("MRP: now playing, volume and outputs are not available"),
        };
        if let Some(missing) = missing {
            warn!("[{}] not connected: {missing}", self.cfg.name);
            // `closed()` fires only for connected protocols, so the missing one needs a full reconnect.
            self.timers.full_reconnect =
                Some(ctx.run_interval(FULL_RECONNECT_INTERVAL, Self::full_reconnect));
        }

        let mut dirty = Dirty::CONNECTION;
        self.s.connected = true;
        self.s.mrp = tv.mrp().is_some();
        let own_uid = tv.mrp().and_then(|m| m.device_info().device_uid.clone());
        if self.s.own_output_uid != own_uid {
            self.s.own_output_uid = own_uid;
            dirty |= Dirty::OUTPUTS;
        }

        if let Some(rx) = tv.now_playing() {
            ctx.add_message_stream(changes(rx.clone(), WatchChanged::NowPlaying(generation)));
            self.np = Some(rx);
        }
        if let Some(rx) = tv.volume() {
            ctx.add_message_stream(changes(rx.clone(), WatchChanged::Volume(generation)));
            self.volume = Some(rx);
        }
        if let Some(companion) = tv.companion() {
            let events = BroadcastStream::new(companion.events()).map(move |e| CompanionEvent {
                generation,
                event: e.ok(),
            });
            ctx.add_message_stream(events);
        }

        let closed = tv.clone();
        ctx.spawn(async move { closed.closed().await }.into_actor(self).map(
            move |reason, act, ctx| {
                if act.generation == generation {
                    match reason {
                        Some(e) => warn!("[{}] connection closed: {e}", act.cfg.name),
                        None => info!("[{}] connection closed", act.cfg.name),
                    }
                    act.lost_connection(ctx);
                    act.schedule_reconnect(ctx);
                }
            },
        ));

        self.tv = Some(tv);
        self.set_status(DeviceStatus::Connected);
        self.query_power(ctx);
        // Until the device sends its state, the channels hold empty values. Using them would clear
        // what the Remote shows, so they are used only after the first update or after a wait.
        if self
            .np
            .as_ref()
            .is_some_and(|rx| rx.has_changed().unwrap_or(false))
        {
            dirty |= self.update_now_playing(ctx);
        }
        if self
            .volume
            .as_ref()
            .is_some_and(|rx| rx.has_changed().unwrap_or(false))
        {
            dirty |= self.update_volume();
        }
        ctx.run_later(FIRST_STATE_WAIT, move |act, ctx| {
            if act.generation == generation {
                let dirty = act.update_now_playing(ctx) | act.update_volume();
                act.emit(dirty);
            }
        });
        self.emit(dirty);

        self.poll(ctx);
        self.timers.poll = Some(ctx.run_interval(POLL_INTERVAL, Self::poll));
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
        self.home_since = None;
        self.media_flags = None;
        self.http_base = None;
        self.expected_mute = None;
        self.expected_power = None;
        for handle in [
            self.timers.poll.take(),
            self.timers.chapter.take(),
            self.timers.full_reconnect.take(),
            self.timers.screen.take(),
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
        for handle in [t.reconnect, t.poll, t.chapter, t.full_reconnect, t.screen]
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
        let now = Instant::now();
        let end = *self
            .network_retry_end
            .get_or_insert(now + NETWORK_RETRY_TIMEOUT);
        if now >= end {
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
            self.network_retry_end = None;
            self.connect(ctx);
        }
    }

    /// Reconnect all protocols. Skipped while Siri or a fast forward or rewind is active.
    fn full_reconnect(&mut self, ctx: &mut Context<Self>) {
        if self.siri.is_some() || self.seek.is_some() {
            debug!("[{}] full reconnect skipped: device is busy", self.cfg.name);
            return;
        }
        info!("[{}] reconnecting to get all protocols", self.cfg.name);
        // Keep the entity states: they become unavailable only if the connect fails.
        self.close(ctx);
        self.status = DeviceStatus::Disconnected;
        self.connect(ctx);
    }

    fn query_power(&self, ctx: &mut Context<Self>) {
        let Some(tv) = self.tv.clone() else { return };
        if tv.companion().is_none() {
            return;
        }
        let generation = self.generation;
        ctx.spawn(
            async move {
                match tv.companion() {
                    Some(c) => c.power_state().await,
                    None => Err(ratv::Error::NotConnected("Companion")),
                }
            }
            .into_actor(self)
            .map(move |result, act, _| match result {
                Ok(state) if act.generation == generation => act.set_attention(state),
                Ok(_) => {}
                Err(e) => warn!("[{}] power state failed: {e}", act.cfg.name),
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
        if self.home_since.is_some_and(|t| t.elapsed() < HOME_GRACE) {
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
        let dirty = self.s.update_now_playing(&rx.borrow());
        self.apply_now_playing(dirty, ctx)
    }

    fn apply_now_playing(&mut self, dirty: Dirty, ctx: &mut Context<Self>) -> Dirty {
        if dirty.any(Dirty::LANGUAGES) && log_enabled!(Level::Debug) {
            self.log_languages();
        }
        if dirty.any(Dirty::METADATA | Dirty::ARTWORK) {
            debug!(
                "[{}] item: {:?} / {:?} / {:?}, artwork id {:?}",
                self.cfg.name, self.s.title, self.s.artist, self.s.album, self.s.artwork_id
            );
        }
        if dirty.any(Dirty::ARTWORK) {
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
        let generation = self.generation;
        self.timers.screen = Some(ctx.run_later(SCREEN_SETTLE, move |act, ctx| {
            act.timers.screen = None;
            // Another screen opened after the launch: the launched app may not be in front.
            if act.launched_at.is_some_and(|t| t.elapsed() > LAUNCH_GRACE)
                && act.s.launched_app.take().is_some()
            {
                act.launched_at = None;
                act.emit(Dirty::APPS);
            }
            if act.generation != generation
                || act.home_since.is_some()
                || act.s.state == Some(PlaybackState::Playing)
                || act.media_flags.is_some_and(|f| f != 0)
            {
                return;
            }
            debug!("[{}] Home Screen: clearing now playing", act.cfg.name);
            act.home_since = Some(Instant::now());
            let dirty = act.s.update_now_playing(&NowPlaying::default());
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
            "[{}] audio: {:?} of {:?}, subtitles: {:?} of {:?}, auto: {}",
            self.cfg.name,
            id(self.s.audio_language.as_ref()),
            ids(&self.s.audio_languages),
            id(self.s.subtitle_language.as_ref()),
            ids(&self.s.subtitle_languages),
            self.s.subtitles_auto
        );
    }

    /// Use the artwork URL, or fetch the image and serve it from a file.
    ///
    /// The image is also fetched without an artwork id: some apps send none, but the Apple TV
    /// has the image.
    fn update_artwork(&mut self, ctx: &mut Context<Self>) {
        self.artwork_seq += 1;
        if !self.s.artwork_fetch {
            if let Some(url) = &self.s.artwork_url {
                debug!("[{}] artwork url: {url}", self.cfg.name);
            }
            self.shown_file = None;
            return;
        }
        let Some(tv) = self.tv.clone().filter(|_| self.s.state.is_some()) else {
            debug!("[{}] no artwork: nothing plays", self.cfg.name);
            return;
        };
        let id = self.s.artwork_id.clone();
        debug!("[{}] fetching artwork {id:?}", self.cfg.name);
        let generation = self.generation;
        let seq = self.artwork_seq;
        let item = self.s.item_seq;
        let item_key = self.s.item_key();
        // The Apple TV scales the image.
        ctx.spawn(
            async move { tv.artwork(f64::from(ARTWORK_SIZE), -1.0).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    if act.generation != generation || act.artwork_seq != seq {
                        return;
                    }
                    match result {
                        Ok(Some(art)) => {
                            // Without an artwork id, the image data gives a stable file name.
                            let id = id.unwrap_or_else(|| format!("{:016x}", fnv1a(&art.data)));
                            act.save_artwork(id, art, seq, (item, item_key), ctx);
                        }
                        // Some apps drop the image of the same item. Keep it until the item changes.
                        Ok(None) if act.s.artwork_item == item => {
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
                            if let Some(name) = act.cached_art(item_key) {
                                debug!("[{}] no artwork, using the image seen before for this item", act.cfg.name);
                                act.show_file(name, item);
                                return;
                            }
                            debug!(
                                "[{}] no artwork for this item, item before lasted {:?}",
                                act.cfg.name, act.s.prev_item_lasted
                            );
                            act.shown_file = None;
                            if act.s.artwork_url.take().is_some() {
                                act.emit(Dirty::ARTWORK);
                            }
                        }
                        Err(e) => warn!("[{}] artwork failed: {e}", act.cfg.name),
                    }
                }),
        );
    }

    /// Save the image. `item` is the item sequence and the item key at fetch time.
    fn save_artwork(
        &self,
        id: String,
        art: ratv::mrp::Artwork,
        seq: u64,
        (item, item_key): (u32, u64),
        ctx: &mut Context<Self>,
    ) {
        let store = self.ctx.artwork.clone();
        let key = self.key.clone();
        ctx.spawn(
            async move {
                store
                    .save(&key, &id, art.mime_type.as_deref(), &art.data)
                    .await
            }
            .into_actor(self)
            .map(move |result, act, _| {
                let name: Box<str> = match result {
                    Ok(name) => name.into(),
                    Err(e) => {
                        warn!("[{}] artwork save failed: {e}", act.cfg.name);
                        return;
                    }
                };
                act.item_art.remember(item_key, name.clone());
                if act.artwork_seq == seq {
                    act.show_file(name, item);
                }
            }),
        );
    }

    /// Show a kept artwork file.
    fn show_file(&mut self, name: Box<str>, item: u32) {
        if self.http_base.is_none() {
            self.http_base = self.find_http_base();
        }
        let Some(base) = &self.http_base else {
            warn!("[{}] no local address for the artwork URL", self.cfg.name);
            return;
        };
        let url = format!("{base}/artwork/{}/{name}", self.key);
        debug!("[{}] artwork file: {url}", self.cfg.name);
        self.s.artwork_url = Some(url);
        self.s.artwork_item = item;
        self.s.artwork_kept = false;
        self.shown_file = Some(name);
        self.emit(Dirty::ARTWORK);
    }

    /// The file seen before for the item, if it is still kept.
    fn cached_art(&self, item_key: u64) -> Option<Box<str>> {
        let name = self.item_art.get(item_key)?;
        self.ctx.artwork.touch(&self.key, name).then(|| name.into())
    }

    /// Base URL of the HTTP server, with an address on the network of the Apple TV.
    fn find_http_base(&self) -> Option<String> {
        let ip = self.ctx.http_address.or_else(|| {
            // No packet is sent: connect only selects the local address.
            let target = SocketAddr::new(self.cfg.address?, 9);
            let socket = UdpSocket::bind((IpAddr::from([0, 0, 0, 0]), 0)).ok()?;
            socket.connect(target).ok()?;
            Some(socket.local_addr().ok()?.ip())
        })?;
        Some(format!(
            "http://{}",
            SocketAddr::new(ip, self.ctx.http_port)
        ))
    }

    /// Update the chapter select when the next chapter starts.
    fn schedule_chapter(&mut self, ctx: &mut Context<Self>) {
        if let Some(handle) = self.timers.chapter.take() {
            ctx.cancel_future(handle);
        }
        let rate = f64::from(self.s.rate.unwrap_or(0.0));
        if rate <= 0.0 || self.s.chapters.is_empty() {
            return;
        }
        let Some(pos) = self.s.position(now()) else {
            return;
        };
        let Some(next) = self
            .s
            .chapters
            .iter()
            .filter_map(|c| c.start)
            .find(|&start| start > pos + 0.5)
        else {
            return;
        };
        let Some(delay) = chapter_delay(next - pos, rate) else {
            return;
        };
        self.timers.chapter = Some(ctx.run_later(delay, |act, ctx| {
            act.timers.chapter = None;
            act.emit(Dirty::CHAPTER);
            act.schedule_chapter(ctx);
        }));
    }

    /// Fetch the apps, accounts and AirPlay devices.
    fn poll(&mut self, ctx: &mut Context<Self>) {
        let Some(tv) = self.tv.clone() else { return };
        let generation = self.generation;

        if tv.companion().is_some() {
            let fetch_apps = self
                .apps_fetched
                .is_none_or(|t| t.elapsed() + APPS_MARGIN >= POLL_INTERVAL);
            let companion_tv = tv.clone();
            // One after the other: requests that time out together count as one silent period
            // only, and ratv closes the connection after two.
            let fut = async move {
                let apps = if fetch_apps {
                    Some(companion_tv.apps().await)
                } else {
                    None
                };
                (apps, companion_tv.accounts().await)
            };
            ctx.spawn(fut.into_actor(self).map(move |(apps, accounts), act, _| {
                act.apply_apps(apps, generation);
                match accounts {
                    Ok(accounts) if act.generation == generation => {
                        let accounts = accounts
                            .into_iter()
                            .map(|a| AccountInfo {
                                id: a.id,
                                name: a.name,
                            })
                            .collect();
                        let dirty = act.s.set_accounts(accounts);
                        act.emit(dirty);
                    }
                    Ok(_) => {}
                    Err(e) => warn!("[{}] account list failed: {e}", act.cfg.name),
                }
            }));
        }

        if tv.mrp().is_some() {
            let own_uid = self.s.own_output_uid.clone();
            ctx.spawn(
                async move { discovery::scan_airplay_outputs(own_uid.as_deref()).await }
                    .into_actor(self)
                    .map(move |result, act, _| match result {
                        Ok(devices) if act.generation == generation => {
                            let dirty = act.s.set_airplay_devices(devices);
                            if !dirty.is_empty() {
                                act.save_cache();
                                act.emit(dirty);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => warn!("[{}] AirPlay scan failed: {e}", act.cfg.name),
                    }),
            );
        }
    }

    fn apply_apps(
        &mut self,
        result: Option<Result<Vec<ratv::companion::App>, ratv::Error>>,
        generation: u64,
    ) {
        match result {
            Some(Ok(apps)) if self.generation == generation => {
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
            }
            Some(Err(e)) => warn!("[{}] app list failed: {e}", self.cfg.name),
            _ => {}
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

    /// Turn an entity command into calls.
    fn plan(&mut self, msg: &Command) -> Result<Vec<Call>, ServiceError> {
        let params = msg.params.as_ref();
        let run = match msg.kind {
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
        };
        // Kept only if the plan succeeds.
        let mut seek = self.seek;
        let mut planner = Planner {
            s: &self.s,
            seek: &mut seek,
            power: self.expected_power.unwrap_or(self.s.power),
            muted: self.expected_mute.or(self.s.muted),
            screensaver: self.attention == Some(AttentionState::Screensaver),
            powered: false,
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
            let tv = self.tv.as_ref().ok_or(ServiceError::NotConnected)?;
            if tv.companion().is_none() && tv.mrp().is_none() {
                return Err(ServiceError::ServiceUnavailable(
                    "Siri needs Companion or MRP".into(),
                ));
            }
        }
        if let Some(old) = self.siri.take() {
            info!(
                "[{}] Siri session {} replaced by {session_id}",
                self.cfg.name, old.session_id
            );
            old.replaced.store(true, Ordering::Relaxed);
        }
        let (audio, mut rx) = mpsc::channel::<Bytes>(SIRI_QUEUE);
        let replaced = Arc::new(AtomicBool::new(false));
        self.siri = Some(SiriSession {
            session_id,
            audio,
            replaced: replaced.clone(),
        });

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
        let ended = replaced.clone();
        // Returns the number of audio chunks sent to the Apple TV.
        let fut = move |tv: Option<AppleTv>| async move {
            let tv = tv.ok_or(ratv::Error::NotConnected("Apple TV"))?;
            // ratv allows one session. The session before ends after its audio is sent.
            let _active = tokio::time::timeout(SIRI_WAIT, lock.lock_owned())
                .await
                .map_err(|_| ratv::Error::SiriActive)?;
            // Replaced while it waited: do not start it.
            if replaced.load(Ordering::Relaxed) {
                return Ok(None);
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
        let start = connect_ended(connecting)
            .into_actor(self)
            .then(move |(), act, _| fut(act.tv.clone()).into_actor(act));
        ctx.spawn(start.map(move |result, act, _| {
            if act
                .siri
                .as_ref()
                .is_some_and(|s| s.session_id == session_id)
            {
                act.siri = None;
            }
            let replaced = ended.load(Ordering::Relaxed);
            let event = siri_event(&act.cfg.name, entity_id, session_id, result, replaced);
            act.controller.do_send(DeviceEvent::Assistant(event));
        }));
        Ok(())
    }
}

/// Starts Siri on the path of `mode`.
async fn start_siri(tv: &AppleTv, mode: SiriMode) -> Result<ratv::Siri, ratv::Error> {
    if mode == SiriMode::Auto {
        return tv.start_siri().await;
    }
    let mut siri = tv
        .mrp()
        .ok_or(ratv::Error::NotConnected("MRP"))?
        .start_siri()
        .await?;
    siri.set_paced(mode != SiriMode::Mrp);
    if mode == SiriMode::MrpPacedDelay {
        siri.set_release_delay(SIRI_RELEASE_DELAY);
    }
    Ok(ratv::Siri::Mrp(siri))
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
        // Seen when Siri is off on the Apple TV.
        Err(e @ ratv::Error::SiriNotRecording) => {
            warn!("[{name}] Siri failed: {e}. Is Siri on in the Apple TV settings?");
            "Apple TV did not start listening. Check that Siri is on: Settings > General > Siri."
                .to_string()
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

/// Wait until a running connect has ended, at most [`CONNECT_WAIT`].
async fn connect_ended(status: Option<watch::Receiver<DeviceStatus>>) {
    if let Some(mut status) = status {
        let ended = status.wait_for(|s| *s != DeviceStatus::Connecting);
        let _ = tokio::time::timeout(CONNECT_WAIT, ended).await;
    }
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
        self.close(ctx);
        self.ctx.artwork.remove(&self.key);
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
        if msg.generation != self.generation {
            return;
        }
        match msg.event {
            Some(Event::SystemStatus(state)) => self.set_attention(state),
            Some(Event::ScreenChanged { .. }) => self.screen_changed(ctx),
            Some(Event::MediaControl(flags)) => {
                self.media_flags = Some(flags.0);
                if flags.0 != 0 {
                    self.leave_home(ctx);
                }
            }
            Some(_) => {}
            // Events were lost: ask again.
            None => self.query_power(ctx),
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
        self.set_status(DeviceStatus::Disconnected);
    }
}

impl Handler<Stop> for AppleTvActor {
    type Result = ();

    fn handle(&mut self, _: Stop, ctx: &mut Self::Context) {
        self.want = false;
        ctx.stop();
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
        }
    }
}

impl Handler<Command> for AppleTvActor {
    type Result = ResponseActFuture<Self, Result<(), ServiceError>>;

    fn handle(&mut self, msg: Command, ctx: &mut Self::Context) -> Self::Result {
        if msg.kind == EntityKind::VoiceAssistant {
            let result = match msg.cmd_id.as_str() {
                "voice_start" => self.voice_start(msg.params.as_ref(), ctx),
                _ => Err(ServiceError::NotSupported(msg.cmd_id)),
            };
            return Box::pin(fut::ready(result));
        }
        // The core can send a command before `exit_standby`.
        if self.tv.is_none() && self.status == DeviceStatus::Disconnected {
            debug!("[{}] command while disconnected, connecting", self.cfg.name);
            self.start_connect(ctx);
        }
        // A command can come right after the Remote wakes up, while the connect still runs.
        let connecting = self.running_connect();
        if self.tv.is_none() && connecting.is_none() {
            return Box::pin(fut::ready(Err(ServiceError::NotConnected)));
        }
        // Plan after the command before has ended, so toggles see its result.
        let lock = self.cmd_lock.clone();
        Box::pin(
            async move {
                connect_ended(connecting).await;
                lock.lock_owned().await
            }
            .into_actor(self)
            .then(move |guard, act, _| act.run_command(&msg, guard)),
        )
    }
}

impl AppleTvActor {
    /// Plan and run a command. `guard` is held until the results are applied.
    fn run_command(
        &mut self,
        msg: &Command,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> impl ActorFuture<Self, Output = Result<(), ServiceError>> + use<> {
        let generation = self.generation;
        let prepared = match (self.tv.clone(), self.plan(msg)) {
            (None, _) => Err(ServiceError::NotConnected),
            (_, Err(e)) => Err(e),
            (Some(tv), Ok(calls)) => Ok((tv, calls)),
        };
        let mut account = None;
        let mut launched = None;
        let mut mute = None;
        let mut power = None;
        let mut begins_seek = false;
        if let Ok((_, calls)) = &prepared {
            debug!("[{}] {} -> {calls:?}", self.cfg.name, msg.cmd_id);
            for call in calls {
                match call {
                    // The current account is not reported by the Apple TV, so it is set after a switch.
                    Call::SwitchAccount(_, name) => account = Some(name.clone()),
                    // The app in front is not reported either.
                    Call::LaunchApp(id) => launched = Some(id.clone()),
                    Call::Mute(m) => mute = Some(*m),
                    Call::TurnOn => power = Some(Power::On),
                    Call::TurnOff => power = Some(Power::Off),
                    Call::BeginFastForward | Call::BeginRewind => begins_seek = true,
                    _ => {}
                }
            }
        }
        let fut = async move {
            let result = match prepared {
                Ok((tv, calls)) => exec_all(&tv, calls).await.map_err(ServiceError::from),
                Err(e) => Err(e),
            };
            (result, guard)
        };
        let cmd_id = msg.cmd_id.clone();
        fut.into_actor(self).map(move |(result, _guard), act, ctx| {
            let ok = result.is_ok();
            if let Err(e) = &result {
                warn!("[{}] {cmd_id} failed: {e}", act.cfg.name);
            }
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
                if power.is_some() {
                    act.expected_power = power.filter(|_| ok);
                }
            }
            if !ok {
                return result;
            }
            if let Some(name) = account {
                act.s.current_account = Some(name);
                act.emit(Dirty::ACCOUNTS);
            }
            if let Some(id) = launched {
                act.s.launched_app = Some(id);
                act.launched_at = Some(Instant::now());
                act.emit(Dirty::APPS);
            }
            result
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
        let Some(siri) = &self.siri else {
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
        if siri.audio.try_send(msg.data).is_err() {
            warn!("[{}] Siri audio queue full, chunk dropped", self.cfg.name);
        }
        if end {
            // Dropping the sender also ends the session if the end marker was dropped.
            self.siri = None;
        }
    }
}

/// Returns `true` for errors that come while the network of the Remote starts, for example after wake-up.
fn network_not_ready(e: &ratv::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e,
        ratv::Error::Io(io) if matches!(
            io.kind(),
            ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable | ErrorKind::NetworkDown
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_errors() {
        use std::io::{Error, ErrorKind};
        assert!(network_not_ready(
            &Error::from(ErrorKind::NetworkUnreachable).into()
        ));
        assert!(network_not_ready(
            &Error::from(ErrorKind::HostUnreachable).into()
        ));
        assert!(!network_not_ready(
            &Error::from(ErrorKind::ConnectionRefused).into()
        ));
        assert!(!network_not_ready(&ratv::Error::Timeout));
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
