# meshtastic-serial-udp

[![codecov](https://codecov.io/gh/USA-RedDragon/meshtastic-serial-udp/graph/badge.svg?token=klhxi5s9gw)](https://codecov.io/gh/USA-RedDragon/meshtastic-serial-udp) [![License](https://badgen.net/github/license/USA-RedDragon/meshtastic-serial-udp)](https://github.com/USA-RedDragon/meshtastic-serial-udp/blob/main/LICENSE) [![Release](https://img.shields.io/github/release/USA-RedDragon/meshtastic-serial-udp.svg)](https://github.com/USA-RedDragon/meshtastic-serial-udp/releases/)

A lightweight Rust bridge that connects a USB-serial Meshtastic radio to the local network via UDP multicast. This enables applications like [Raven](https://github.com/KN6PLV/Raven) and other Meshtastic-over-LAN clients to interact with the radio without needing a LAN-capable Meshtastic device.

This was designed to be as lightweight as possible in order to fit on resource-constrained routers running OpenWRT, but it of course also works on any platform with Rust support and a serial connection to a Meshtastic radio.

## Installation

To integrate with Raven, you'll need Raven release 0.0.1 r7681671 or later. After installing Raven, you'll need to enable the Meshtastic bridge in Raven's configuration file (`/usr/local/raven/raven.conf`) by adding the following section:

```bash
"meshtastic": { "address": "127.0.0.1" }
```

This will bind Raven's multicast UDP listener to the loopback interface, allowing it to receive packets from this bridge running on the same machine.

After adding the above configuration, `service raven restart` to apply the changes.

We require `kmod-usb-acm` to be installed for the appropriate USB-serial drivers. You can install it with `opkg install kmod-usb-acm`.

Then, the relevant .ipk/.apk file for your device can be found on the [releases page](https://github.com/USA-RedDragon/meshtastic-serial-udp/releases/latest). After installation, this service should start right away. You can check its status with `service meshtastic-serial-udp status` and view logs with `logread -e meshtastic-serial-udp`. After plugging in and powering on your Meshtastic radio, this program should automatically detect it, perform the serial handshake, and start relaying packets between the radio and the local network.

## How it works

The bridge opens a serial connection to a Meshtastic radio and joins a UDP multicast group (default `224.0.0.69:4403`). It then performs the Meshtastic serial handshake to retrieve channel configuration (names, PSKs, channel hashes) from the radio.

Once running, it acts as a bidirectional relay:

- **Serial → UDP**: Packets received from the radio are decoded (protobuf `FromRadio` → `MeshPacket`), re-encrypted with the appropriate channel key, and sent to the multicast group as raw `MeshPacket` bytes.
- **UDP → Serial**: Packets received from multicast are decrypted, stamped with the `TransportMulticastUdp` origin marker, wrapped in a `ToRadio` protobuf frame, and written to the serial port.

Duplicate suppression prevents echo loops. Packets are tracked by ID so they aren't forwarded back to the direction they came from. A periodic heartbeat keeps the serial connection alive.

Encryption is handled transparently: the radio speaks decoded payloads over serial, while the UDP side uses standard Meshtastic AES-CTR encryption, so LAN clients see the same encrypted packets they'd receive over the air.

## Raven integration (OpenWrt)

When running with `--platform openwrt`, the bridge checks for a [Raven](https://github.com/KN6PLV/Raven) configuration file at `/usr/local/raven/raven.conf`. If found, it parses the channel list from the config and merges them with the channels already configured on the Meshtastic radio:

- **Duplicate channels** (matched by name) have their PSK updated to the Raven version.
- **New channels** from Raven are added as SECONDARY channels on the device.
- Channels are automatically installed on the Meshtastic device via admin commands during startup, so users don't need to manually pre-configure all channels on the radio.

If `raven.conf` is not present, the bridge logs a message and continues with the device's existing channels only.
