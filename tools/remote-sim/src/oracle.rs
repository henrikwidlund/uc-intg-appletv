// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! A second connection to the Apple TV. It gives the values that the Apple TV has, to compare
//! them with the values that the driver shows.
//!
//! It uses its own pairing. With the credentials of the driver, the Apple TV closes the Companion
//! connection of the driver.

use crate::Fail;
use ratv::companion::{App, AttentionState};
use ratv::discovery::{MDNS_PORT, ScanOptions, scan};
use ratv::mrp::{NowPlaying, PlaybackState, Volume};
use ratv::{AppleTv, ClientOptions, Credentials, Endpoints, ExposeSecret, PairOptions, Protocol};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Client name shown on the Apple TV.
const NAME: &str = "remote-sim";

/// Content that a check needs on the Apple TV. The order is used only to not ask again after a
/// decline: a check skips when the owner declined its content or an earlier variant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Content {
    /// Something that plays and has a title.
    Playing,
    /// A video with artwork.
    Video,
    /// A movie with chapters and more than one audio or subtitle language.
    Movie,
}

impl Content {
    /// What the owner opens when the content is missing.
    pub const fn text(self) -> &'static str {
        match self {
            Self::Movie => "a movie with chapters and more than one audio or subtitle language",
            Self::Video => "a video with artwork",
            Self::Playing => "a video",
        }
    }

    /// Start of the request to open the content, after a text that says what is wrong.
    pub const fn open(self) -> &'static str {
        match self {
            Self::Movie => "Open such a movie",
            Self::Video => "Open such a video",
            Self::Playing => "Open a video",
        }
    }
}

pub struct Oracle {
    tv: AppleTv,
    pub apps: Vec<App>,
}

async fn endpoints(address: IpAddr) -> Result<Endpoints, Fail> {
    let mut options = ScanOptions::default();
    options.timeout = Duration::from_secs(3);
    options.targets = vec![SocketAddr::new(address, MDNS_PORT)];
    let devices = scan(&options).await?;
    let device = devices
        .iter()
        .find(|d| d.addresses.iter().any(|a| a.ip == address))
        .ok_or_else(|| Fail::new(format!("no Apple TV answers at {address}")))?;
    Ok(Endpoints::from(device))
}

/// Pair the test program with the Apple TV. The PIN is read from the terminal.
pub async fn pair(address: IpAddr, file: &Path) -> Result<(), Fail> {
    let ep = endpoints(address).await?;
    let mut options = PairOptions::default();
    options.name = NAME.into();
    let prompt = ratv::pair(&ep, &options).await?;
    println!("Type the PIN shown on the Apple TV and press Enter:");
    let mut pin = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut pin)
        .await?;
    let credentials = prompt.finish(pin.trim()).await?;
    write_private(file, credentials.to_secret_string().expose_secret())?;
    println!("Saved the pairing to {}", file.display());
    Ok(())
}

/// Write a file that only the owner can read.
pub fn write_private(file: &Path, text: &str) -> Result<(), Fail> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(file)?.write_all(text.as_bytes())?;
    Ok(())
}

impl Oracle {
    pub async fn connect(address: IpAddr, file: &Path) -> Result<Self, Fail> {
        let text = tokio::fs::read_to_string(file).await.map_err(|e| {
            Fail::new(format!(
                "no pairing in {} ({e}), run `remote-sim pair <address>` first",
                file.display()
            ))
        })?;
        let credentials: Credentials = text
            .trim()
            .parse()
            .map_err(|_| Fail::new(format!("invalid pairing in {}", file.display())))?;
        let ep = endpoints(address).await?;
        let mut options = ClientOptions::default();
        options.name = NAME.into();
        let tv = AppleTv::connect(&ep, &credentials, &options)
            .await
            .map_err(|e| {
                if e.is_auth() {
                    Fail::new(format!(
                        "{e}. The pairing in {} is not valid anymore: run `remote-sim pair \
                         <address>`",
                        file.display()
                    ))
                } else {
                    e.into()
                }
            })?;
        ensure_protocols(&tv)?;
        let apps = tv.apps().await?;
        Ok(Self { tv, apps })
    }

    pub async fn close(&self) {
        let _ = self.tv.close().await;
    }

    pub async fn power(&self) -> Result<AttentionState, Fail> {
        let companion = self
            .tv
            .companion()
            .ok_or_else(|| Fail::new("no Companion connection"))?;
        Ok(companion.power_state().await?)
    }

