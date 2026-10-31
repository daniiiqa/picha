# picha

Quick Share for Linux, from the terminal: share from your Android phone and the
file lands on your laptop — no app on the phone. *Yo te picho*, like the cheve
at a carne asada.

```
$ picha
picha: visible as "hachikuji" (:34431) · saving to ~/Downloads
picha: S21 de Dany te picha foto.jpg (2.1 MB · PIN 4821)
picha: accepted
picha: 100% (2.1 MB)
picha: Received foto.jpg → ~/Downloads
```

On the phone: **Share → Quick Share** and pick your laptop. picha asks through a
desktop notification (Accept / Reject) showing the same PIN as the phone. Links
open or copy, text goes to the clipboard.

## Status

- ✅ Receiving files, links and text over Wi-Fi (phone and laptop on the same network),
  tested with a Galaxy S21 on One UI 7
- ✅ Receiving over Bluetooth with no network at all: Share → Bluetooth from a paired
  phone (needs `bluez-obex`; slower, fine for photos and documents)
- ✅ "Always from this phone": trusted phones skip the prompt (`picha confianza`, `picha olvida`)
- ⏳ Sending to the phone (the protocol side exists; Android only listens after a
  Bluetooth LE nudge, which is next)

## Install

```sh
cargo install --path . --root ~/.local
picha            # stays visible while it runs
```

`~/.config/picha/config.json` (all optional):

```json
{ "name": "hachikuji", "dir": "~/Downloads", "lang": "es" }
```

## How it works

picha advertises the `_FC9F5ED42C8A._tcp` mDNS service Android's share sheet looks
for, then speaks Nearby Share: a UKEY2 key exchange (P-256), an AES-256-CBC +
HMAC-SHA256 channel, and the Nearby Connections / Sharing frames inside it.

Built from [NearDrop](https://github.com/grishka/NearDrop)'s protocol write-up
(thank you, @grishka). The protobuf definitions in `proto/` are Google's (Apache 2.0).

```sh
cargo test                        # crypto, framing, and full transfers over TCP
cargo run --example browse        # list Quick Share devices on the network
```

## License

MIT
