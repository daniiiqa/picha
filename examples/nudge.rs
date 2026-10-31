//! Nudges nearby Android phones and lists the Quick Share receivers that show up:
//! `cargo run --example nudge`
use std::time::{Duration, Instant};

use base64::Engine;
use mdns_sd::{ServiceDaemon, ServiceEvent};

#[tokio::main]
async fn main() {
    let nudge = picha::ble::nudge().await.expect("nudge");
    println!("nudging…");
    let mdns = ServiceDaemon::new().unwrap();
    let events = mdns.browse("_FC9F5ED42C8A._tcp.local.").unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while let Ok(ev) = events.recv_timeout(end.saturating_duration_since(Instant::now())) {
        if let ServiceEvent::ServiceResolved(i) = ev {
            let txt = i.get_property_val_str("n").unwrap_or_default();
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(txt).unwrap_or_default();
            println!("{:.1}s {} {:?} :{} first={:02x?} len={}", 20.0 - end.saturating_duration_since(Instant::now()).as_secs_f32(), i.get_fullname(), i.get_addresses_v4(), i.get_port(), raw.first(), raw.len());
        }
    }
    nudge.stop().await;
    let _ = mdns.shutdown();
}
