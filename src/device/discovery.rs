// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Find Apple TV devices and AirPlay outputs on the network.

use crate::config_store::DeviceConfig;
use log::{debug, info};
use ratv::discovery::{Address, Device, MDNS_PORT, ScanOptions, ServiceKind, scan, scan_until};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Time for a multicast scan.
const SCAN_TIMEOUT: Duration = Duration::from_secs(3);
/// Time for a unicast scan to a known address.
const UNICAST_TIMEOUT: Duration = Duration::from_secs(2);

/// The identifiers of a discovered device: AirPlay, MRP and Companion.
pub fn device_ids(device: &Device) -> impl Iterator<Item = &str> {
    [
        device
            .service(ServiceKind::AirPlay)
            .and_then(|s| s.txt.get("deviceid")),
        device
            .service(ServiceKind::MediaRemote)
            .and_then(|s| s.txt.get("UniqueIdentifier")),
        device
            .service(ServiceKind::Companion)
            .and_then(|s| s.txt.get("rpMRtID")),
    ]
    .into_iter()
    .flatten()
}

/// Returns `true` for an Apple TV. HomePod devices also report tvOS, so the model is checked.
pub fn is_apple_tv(device: &Device) -> bool {
    device.model().is_some_and(is_supported_model)
}

/// Apple TV HD (`AppleTV5,3`) and newer. Older models have no tvOS.
fn is_supported_model(model: &str) -> bool {
    model
        .strip_prefix("AppleTV")
        .and_then(|v| v.split(',').next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 5)
}

/// Scan for Apple TV devices. With an address, that host is asked first.
///
/// If the host does not answer, a multicast scan is used and only devices with that address
/// are kept. Some networks forward mDNS multicast between VLANs but block unicast UDP.
pub async fn scan_apple_tvs(address: Option<IpAddr>) -> Result<Vec<Device>, ratv::Error> {
    let mut options = ScanOptions::default();
    options.timeout = SCAN_TIMEOUT;
    let Some(ip) = address else {
        let mut devices = scan(&options).await?;
        devices.retain(is_apple_tv);
        return Ok(devices);
    };

    let mut unicast = options.clone();
    unicast.targets = vec![SocketAddr::new(ip, MDNS_PORT)];
    let mut devices = scan_address(&unicast, ip).await?;
    if devices.is_empty() {
        info!("no answer from {ip}, scanning the network");
        let has_ip = |d: &Device| d.addresses.iter().any(|a| a.ip == ip);
        devices = scan_until(&options, |d| has_ip(d) && has_endpoints(d)).await?;
        devices.retain(has_ip);
    }
    devices.retain(is_apple_tv);
    Ok(devices)
}

/// Returns `true` for errors that come while the network of the Remote starts, for example after wake-up.
pub fn network_not_ready(e: &ratv::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e,
        ratv::Error::Io(io) if matches!(
            io.kind(),
            ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable | ErrorKind::NetworkDown
        )
    )
}

/// Ask one address. An error that is not [`network_not_ready`] gives an empty list, so the
/// caller scans the network. The scan socket cannot send to some addresses, for example IPv6.
async fn scan_address(options: &ScanOptions, ip: IpAddr) -> Result<Vec<Device>, ratv::Error> {
    match scan(options).await {
        Err(e) if !network_not_ready(&e) => {
            debug!("scan of {ip} failed: {e}");
            Ok(Vec::new())
        }
        r => r,
    }
}

/// Find a configured Apple TV.
///
/// Asks the last known address first, then the whole network unless the address was entered
/// by the user and the device answered there.
pub async fn find(cfg: &DeviceConfig) -> Result<Option<Device>, ratv::Error> {
    if let Some(ip) = cfg.address {
        let mut options = ScanOptions::default();
        options.timeout = UNICAST_TIMEOUT;
        options.targets = vec![SocketAddr::new(ip, MDNS_PORT)];
        if let Some(device) = scan_address(&options, ip)
            .await?
            .into_iter()
            .find(|d| cfg.matches(device_ids(d)))
        {
            debug!("[{}] found at {ip}", cfg.name);
            return Ok(Some(device));
        }
        info!("[{}] not found at {ip}, scanning the network", cfg.name);
    }

    let mut options = ScanOptions::default();
    options.timeout = SCAN_TIMEOUT;
    let found = |d: &Device| cfg.matches(device_ids(d));
    Ok(scan_until(&options, |d| found(d) && has_endpoints(d))
        .await?
        .into_iter()
        .find(found))
}

