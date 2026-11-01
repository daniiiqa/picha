//! Which network a Quick Share connection came in on. Over Wi-Fi a phone is
//! only the name it announces, and anyone on the same network can announce
//! any name; so "Always" holds for that phone on the network where it was
//! given (NetworkManager's saved connection, by its UUID), not everywhere.

use std::net::{IpAddr, SocketAddr};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// The connection's name (usually the Wi-Fi's), for showing.
    pub name: String,
    pub uuid: String,
}

/// The network of the interface `local` (our end of the connection) is on;
/// None without NetworkManager or when it isn't one of its connections.
pub fn of(local: SocketAddr) -> Option<Network> {
    let dev = interface(local)?;
    let out = Command::new("nmcli").args(["-t", "-f", "NAME,UUID,DEVICE", "connection", "show", "--active"]).output().ok()?;
    active(&String::from_utf8_lossy(&out.stdout), &dev)
}

fn interface(local: SocketAddr) -> Option<String> {
    // link-local IPv6 (lumbre) carries its interface
    if let SocketAddr::V6(v6) = local
        && v6.scope_id() != 0
    {
        return by_index(v6.scope_id());
    }
    let ip = match local.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        ip => ip,
    };
    let out = Command::new("ip").args(["-j", "addr", "show"]).output().ok()?;
    owner(&String::from_utf8_lossy(&out.stdout), ip)
}

fn by_index(index: u32) -> Option<String> {
    std::fs::read_dir("/sys/class/net").ok()?.flatten().find_map(|e| {
        let i: u32 = std::fs::read_to_string(e.path().join("ifindex")).ok()?.trim().parse().ok()?;
        (i == index).then(|| e.file_name().to_string_lossy().into_owned())
    })
}

/// The interface with `ip` among `ip -j addr`'s.
fn owner(json: &str, ip: IpAddr) -> Option<String> {
    let links: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    links.iter().find_map(|l| {
        let has = l["addr_info"].as_array()?.iter().any(|a| a["local"].as_str().and_then(|s| s.parse::<IpAddr>().ok()) == Some(ip));
        has.then(|| l["ifname"].as_str().map(String::from)).flatten()
    })
}

/// The connection on `dev` in `nmcli -t -f NAME,UUID,DEVICE connection show --active`.
fn active(out: &str, dev: &str) -> Option<Network> {
    out.lines().find_map(|line| match fields(line).as_slice() {
        [name, uuid, d] if d == dev && !uuid.is_empty() => Some(Network { name: name.clone(), uuid: uuid.clone() }),
        _ => None,
    })
}

/// nmcli -t's fields: split on `:`, with `\:` and `\\` escaped.
fn fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.last_mut().unwrap().extend(chars.next()),
            ':' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_conexion_del_dispositivo() {
        let out = "lo:6f1c…:lo\nTotalplay\\:5G:3b7e-11aa:wlan0\nlumbre:9d2c-44bb:wlan1\n";
        assert_eq!(active(out, "wlan0"), Some(Network { name: "Totalplay:5G".into(), uuid: "3b7e-11aa".into() }));
        assert_eq!(active(out, "eth0"), None);
    }

    #[test]
    fn la_interfaz_de_la_ip() {
        let json = r#"[{"ifname":"lo","addr_info":[{"local":"127.0.0.1"}]},
                       {"ifname":"wlan0","addr_info":[{"local":"192.168.1.40"},{"local":"fe80::1"}]}]"#;
        assert_eq!(owner(json, "192.168.1.40".parse().unwrap()).as_deref(), Some("wlan0"));
        assert_eq!(owner(json, "10.0.0.2".parse().unwrap()), None);
    }
}
