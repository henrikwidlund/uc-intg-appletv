// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Configured Apple TV devices and per-device cache files.
//!
//! Files are written to a temporary file first and then renamed, so a crash never leaves a broken
//! file. The configuration holds credentials and is only readable by the owner.
//!
//! The data is serialized on the caller thread. One background thread writes the files in order,
//! so the actor thread does not wait for the disk.

use log::{error, info};
use ratv::{Credentials, ExposeSecret};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const CONFIG_FILE: &str = "config.json";
const SETTINGS_FILE: &str = "settings.json";
/// Length of the device key used in entity ids.
pub const KEY_LEN: usize = 12;

/// One configured Apple TV.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Identifier at pairing time. It never changes.
    pub id: String,
    /// Key for the entity ids, see [`device_key`].
    pub key: String,
    /// Device name.
    pub name: String,
    /// Latest AirPlay, MRP and Companion identifiers. They can change after tvOS updates.
    #[serde(default)]
    pub known_ids: Vec<String>,
    /// Last known IP address.
    #[serde(default)]
    pub address: Option<IpAddr>,
    /// The address was entered by the user. Multicast discovery is only used as a fallback.
    #[serde(default)]
    pub manual_address: bool,
    /// Pairing credentials.
    #[serde(
        serialize_with = "ser_credentials",
        deserialize_with = "de_credentials"
    )]
    pub credentials: Credentials,
    /// Change the volume of all AirPlay outputs together.
    #[serde(default = "default_true")]
    pub global_volume: bool,
}

impl DeviceConfig {
    /// Returns `true` if one of the identifiers belongs to this device.
    pub fn matches<'a>(&self, mut ids: impl Iterator<Item = &'a str>) -> bool {
        ids.any(|id| id == self.id || self.known_ids.iter().any(|k| k == id))
    }
}

const fn default_true() -> bool {
    true
}

fn ser_credentials<S: Serializer>(credentials: &Credentials, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(credentials.to_secret_string().expose_secret())
}

fn de_credentials<'de, D: Deserializer<'de>>(d: D) -> Result<Credentials, D::Error> {
    let value = String::deserialize(d)?;
    value.parse().map_err(serde::de::Error::custom)
}

/// Derive the entity id key from the device identifier.
///
/// - An AirPlay `deviceid` (`AA:BB:CC:DD:EE:FF`) gives its 12 hex digits.
/// - Other identifiers (MRP `UniqueIdentifier`, Companion `rpMRtID`) give the first 12 hex digits
///   of a 64-bit FNV-1a hash. FNV-1a is used because the output must never change. The std
///   `DefaultHasher` can change between Rust versions.
pub fn device_key(id: &str) -> String {
    let hex: String = id.chars().filter(|c| *c != ':').collect();
    if hex.len() == KEY_LEN && id.len() == 17 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return hex.to_ascii_uppercase();
    }
    let hash = fnv1a(id.as_bytes());
    let mut key = format!("{hash:016X}");
    key.truncate(KEY_LEN);
    key
}

/// Version of the backup format.
const BACKUP_VERSION: u8 = 1;

#[derive(Serialize)]
struct BackupRef<'a> {
    version: u8,
    devices: &'a [DeviceConfig],
}

#[derive(Deserialize)]
struct Backup {
    version: u8,
    devices: Vec<DeviceConfig>,
}

/// Serialize the devices for a backup. The text contains the pairing credentials.
pub fn backup_json(devices: &[DeviceConfig]) -> Option<String> {
    serde_json::to_string(&BackupRef {
        version: BACKUP_VERSION,
        devices,
    })
    .ok()
}

/// Read the devices of a backup. Returns `None` if the text is not a valid backup.
///
/// The reason is not returned, because the text contains secrets and parse errors can quote it.
pub fn parse_backup(text: &str) -> Option<Vec<DeviceConfig>> {
    let backup: Backup = serde_json::from_str(text.trim()).ok()?;
    if backup.version != BACKUP_VERSION || backup.devices.is_empty() {
        return None;
    }
    let mut keys: Vec<&str> = Vec::with_capacity(backup.devices.len());
    for device in &backup.devices {
        if device.key != device_key(&device.id) || keys.contains(&device.key.as_str()) {
            return None;
        }
        keys.push(&device.key);
    }
    Some(backup.devices)
}