/// Returns `true` if the Companion and AirPlay services have answered. The connect uses both.
fn has_endpoints(device: &Device) -> bool {
    device.service(ServiceKind::Companion).is_some()
        && device.service(ServiceKind::AirPlay).is_some()
}

/// Update the identifiers, name and address of a configuration from a discovered device.
///
/// The address changes only if the device does not have the stored address anymore. This is
/// also done for an address entered by the user: the device matched by its identifiers.
///
/// Returns `true` if something changed.
pub fn update_config(cfg: &mut DeviceConfig, device: &Device) -> bool {
    let mut changed = false;
    for id in merge_ids(&mut cfg.known_ids, device_ids(device)) {
        info!("[{}] new identifier: {id}", cfg.name);
        changed = true;
    }
    let name = device.name();
    if !name.is_empty() && name != cfg.name {
        info!("[{}] name changed to {name}", cfg.name);
        cfg.name = name.to_string();
        changed = true;
    }
    if let Some(ip) = new_address(cfg.address, &device.addresses) {
        info!("[{}] address changed to {ip}", cfg.name);
        cfg.address = Some(ip);
        changed = true;
    }
    changed
}

/// The address to store, if the device does not have the stored one anymore.
fn new_address(stored: Option<IpAddr>, found: &[Address]) -> Option<IpAddr> {
    let first = found.first()?.ip;
    (!stored.is_some_and(|a| found.iter().any(|f| f.ip == a))).then_some(first)
}

/// Add the ids that are not known yet and return them. Known ids are kept: a scan may not see
/// all services.
fn merge_ids<'a>(
    known: &mut Vec<String>,
    ids: impl Iterator<Item = &'a str>,
) -> impl Iterator<Item = &'a str> {
    ids.filter(|id| {
        if known.iter().any(|k| k == id) {
            return false;
        }
        known.push((*id).to_string());
        true
    })
}

/// Returns `true` if both lists have the same ids, in any order.
pub fn same_ids(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|id| b.contains(id)) && b.iter().all(|id| a.contains(id))
}

/// Scan for AirPlay devices that can join the group of an Apple TV: (output uid, name).
///
/// `own_uid` is the output uid of the Apple TV. It is left out.
pub async fn scan_airplay_outputs(
    own_uid: Option<&str>,
) -> Result<Vec<(String, String)>, ratv::Error> {
    let mut options = ScanOptions::default();
    options.timeout = SCAN_TIMEOUT;
    options.services = vec![ServiceKind::AirPlay];
    let devices = scan(&options).await?;
    Ok(devices
        .iter()
        .filter_map(|d| {
            let uid = d.output_uid()?;
            if own_uid.is_some_and(|o| o.eq_ignore_ascii_case(uid)) {
                return None;
            }
            Some((uid.to_string(), d.name().to_string()))
        })
        .collect())
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
    fn merge_keeps_known_ids() {
        let mut known = vec!["a".to_string(), "b".to_string()];
        assert_eq!(0, merge_ids(&mut known, ["b"].into_iter()).count());
        assert_eq!(
            vec!["c"],
            merge_ids(&mut known, ["c", "a"].into_iter()).collect::<Vec<_>>()
        );
        assert_eq!(vec!["a", "b", "c"], known);
    }

    #[test]
    fn address_changes_only_when_gone() {
        let a: IpAddr = "192.0.2.6".parse().expect("ip");
        let b: IpAddr = "192.0.2.7".parse().expect("ip");
        let v6: IpAddr = "fe80::1".parse().expect("ip");
        let found = |ips: &[IpAddr]| ips.iter().map(|&ip| Address::from(ip)).collect::<Vec<_>>();
        assert_eq!(None, new_address(Some(a), &found(&[b, a])));
        assert_eq!(None, new_address(Some(a), &[]));
        assert_eq!(Some(b), new_address(Some(a), &found(&[b, v6])));
        assert_eq!(Some(b), new_address(None, &found(&[b])));
    }

    #[test]
    fn same_ids_in_any_order() {
        let a = ["x".to_string(), "y".to_string()];
        let b = ["y".to_string(), "x".to_string()];
        assert!(same_ids(&a, &b));
        assert!(!same_ids(&a, &b[..1]));
    }

    #[test]
    fn supported_models() {
        for model in [
            "AppleTV5,3",
            "AppleTV6,2",
            "AppleTV11,1",
            "AppleTV14,1",
            "AppleTV18,1",
        ] {
            assert!(is_supported_model(model), "{model}");
        }
        for model in [
            "AppleTV3,2",
            "AppleTV2,1",
            "AudioAccessory5,1",
            "AppleTV",
            "AppleTVx,1",
        ] {
            assert!(!is_supported_model(model), "{model}");
        }
    }
}
