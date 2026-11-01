# picha

Your Android phone and your Linux laptop, like Huawei Share — from the terminal,
in the look of the [discada](https://github.com/daniiiqa/dotfiles) rice. *Yo te
picho*, like the cheve at a carne asada.

```
picha                  the menu: send · receive · phone screen · lumbre
picha foto.jpg …       send it to the phone
picha pantalla         the phone's screen here
```

- **Receive** what the phone shares: on the phone, **Share → Quick Share** (Wi-Fi,
  fast) or **Share → Bluetooth** (no network needed) and pick the laptop. A
  notification asks (Aceptar · Rechazar · Siempre); files go to Downloads, links
  open or copy, text goes to the clipboard. "Siempre" trusts that phone from then on.
- **Send** to the phone: picha nudges it over Bluetooth LE, then uses Quick Share at
  Wi-Fi speed when it answers, Bluetooth to the paired phone when not. The phone
  asks to accept.
- **The phone's screen** (scrcpy over Android's Wireless debugging or the cable):
  the clipboard goes both ways, drag files onto it to put them on the phone.
- **Lumbre**: when there's no network, the laptop makes its own Wi-Fi; the menu
  shows a QR the phone's camera joins with.
- **Hide** from the Recibir page, for networks you don't trust.

The menu (and every page) follows the look's `[style]` the way rola, vato and
fierro do — borders, bars, labels, case, selection, tint — and restyles when the
look changes. Without discada it uses sensible defaults and the terminal's colors.

## Install

```sh
cargo install --path . --root ~/.local
```

Needs: BlueZ with `bluez-obex` (Bluetooth), NetworkManager and dnsmasq (lumbre), and for the
phone's screen `scrcpy` and `android-tools`. Optional: `taco` (picks files for
sending), `wl-clipboard` (text to the clipboard).

The receiver runs in the background:

```sh
picha recibe                     # e.g. exec-once in Hyprland
# or: install -Dm644 dist/picha-recibe.service ~/.config/systemd/user/ && systemctl --user enable --now picha-recibe
```

### The phone, once

- **Bluetooth**: pair it with the laptop (for sending, and receiving without a network).
- **Phone screen**: Developer options → Wireless debugging → *Pair device with
  pairing code*, then `picha pantalla --empareja`. Or plug in the cable: picha
  turns Wireless debugging back on through it when Samsung switches it off.

## Config

`~/.config/picha/config.json` (all optional):

```json
{ "name": "hachikuji", "dir": "~/Downloads", "lang": "es", "trusted": ["Daniel's S21"] }
```

## How it works

Receiving speaks Google's Nearby Share as the server: an mDNS service Android's
share sheet looks for, a UKEY2 key exchange (P-256), an AES-256-CBC + HMAC-SHA256
channel and the Nearby Connections / Sharing frames inside it. Sending is the same
as the client, after the BLE advertisement Android waits for. Bluetooth transfers
go through BlueZ's obexd; the phone's screen is scrcpy.

Built from [NearDrop](https://github.com/grishka/NearDrop)'s protocol write-up
(thank you, @grishka) and Google's open-source
[Nearby Connections](https://github.com/google/nearby); the protobuf definitions in
`proto/` are Google's (Apache 2.0). Notes on Quick Share without any network:
[docs/quick-share-sin-red.md](docs/quick-share-sin-red.md).

Tested with a Galaxy S21 (Android 15, One UI 7).

```sh
cargo test                        # crypto, framing and whole transfers over TCP
cargo run --example browse        # Quick Share devices on the network
cargo run --example nudge         # nudge phones and watch who answers
```

## License

MIT
