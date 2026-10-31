//! Lists Quick Share receivers on the network, like a phone's share sheet does:
//! `cargo run --example browse`
use std::time::{Duration, Instant};

use base64::Engine;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use picha::wire::Endpoint;

fn main() {
    let mdns = ServiceDaemon::new().expect("mDNS");
    let events = mdns.browse("_FC9F5ED42C8A._tcp.local.").expect("browse");
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let ServiceEvent::ServiceResolved(info) = event {
            let txt = info.get_property_val_str("n").unwrap_or_default();
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(txt).unwrap_or_default();
            println!("{} → {:?} :{} {:?}", info.get_fullname(), info.get_addresses_v4(), info.get_port(), Endpoint::decode(&raw));
        }
    }
    let _ = mdns.shutdown();
}