    pub fn now_playing(&self) -> NowPlaying {
        self.tv.now_playing().borrow().clone()
    }

    /// Playback state, without a copy of now playing.
    pub fn playback_state(&self) -> Option<PlaybackState> {
        self.tv.now_playing().borrow().state
    }

    /// Playback rate, without a copy of now playing.
    pub fn rate(&self) -> Option<f32> {
        self.tv.now_playing().borrow().rate
    }

    /// Set the playback rate on the Apple TV, like the speed menu of the player.
    pub async fn set_rate(&self, rate: f32) -> Result<(), ratv::Error> {
        self.tv.set_playback_rate(rate).await
    }

    /// True if ratv sends `rate`: the app lists it and has the rate command enabled.
    pub fn accepts_rate(&self, rate: f32) -> bool {
        self.tv.now_playing().borrow().accepts_rate(rate)
    }

    /// Normal playback speeds that the app lists. Empty if the app lists no normal speed other
    /// than 1.0.
    pub fn playback_speeds(&self) -> Vec<f32> {
        self.tv.now_playing().borrow().playback_speeds()
    }

    /// Send speech to Siri over this connection, not through the driver, like the `remote`
    /// example of ratv: Companion if it is open, else MRP. One chunk of 100 ms every 100 ms.
    /// Returns the protocol and the number of samples pushed to ratv (ratv may still hold up to
    /// about 100 ms of it). Fewer than in `pcm` means that the Apple TV stopped recording first.
    /// The error has `true` if Siri had started.
    pub async fn ask_siri(
        &self,
        pcm: &[i16],
    ) -> Result<(&'static str, usize), (ratv::Error, bool)> {
        let mut siri = self.tv.start_siri().await.map_err(|e| (e, false))?;
        let protocol = match siri {
            ratv::Siri::Companion(_) => "Companion",
            ratv::Siri::Mrp(_) => "MRP",
        };
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut sent = 0;
        for chunk in pcm.chunks(1600) {
            tick.tick().await;
            if siri.ended() {
                break;
            }
            match siri.push_pcm(chunk).await {
                // ratv keeps the audio and sends it with the next chunk.
                Ok(()) | Err(ratv::Error::QueueFull) => {}
                Err(e) => return Err((e, true)),
            }
            sent += chunk.len();
        }
        siri.finish().await.map_err(|e| (e, true))?;
        Ok((protocol, sent))
    }

    /// True if Siri is on in the Apple TV settings. `None` if the Apple TV does not say.
    pub fn siri_enabled(&self) -> Option<bool> {
        self.tv.siri_enabled()
    }

    /// True if the MRP connection is open.
    pub fn has_mrp(&self) -> bool {
        self.tv.is_open(Protocol::Mrp)
    }

    /// What the current item lacks for `need`. `None` if it has all of it. Needs MRP.
    pub fn missing(&self, need: Content) -> Option<String> {
        if !self.tv.is_open(Protocol::Mrp) {
            return Some("no MRP on the second connection".into());
        }
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        let mut out = Vec::new();
        let plays = matches!(
            np.state,
            Some(
                PlaybackState::Playing
                    | PlaybackState::Paused
                    | PlaybackState::Interrupted
                    | PlaybackState::Seeking
            )
        );
        if !plays {
            out.push("nothing plays");
        }
        let item = np.title.as_deref().is_some_and(|t| !t.is_empty());
        if !item {
            out.push("no title");
        }
        match need {
            Content::Movie => {
                if np.chapters.is_empty() && np.chapter_count.unwrap_or(0) <= 0 {
                    out.push("no chapters");
                }
                // Auto is not a language.
                let subtitles = np
                    .subtitle_languages
                    .iter()
                    .filter(|o| !o.is_auto())
                    .count();
                if np.audio_languages.len() < 2 && subtitles < 2 {
                    out.push("less than 2 audio and less than 2 subtitle languages");
                }
                if np.is_live == Some(true) {
                    out.push("live content");
                }
            }
            Content::Video => {
                // The driver uses the artwork URL, the image data of Companion, or fetches the
                // image over MRP, also without an id.
                if np.artwork_id.is_none() && np.artwork_url.is_none() && np.artwork_data.is_none()
                {
                    out.push("no artwork");
                }
            }
            Content::Playing => {}
        }
        (!out.is_empty()).then(|| out.join(", "))
    }

