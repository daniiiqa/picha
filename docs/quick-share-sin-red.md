# Quick Share without a network (research notes)

How Quick Share reaches a receiver when phone and laptop share no Wi-Fi, from
Google's open-source Nearby Connections (github.com/google/nearby, Apache 2.0)
and a Galaxy S21's logcat (Android 15, One UI 7, Samsung Quick Share 13.8).

## 1. The phone finds the receiver over BLE

The S21's log while sharing: `Started BLE scanning, service-id=NearbySharing,
is-extended-advert=true` and `Paused bluetooth discovery` — it looks for
receivers in BLE advertisements, not classic Bluetooth names.

`connections/implementation/ble_advertisement.cc`, the advertisement body:

| bytes | what |
|---|---|
| 1 | version (upper 3 bits) and PCP (lower 5) |
| 3 | service id hash (`fc 9f 5e`, as in the mDNS name) |
| 4 | endpoint id |
| 1 + n | endpoint info, length-prefixed (the same bytes as the mDNS `n` record) |
| 6 | the receiver's Bluetooth MAC, for the classic connection |
| … | optional UWB address |

To check against `mediums/ble/` (BLE v2): whether the S21 expects this whole
body in the extended advertisement or a short header plus a GATT read.

## 2. It connects over classic Bluetooth (RFCOMM)

`mediums/bluetooth/bluetooth_classic.cc`: the receiver listens for RFCOMM on
`Uuid("NearbySharing")` — `internal/platform/uuid.cc`: MD5 of the string, then
version 3 and the IETF variant set (a name-based UUID with no namespace).

Classic discovery, when used, renames the adapter to a base64 blob
(`bluetooth_device_name.cc`): version|PCP, endpoint id, service id hash, a flags
byte, reserved bytes, length-prefixed endpoint info.

Over that socket: presumably the same length-prefixed offline frames picha
already speaks over TCP (UKEY2, SecureMessage, payloads).

## 3. Speed: the bandwidth upgrade

Bluetooth alone is OBEX-slow. The receiver (server) may ask to upgrade: it sends
a bandwidth-upgrade frame with a medium and credentials, the phone joins, and the
transfer moves there. With `WIFI_HOTSPOT` the receiver opens a hotspot and sends
SSID, password and port — what `picha lumbre` will already do.

## This laptop

- Bluetooth: BlueZ 5.87, 12 advertising instances, extended advertising
  (secondary channels 1M/2M/Coded, 251-byte advertisements).
- Wi-Fi (iwlwifi): `#{ managed } <= 1, #{ AP, P2P-client, P2P-GO } <= 1,
  #channels <= 1` — client and hotspot at once on one channel; Wi-Fi Direct
  group owner too.

## Plan

1. **Be seen:** BLE advertisement via BlueZ's LEAdvertisingManager1. Pass: the
   S21 lists the laptop in Quick Share with Wi-Fi off.
2. **Be reached:** RFCOMM server on `Uuid("NearbySharing")` via ProfileManager1,
   feeding picha's existing receive path. Pass: a photo arrives over Bluetooth.
3. **Be fast:** answer with a WIFI_HOTSPOT upgrade through `lumbre`. Pass: a
   video arrives at Wi-Fi speed with no network around.
