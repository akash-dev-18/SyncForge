//! mDNS discovery test: advertise on this host and find the service again.
//!
//! Uses real multicast; needs a non-loopback interface with multicast support
//! (the normal case in CI containers).

use std::time::Duration;

use syncengine_protocol::DeviceId;
use syncengine_transport::discovery::{Advertiser, find_peer};

#[test]
fn advertised_devices_are_discovered() {
    let device_id = DeviceId::new();
    let port = 41555;
    let _advertiser =
        Advertiser::start(&device_id, "discovery-test", port).expect("mDNS advertisement starts");

    let candidates = find_peer(&device_id, Duration::from_secs(10)).expect("browse works");
    assert!(
        !candidates.is_empty(),
        "the advertised device is discovered within 10s"
    );
    assert!(candidates.iter().all(|addr| addr.port() == port));
    assert!(candidates.iter().all(|addr| !addr.ip().is_unspecified()));
    let first = candidates.first().expect("non-empty");
    assert!(
        !first.ip().is_ipv6() || candidates.iter().all(|addr| addr.ip().is_ipv6()),
        "IPv4 candidates are preferred"
    );
}
