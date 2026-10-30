# meshtastic-serial-udp

![Coverage](https://raw.githubusercontent.com/USA-RedDragon/meshtastic-serial-udp/main/.github/badges/coverage.svg) [![License](https://badgen.net/github/license/USA-RedDragon/meshtastic-serial-udp)](https://github.com/USA-RedDragon/meshtastic-serial-udp/blob/main/LICENSE) [![Release](https://img.shields.io/github/release/USA-RedDragon/meshtastic-serial-udp.svg)](https://github.com/USA-RedDragon/meshtastic-serial-udp/releases/)

A lightweight Rust bridge that connects a USB-serial Meshtastic radio to the local network via UDP multicast. This enables applications like [Raven](https://github.com/KN6PLV/Raven) and other Meshtastic-over-LAN clients to interact with the radio without needing a LAN-capable Meshtastic device.

This was designed to be as lightweight as possible in order to fit on resource-constrained routers running OpenWRT, but it of course also works on any platform with Rust support and a serial connection to a Meshtastic radio.

## Installation

To integrate with Raven, you'll need Raven release 0.0.1 r7681671 or later. After installing Raven, you'll need to enable the Meshtastic bridge in Raven's configuration file (`/usr/local/raven/raven.conf`) by adding the following section:

```bash
"meshtastic": { "address": "127.0.0.1" }
```

This will bind Raven's multicast UDP listener to the loopback interface, allowing it to receive packets from this bridge running on the same machine.

After adding the above configuration, `service raven restart` to apply the changes.

The package depends on the USB-serial drivers for common Meshtastic radios: `kmod-usb-acm` (native USB, `/dev/ttyACM*`), and `kmod-usb-serial-cp210x` and `kmod-usb-serial-ch341` (USB-to-serial chips, `/dev/ttyUSB*`). The package manager installs them with the package.

Then, the relevant .ipk/.apk file for your device can be found on the [releases page](https://github.com/USA-RedDragon/meshtastic-serial-udp/releases/latest). After installation, this service should start right away. You can check its status with `service meshtastic-serial-udp status` and view logs with `logread -e meshtastic-serial-udp`. After plugging in and powering on your Meshtastic radio, this program finds it on its own (see [Choosing the serial port](#choosing-the-serial-port)), performs the serial handshake, and starts relaying packets between the radio and the local network.

## Choosing the serial port

By default (`--port auto`, or `PORT=auto` in `/etc/meshtastic-serial-udp.conf`) the bridge looks for the radio itself. It tries these serial ports in order:

1. Each link in `/dev/serial/by-id/`, where the system provides that directory.
2. `/dev/ttyACM0`, `/dev/ttyACM1`, and so on.
3. `/dev/ttyUSB0`, `/dev/ttyUSB1`, and so on.

A port that is reached through more than one of these paths is tried once. A port that another program already has open is skipped and not opened. On each other port, the bridge sends the Meshtastic `want_config` request, the same request it sends to a known radio, and nothing else. If no Meshtastic reply comes back within about 6 seconds, it closes the port and tries the next one. It uses the first port that completes the handshake and logs the reason it skipped each of the others. If no port answers, it logs an error and tries again, waiting longer each time up to 30 seconds. After the radio disconnects or reboots, the bridge looks again, so the radio can come back on a different port.

To always use one port, give its path. On OpenWrt, set `PORT` in `/etc/meshtastic-serial-udp.conf` and run `service meshtastic-serial-udp restart`. On other systems, pass `--port`. A `/dev/serial/by-id/` path stays the same when other USB devices are plugged in or the order changes, so use one if your system has them:

```bash
PORT=/dev/serial/by-id/usb-RAKwireless_WisCore_RAK4631_Board_0123456789ABCDEF-if00
```

Set a fixed port if other serial devices are connected and you do not want the bridge to send them the probe request. Opening a serial port can reset some boards, for example ESP32 and Arduino boards with a USB-to-serial chip.

Radios with a USB-to-serial chip other than CP210x or CH340 show up as `/dev/ttyUSB*` only after you install the kernel driver for that chip.

### Upgrading from an older version

Older versions used `PORT=/dev/ttyACM0` by default. The first time this version is installed or upgraded, a config that still has `PORT=/dev/ttyACM0` is changed to `PORT=auto`. Any other `PORT` value is kept. This change happens once: if you need `/dev/ttyACM0` and nothing else, set `PORT=/dev/ttyACM0` again and later upgrades keep it.

## How it works

The bridge opens a serial connection to a Meshtastic radio and joins a UDP multicast group (default `224.0.0.69:4403`). It then performs the Meshtastic serial handshake to retrieve channel configuration (names, PSKs, channel hashes) from the radio.

Once running, it acts as a bidirectional relay:

- **Serial → UDP**: Packets received from the radio are decoded (protobuf `FromRadio` → `MeshPacket`), re-encrypted with the appropriate channel key, and sent to the multicast group as raw `MeshPacket` bytes.
- **UDP → Serial**: Encrypted packets received from multicast are handed to the radio through its MQTT client proxy (`ToRadio.mqttClientProxyMessage`), exactly as received. The radio treats them like packets downlinked from an MQTT broker: they keep their original sender, packet ID and hop count, and the firmware's own duplicate suppression and flooding rules decide whether to rebroadcast them over LoRa.

Duplicate suppression prevents echo loops. Packets are tracked by sender and ID so they aren't forwarded back to the direction they came from. A periodic heartbeat keeps the serial connection alive.

Encryption is handled transparently: the radio speaks decoded payloads over serial, while the UDP side uses standard Meshtastic AES-CTR encryption and channel hashes, so LAN clients see the same encrypted packets they'd receive over the air.

### Radio setup

The radio needs a LoRa region set before it will transmit or receive anything; a factory-fresh radio has none, and the bridge won't pick one for you (it logs an error if it's unset). Set it once with the Meshtastic app or CLI, e.g. `meshtastic --set lora.region US`.

### Radio settings the bridge changes

For the MQTT client proxy path to work, the bridge configures the radio on first start:

- **MQTT** is enabled with **Proxy to client** turned on. If MQTT was off and no broker address was set, the address is set to `127.0.0.1`, so a phone app connected to the radio (which would otherwise act as the proxy) can't connect to the public broker. Changing the MQTT settings reboots the radio once; the bridge waits for it and reconnects.
- **Downlink** is enabled on every channel.

Nodes that have **Ignore MQTT** set will ignore packets bridged from the LAN, since they are marked as having come via MQTT.

If the radio's firmware is built without MQTT support (e.g. RAK11200 and STM32 builds), the bridge falls back to `--ingress phone`. In that mode packets are sent as ordinary `ToRadio` packets, which the firmware re-sends as the radio's *own* messages (the original sender is lost) and rate-limits like a phone app: one text message per 2 seconds, one position/telemetry/waypoint per 10 seconds per port, one traceroute per 30 seconds.

### Reconnects

If the radio reboots (it announces this over serial) the bridge redoes the handshake; if the serial device disappears (as USB-CDC radios do when they reboot) the bridge reopens it once it's back.

## Raven integration (OpenWrt)

When running with `--platform openwrt`, the bridge checks for a [Raven](https://github.com/KN6PLV/Raven) configuration file at `/usr/local/raven/raven.conf`. If found, it parses the channel list from the config and merges them with the channels already configured on the Meshtastic radio:

- **Duplicate channels** (matched by name) have their PSK updated to the Raven version.
- **New channels** from Raven are added as SECONDARY channels on the device.
- Channels are automatically installed on the Meshtastic device via admin commands during startup, so users don't need to manually pre-configure all channels on the radio.

If `raven.conf` is not present, the bridge logs a message and continues with the device's existing channels only.
