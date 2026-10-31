//! What Android's Wireless debugging advertises: `cargo run --example adbscan`
use std::time::{Duration, Instant};
use mdns_sd::{ServiceDaemon, ServiceEvent};

fn main() {
    let mdns = ServiceDaemon::new().unwrap();
    let a = mdns.browse("_adb-tls-connect._tcp.local.").unwrap();
    let b = mdns.browse("_adb-tls-pairing._tcp.local.").unwrap();
    let c = mdns.browse("_adb._tcp.local.").unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        for (n, rx) in [("connect", &a), ("pairing", &b), ("adb", &c)] {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ServiceEvent::ServiceResolved(i) => println!("{n}: {} {:?} :{}", i.get_fullname(), i.get_addresses(), i.get_port()),
                    ServiceEvent::ServiceFound(t, f) => println!("{n}: found {f} ({t})"),
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = mdns.shutdown();
}
