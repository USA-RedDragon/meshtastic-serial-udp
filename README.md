# meshtastic-serial-udp

A lightweight Rust bridge that connects a USB-serial Meshtastic radio to the local network via UDP multicast. This enables applications like [Raven](https://github.com/KN6PLV/Raven) and other Meshtastic-over-LAN clients to interact with the radio without needing a LAN-capable Meshtastic device.

This was designed to be as lightweight as possible in order to fit on resource-constrained routers running OpenWRT, but it of course also works on any platform with Rust support and a serial connection to a Meshtastic radio.

## How it works

The bridge opens a serial connection to a Meshtastic radio and joins a UDP multicast group (default `224.0.0.69:4403`). It then performs the Meshtastic serial handshake to retrieve channel configuration (names, PSKs, channel hashes) from the radio.

Once running, it acts as a bidirectional relay:

- **Serial → UDP**: Packets received from the radio are decoded (protobuf `FromRadio` → `MeshPacket`), re-encrypted with the appropriate channel key, and sent to the multicast group as raw `MeshPacket` bytes.
- **UDP → Serial**: Packets received from multicast are decrypted, stamped with the `TransportMulticastUdp` origin marker, wrapped in a `ToRadio` protobuf frame, and written to the serial port.

Duplicate suppression prevents echo loops. Packets are tracked by ID so they aren't forwarded back to the direction they came from. A periodic heartbeat keeps the serial connection alive.

Encryption is handled transparently: the radio speaks decoded payloads over serial, while the UDP side uses standard Meshtastic AES-CTR encryption, so LAN clients see the same encrypted packets they'd receive over the air.
