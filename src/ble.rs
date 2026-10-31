//! The Bluetooth LE "nudge": Android only listens for Quick Share senders (and
//! shows itself on the network) after it hears this advertisement, the one its own
//! share sheet sends while looking for receivers. Service UUID FE2C, service data
//! `fc 12 8e 01 42 00…00` + 10 random bytes (NearDrop's PROTOCOL.md).

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};
use zbus::{interface, Connection, Proxy};

use crate::crypto::random_bytes;
use crate::error::{protocol, Result};

const PATH: &str = "/picha/nudge";

struct Advert {
    data: Vec<u8>,
}

#[interface(name = "org.bluez.LEAdvertisement1")]
impl Advert {
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "broadcast".into()
    }

    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec!["fe2c".into()]
    }

    #[zbus(property, name = "ServiceData")]
    fn service_data(&self) -> HashMap<String, Value<'_>> {
        HashMap::from([("fe2c".to_string(), Value::from(self.data.clone()))])
    }

    fn release(&self) {}
}

fn payload() -> Vec<u8> {
    let mut data = vec![0xfc, 0x12, 0x8e, 0x01, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    data.extend(random_bytes::<10>());
    data
}

/// Nudges while alive.
pub struct Nudge {
    conn: Connection,
    adapter: String,
}

/// Starts advertising on the first adapter.
pub async fn nudge() -> Result<Nudge> {
    let err = |e: zbus::Error| protocol(format!("Bluetooth LE: {e}"));
    let conn = Connection::system().await.map_err(err)?;
    let adapter = "/org/bluez/hci0".to_string();
    conn.object_server().at(PATH, Advert { data: payload() }).await.map_err(err)?;
    let manager = Proxy::new(&conn, "org.bluez", adapter.clone(), "org.bluez.LEAdvertisingManager1").await.map_err(err)?;
    let path = OwnedObjectPath::from(ObjectPath::try_from(PATH).expect("valid path"));
    manager.call::<_, _, ()>("RegisterAdvertisement", &(path, HashMap::<String, Value>::new())).await.map_err(err)?;
    Ok(Nudge { conn, adapter })
}

impl Nudge {
    pub async fn stop(self) {
        if let Ok(manager) = Proxy::new(&self.conn, "org.bluez", self.adapter.clone(), "org.bluez.LEAdvertisingManager1").await {
            let path = OwnedObjectPath::from(ObjectPath::try_from(PATH).expect("valid path"));
            let _ = manager.call::<_, _, ()>("UnregisterAdvertisement", &(path,)).await;
        }
        let _ = self.conn.object_server().remove::<Advert, _>(PATH).await;
    }
}
