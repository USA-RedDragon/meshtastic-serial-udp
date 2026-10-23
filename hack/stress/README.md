# End-to-end stress harness

`stress.py` runs the bridge against real Meshtastic firmware (the native `meshtasticd` build in simulated-radio mode) and checks every packet in both directions.

```
LAN client (stress.py) <-- UDP multicast --> bridge <-- PTY --> proxy (stress.py) <-- TCP API --> meshtasticd -s
```

The proxy presents meshtasticd's API to the bridge as a serial port and plays the part of the air: it injects packets as if the radio heard them over LoRa, and records what the radio transmits. It also simulates radio reboots, either UART-style (port stays open, radio sends `rebooted`) or USB-CDC-style (`--usb-cdc`: the serial device disappears and comes back).

## Setup

- Build the bridge: `cargo build --release`
- Build the firmware: in a checkout of [meshtastic/firmware](https://github.com/meshtastic/firmware), `pio run -e native` (needs the native build dependencies listed in the firmware docs). The binary ends up at `.pio/build/native/meshtasticd`.
- `pip install meshtastic cryptography`

## Running

```sh
python3 hack/stress/stress.py --meshtasticd /path/to/meshtasticd --workdir /tmp/stress \
    --scenario bidir --duration 300 --rate 0.5
```

Scenarios: `lora2udp`, `udp2lora`, `bidir`, `dupes` (each LAN packet is also heard over the air through a relay). Useful options: `--portnum` (e.g. `POSITION_APP`), `--reboot-at SECONDS`, `--usb-cdc`, `--ingress phone`. Run several at once by giving each its own `--tcp-port` and `--udp-port`.

The result is printed as JSON and saved with the node and bridge logs in the work directory. Its `timeline_sent_delivered` entries are `[sent, delivered]` per tenth of the run, which shows when delivery starts degrading.

Keep the radio's airtime in mind: on LongFast the radio transmits roughly one packet per second, and its transmit queue holds 16 packets, so sustained LAN→LoRa rates above that will drop packets whatever the bridge does.