/// 64-bit FNV-1a hash. The output never changes between Rust versions.
pub fn fnv1a(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    data.iter()
        .fold(OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME))
}

/// Driver settings that are not part of a device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverSettings {
    /// Log debug messages of the driver and ratv.
    #[serde(default)]
    pub debug_log: bool,
    #[serde(default)]
    pub siri_timing: SiriTiming,
}

/// How the Siri audio is sent to the Apple TV. A test setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SiriTiming {
    /// Audio kept while Siri starts is sent at once.
    #[default]
    Normal,
    /// Audio is sent no faster than real time.
    Paced,
    /// Paced, and Siri is released 250 ms after the last audio.
    PacedDelay,
}

impl SiriTiming {
    pub const ALL: [Self; 3] = [Self::Normal, Self::Paced, Self::PacedDelay];

    /// Id in the setup page.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Paced => "paced",
            Self::PacedDelay => "paced_delay",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Paced => "Paced",
            Self::PacedDelay => "Paced + 250 ms release delay",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }

    /// Value of `self as u8`. Unknown values give `Normal`.
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Paced,
            2 => Self::PacedDelay,
            _ => Self::Normal,
        }
    }
}

/// Load the driver settings. A missing or broken file gives the defaults.
///
/// It is a free function, because the logger needs the settings before the store exists.
pub fn load_settings(dir: &Path) -> DriverSettings {
    read_json(&dir.join(SETTINGS_FILE))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Reads and writes the configuration files.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    dir: PathBuf,
    writer: Arc<Writer>,
}

impl ConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            writer: Arc::new(Writer::new()),
        }
    }

    /// Load the configured devices. A missing file gives an empty list.
    ///
    /// A broken file is moved to `config.json.bad-<unix time>`, so the next save does not
    /// overwrite it.
    pub fn load_devices(&self) -> Vec<DeviceConfig> {
        self.writer.flush();
        let path = self.dir.join(CONFIG_FILE);
        match read_json(&path) {
            Ok(Some(devices)) => devices,
            Ok(None) => Vec::new(),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let bad = self.dir.join(format!("{CONFIG_FILE}.bad-{secs}"));
                match fs::rename(&path, &bad) {
                    Ok(()) => error!(
                        "Invalid {}: {e}. The file was moved to {}",
                        path.display(),
                        bad.display()
                    ),
                    Err(re) => error!(
                        "Invalid {}: {e}. Could not move the file to {}: {re}",
                        path.display(),
                        bad.display()
                    ),
                }
                Vec::new()
            }
            Err(e) => {
                error!("Error reading {}: {e}", path.display());
                Vec::new()
            }
        }
    }

    /// Save the configured devices. Waits for the write, so the caller gets write errors.
    pub fn save_devices(&self, devices: &[DeviceConfig]) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(&devices).map_err(io::Error::other)?;
        let (tx, rx) = mpsc::sync_channel(1);
        self.writer.send(Job::Write {
            path: self.dir.join(CONFIG_FILE),
            data,
            private: true,
            done: Some(tx),
        });
        rx.recv().map_err(io::Error::other)??;
        info!("Saved configuration with {} device(s)", devices.len());
        Ok(())
    }

    /// Queue the save of the configured devices and return at once. A write error is only
    /// logged. Use it where no caller waits for the result.
    pub fn queue_devices(&self, devices: &[DeviceConfig]) -> io::Result<()> {
        self.write_json(self.dir.join(CONFIG_FILE), devices, true)
    }

    /// Queue the save of the driver settings. A write error is only logged.
    pub fn save_settings(&self, settings: DriverSettings) -> io::Result<()> {
        self.write_json(self.dir.join(SETTINGS_FILE), &settings, false)
    }

    /// Load the cache of a device. A missing or broken file gives `None`.
    pub fn load_cache<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.writer.flush();
        let path = self.cache_path(key);
        read_json(&path).unwrap_or_else(|e| {
            error!("Error reading {}: {e}", path.display());
            None
        })
    }

    /// Save the cache of a device. Write errors are only logged.
    pub fn save_cache<T: Serialize>(&self, key: &str, cache: &T) -> io::Result<()> {
        self.write_json(self.cache_path(key), cache, false)
    }

    /// Delete the cache of a device.
    pub fn remove_cache(&self, key: &str) {
        self.writer.send(Job::Remove(self.cache_path(key)));
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("cache_{key}.json"))
    }

    /// Serialize the value and queue the write. Returns only serialization errors.
    fn write_json<T: Serialize + ?Sized>(
        &self,
        path: PathBuf,
        value: &T,
        private: bool,
    ) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        self.writer.send(Job::Write {
            path,
            data,
            private,
            done: None,
        });
        Ok(())
    }
}

