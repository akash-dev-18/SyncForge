//! mDNS discovery of paired devices on the local network (SRD §4.2).
//!
//! Each device advertises `_syncengine._tcp.local.` with its device ID as a
//! TXT property and its TCP port. Discovery only *finds* candidates; the
//! handshake plus the trusted-peer check decide whether a session may run.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use syncengine_protocol::DeviceId;

use crate::error::TransportError;

/// The mDNS service type advertised by every SyncEngine device.
pub const SERVICE_TYPE: &str = "_syncengine._tcp.local.";

/// Keeps one mDNS advertisement alive; unregisters on drop.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Starts advertising this device under `device_name` on `port`.
    pub fn start(
        device_id: &DeviceId,
        device_name: &str,
        port: u16,
    ) -> Result<Self, TransportError> {
        let daemon =
            ServiceDaemon::new().map_err(|error| TransportError::Discovery(error.to_string()))?;
        let sanitized: String = device_name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let instance = if sanitized.is_empty() {
            format!("syncengine-{}", short_id(device_id))
        } else {
            format!("{}-{}", sanitized, short_id(device_id))
        };
        let host = format!("{}.local.", instance.to_ascii_lowercase());
        let properties = [("device_id", device_id.to_string())];
        let service = ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, &properties[..])
            .map_err(|error| TransportError::Discovery(error.to_string()))?
            .enable_addr_auto();
        let fullname = service.get_fullname().to_string();
        daemon
            .register(service)
            .map_err(|error| TransportError::Discovery(error.to_string()))?;
        tracing::debug!(port, %instance, "advertising over mdns");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.unregister(&self.fullname) {
            tracing::debug!(%error, "failed to unregister mdns advertisement");
        }
    }
}

/// Browses for one specific device and returns every reachable address of
/// its advertisement, best candidates first: non-loopback IPv4, then any
/// IPv4, then the rest. Multi-homed hosts (docker bridges, VPNs, IPv6-only
/// interfaces) can resolve to addresses a given network path rejects, so
/// callers should dial them in order until one connects.
///
/// This is a blocking call (mdns-sd delivers events on its own threads); call
/// it from `spawn_blocking` in async contexts. Returns an empty list when the
/// device does not answer within `timeout`.
pub fn find_peer(
    device_id: &DeviceId,
    timeout: Duration,
) -> Result<Vec<SocketAddr>, TransportError> {
    let daemon =
        ServiceDaemon::new().map_err(|error| TransportError::Discovery(error.to_string()))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|error| TransportError::Discovery(error.to_string()))?;
    let wanted = device_id.to_string();
    let deadline = Instant::now() + timeout;
    let mut found = Vec::new();
    let mut fallback: Vec<SocketAddr> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if info.get_property_val_str("device_id") != Some(wanted.as_str()) {
                    continue;
                }
                let port = info.get_port();
                let mut candidates: Vec<SocketAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|ip| SocketAddr::new(ip.to_ip_addr(), port))
                    .collect();
                candidates.retain(|addr| !addr.ip().is_unspecified());
                candidates.sort_by_key(|addr| match addr.ip() {
                    std::net::IpAddr::V4(ip) if !ip.is_loopback() => 0,
                    std::net::IpAddr::V4(_) => 1,
                    std::net::IpAddr::V6(_) => 2,
                });
                candidates.dedup();
                if candidates.is_empty() {
                    continue;
                }
                if candidates.iter().any(|addr| addr.is_ipv4()) {
                    found = candidates;
                    break;
                }
                // A bare link-local IPv6 address cannot be dialed without a
                // scope; keep browsing in case the A records arrive too.
                if fallback.is_empty() {
                    fallback = candidates;
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(if found.is_empty() { fallback } else { found })
}

fn short_id(device_id: &DeviceId) -> String {
    let id = device_id.to_string();
    id.chars().take(8).collect()
}
