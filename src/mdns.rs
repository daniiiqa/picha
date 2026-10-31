//! Being visible: the mDNS service Android's share sheet looks for on the local network.

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

use crate::error::{protocol, Result};
use crate::wire::{self, Endpoint};

const SERVICE: &str = "_FC9F5ED42C8A._tcp.local.";

/// Advertises while alive.
pub struct Advert {
    daemon: ServiceDaemon,
    fullname: String,
}

pub fn advertise(me: &Endpoint, host: &str, port: u16) -> Result<Advert> {
    let daemon = ServiceDaemon::new().map_err(|e| protocol(format!("mDNS: {e}")))?;
    // A phone given 127.0.0.1 would dial itself.
    daemon
        .disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])
        .map_err(|e| protocol(format!("mDNS: {e}")))?;
    let txt = wire::mdns_txt(me);
    let info = ServiceInfo::new(
        SERVICE,
        &wire::mdns_name(&wire::endpoint_id()),
        &format!("{host}.local."),
        "",
        port,
        &[("n", txt.as_str())][..],
    )
    .map_err(|e| protocol(format!("mDNS: {e}")))?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).map_err(|e| protocol(format!("mDNS: {e}")))?;
    Ok(Advert { daemon, fullname })
}

impl Drop for Advert {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