/// File changes for the background thread.
#[derive(Debug)]
enum Job {
    Write {
        path: PathBuf,
        data: Vec<u8>,
        private: bool,
        /// Gets the result. Without it, errors are only logged.
        done: Option<mpsc::SyncSender<io::Result<()>>>,
    },
    Remove(PathBuf),
    /// Answer when all earlier jobs are done.
    Flush(mpsc::SyncSender<()>),
}

impl Job {
    fn run(self) {
        match self {
            Self::Write {
                path,
                data,
                private,
                done,
            } => {
                let result = write_file(&path, &data, private);
                if let Err(e) = &result {
                    error!("Error writing {}: {e}", path.display());
                }
                if let Some(done) = done {
                    let _ = done.send(result);
                }
            }
            Self::Remove(path) => {
                if let Err(e) = fs::remove_file(&path)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    error!("Error deleting {}: {e}", path.display());
                }
            }
            Self::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Runs the jobs in order on one thread. When the last [`ConfigStore`] is dropped, the queued
/// jobs are done before the thread ends.
#[derive(Debug)]
struct Writer {
    tx: Option<mpsc::Sender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Writer {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        match thread::Builder::new()
            .name("config-writer".into())
            .spawn(move || rx.into_iter().for_each(Job::run))
        {
            Ok(thread) => Self {
                tx: Some(tx),
                thread: Some(thread),
            },
            Err(e) => {
                error!("Could not start the config writer thread, writing directly: {e}");
                Self {
                    tx: None,
                    thread: None,
                }
            }
        }
    }

    /// Queue the job. Without the thread, the job runs on the caller thread.
    fn send(&self, job: Job) {
        let job = match &self.tx {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        job.run();
    }

    /// Wait until all queued jobs are done.
    fn flush(&self) {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Job::Flush(tx));
        let _ = rx.recv();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the channel ends the thread after the queued jobs.
        self.tx = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("The config writer thread failed");
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_file(
    path: &Path,
    data: &[u8],
    // Only Unix has file modes.
    #[cfg_attr(not(unix), expect(unused_variables))] private: bool,
) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    // A file left from a failed write keeps its old mode, so always create a new file.
    if let Err(e) = fs::remove_file(&tmp)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // Make the rename durable.
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDS: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1:\
        b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2:\
        4141:4242";

    fn device() -> DeviceConfig {
        DeviceConfig {
            id: "AA:BB:CC:DD:EE:FF".into(),
            key: device_key("AA:BB:CC:DD:EE:FF"),
            name: "Living Room".into(),
            known_ids: vec!["AA:BB:CC:DD:EE:00".into(), "UUID-1".into()],
            address: Some("10.0.30.6".parse().expect("ip")),
            manual_address: false,
            credentials: CREDS.parse().expect("credentials"),
            global_volume: true,
        }
    }

    #[test]
    fn key_from_airplay_id() {
        assert_eq!("AABBCCDDEEFF", device_key("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn key_from_uuid_is_stable_and_short() {
        let id = "2F4C3D10-7A2B-4D8E-9F01-23456789ABCD";
        let key = device_key(id);
        assert_eq!(KEY_LEN, key.len());
        assert_eq!(key, device_key(id));
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        // Known FNV-1a value, so the key never changes between builds.
        assert_eq!("CBF29CE48422", device_key(""));
    }

    #[test]
    fn matches_any_known_id() {
        let d = device();
        assert!(d.matches(["X", "AA:BB:CC:DD:EE:FF"].into_iter()));
        assert!(d.matches(std::iter::once("UUID-1")));
        assert!(!d.matches(["UUID-2", "AA:BB:CC:DD:EE:11"].into_iter()));
    }

    #[test]
    fn siri_timing_ids() {
        for t in SiriTiming::ALL {
            assert_eq!(Some(t), SiriTiming::from_id(t.id()));
            assert_eq!(t, SiriTiming::from_u8(t as u8));
        }
        assert_eq!(None, SiriTiming::from_id("fast"));
        // Older settings files have no Siri timing.
        let old: DriverSettings = serde_json::from_str(r#"{"debug_log":true}"#).expect("json");
        assert_eq!(SiriTiming::Normal, old.siri_timing);
    }

    #[test]
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "uc-intg-appletv-test-settings-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        assert_eq!(DriverSettings::default(), load_settings(&dir));

        let store = ConfigStore::new(&dir);
        let settings = DriverSettings {
            debug_log: true,
            siri_timing: SiriTiming::PacedDelay,
        };
        store.save_settings(settings).expect("save");
        store.writer.flush();
        assert_eq!(settings, load_settings(&dir));

        // A broken file gives the defaults.
        fs::write(dir.join(SETTINGS_FILE), b"{").expect("write");
        assert_eq!(DriverSettings::default(), load_settings(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_round_trip() {
        let dir = std::env::temp_dir().join(format!("uc-intg-appletv-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let store = ConfigStore::new(&dir);
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());

        // A file left from a failed write must not keep its mode.
        fs::write(dir.join("config.json.tmp"), b"x").expect("tmp");
        let devices = vec![device()];
        store.save_devices(&devices).expect("save");
        assert_eq!(devices, store.load_devices());

        store.queue_devices(&[]).expect("queue");
        store.writer.flush();
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());
        store.queue_devices(&devices).expect("queue");
        store.writer.flush();
        assert_eq!(devices, store.load_devices());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(CONFIG_FILE))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(0o600, mode & 0o777);
        }

        store
            .save_cache("KEY", &vec!["a".to_string()])
            .expect("cache");
        assert_eq!(
            Some(vec!["a".to_string()]),
            store.load_cache::<Vec<String>>("KEY")
        );
        store.remove_cache("KEY");
        assert_eq!(None, store.load_cache::<Vec<String>>("KEY"));

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn backup_round_trip() {
        let devices = vec![device()];
        let text = backup_json(&devices).expect("backup");
        assert_eq!(Some(devices), parse_backup(&format!("  {text}\n")));
    }

    #[test]
    fn invalid_backups_are_rejected() {
        let valid = backup_json(&[device()]).expect("backup");
        assert_eq!(None, parse_backup(""));
        assert_eq!(None, parse_backup("{broken"));
        assert_eq!(None, parse_backup(&valid[..valid.len() - 2]));
        assert_eq!(
            None,
            parse_backup(&valid.replace("\"version\":1", "\"version\":2"))
        );
        assert_eq!(None, parse_backup(r#"{"version":1,"devices":[]}"#));

        // The key must belong to the identifier.
        let other = valid.replace(&device_key("AA:BB:CC:DD:EE:FF"), "000000000000");
        assert_eq!(None, parse_backup(&other));

        // Two devices with the same key.
        let one = serde_json::to_string(&device()).expect("device");
        let twice = format!(r#"{{"version":1,"devices":[{one},{one}]}}"#);
        assert_eq!(None, parse_backup(&twice));
    }

    #[test]
    fn broken_config_is_moved() {
        let dir =
            std::env::temp_dir().join(format!("uc-intg-appletv-test-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(CONFIG_FILE), b"{broken").expect("write");
        let store = ConfigStore::new(&dir);
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());
        assert!(!dir.join(CONFIG_FILE).exists());

        let bad: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.json.bad-")
            })
            .collect();
        assert_eq!(1, bad.len());
        assert_eq!(
            b"{broken".as_slice(),
            fs::read(bad[0].path()).expect("read")
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