    /// True if now playing has an item: item metadata, an artwork id, an artwork URL or image
    /// data. `None` without now playing or without MRP: Companion alone can have no title while
    /// an item plays. It does not copy now playing. The artwork rule of the driver uses fewer
    /// fields: it has no radio station, content id, artwork URL or image data.
    pub fn has_item(&self) -> Option<bool> {
        if !self.tv.is_open(Protocol::Mrp) {
            return None;
        }
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        let text = |v: &Option<String>| v.as_deref().is_some_and(|t| !t.is_empty());
        let item = [
            &np.title,
            &np.subtitle,
            &np.artist,
            &np.album,
            &np.series_name,
            &np.radio_station,
            &np.content_identifier,
        ]
        .into_iter()
        .any(text)
            || np.artwork_id.is_some()
            || np.artwork_url.is_some()
            || np.artwork_data.is_some();
        Some(item)
    }

    /// Number of audio and subtitle languages. It does not copy now playing. `None` without
    /// MRP: Companion has no languages.
    pub fn language_counts(&self) -> Option<(usize, usize)> {
        if !self.tv.is_open(Protocol::Mrp) {
            return None;
        }
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        Some((np.audio_languages.len(), np.subtitle_languages.len()))
    }

    /// True if the Apple TV sets the live flag. This is only the device flag, not the rule of
    /// the `is_live` sensor of the driver.
    pub fn is_live(&self) -> bool {
        self.tv.now_playing().borrow().is_live == Some(true)
    }

    /// What plays now, for example `'Downsizing' (TV)`. `None` if nothing plays.
    pub fn now_text(&self) -> Option<String> {
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        let plays = matches!(
            np.state,
            Some(
                PlaybackState::Playing
                    | PlaybackState::Paused
                    | PlaybackState::Interrupted
                    | PlaybackState::Seeking
            )
        );
        if !plays {
            return None;
        }
        let title = match np.title.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => format!("'{t}'"),
            None => "an item without a title".to_owned(),
        };
        Some(match np.app.as_deref().and_then(|b| self.app_name(b)) {
            Some(app) => format!("{title} ({app})"),
            None => title,
        })
    }

    /// The media control flags from Companion. `None` if Companion does not say.
    pub fn media_flags(&self) -> Option<u32> {
        self.tv.media_controls().map(|f| f.0)
    }

    /// True if the Apple TV shows the Home Screen (no app shows a player) and nothing plays. The
    /// driver then clears an old paused item. False if Companion does not say.
    pub fn home_screen(&self) -> bool {
        self.tv.media_controls().is_some_and(|f| f.0 == 0)
            && self.playback_state() != Some(PlaybackState::Playing)
    }

    /// Name of the app that plays now, with the state Playing. A paused item can be an old
    /// item of another app, for example Music after the TV app stopped. Without a copy.
    pub fn active_app(&self) -> Option<&str> {
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        if np.state != Some(PlaybackState::Playing) {
            return None;
        }
        self.app_name(np.app.as_deref()?)
    }

    /// Name of the app that plays, without a copy of now playing.
    pub fn playing_app(&self) -> Option<&str> {
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        self.app_name(np.app.as_deref()?)
    }

    /// App of now playing: its name, or the bundle id when the app list does not have it.
    pub fn app_text(&self) -> Option<String> {
        let rx = self.tv.now_playing();
        let np = rx.borrow();
        let id = np.app.as_deref()?;
        Some(self.app_name(id).unwrap_or(id).to_owned())
    }

    pub fn volume(&self) -> Option<Volume> {
        self.tv.volume().map(|rx| rx.borrow().clone())
    }

    /// Name of the app with this bundle id.
    pub fn app_name(&self, bundle_id: &str) -> Option<&str> {
        self.apps
            .iter()
            .find(|a| a.bundle_id == bundle_id)
            .map(|a| a.name.as_str())
    }
}

fn ensure_protocols(tv: &AppleTv) -> Result<(), Fail> {
    match (tv.companion().is_some(), tv.mrp().is_some()) {
        (true, true) => Ok(()),
        (false, _) => Err(Fail::new("the second connection has no Companion")),
        (_, false) => Err(Fail::new("the second connection has no MRP")),
    }
}

/// The Apple TV is on.
pub const fn is_awake(state: AttentionState) -> bool {
    !matches!(state, AttentionState::Asleep | AttentionState::Unknown)
}
