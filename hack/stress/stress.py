import argparse
import collections
import json
import os
import pty
import random
import select
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
import tty

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from meshtastic.protobuf import mesh_pb2, mqtt_pb2, portnums_pb2

DEFAULT_PSK = bytes([0xD4, 0xF1, 0xBB, 0x3A, 0x20, 0x29, 0x07, 0x59, 0xF0, 0xBC, 0xFF, 0xAB, 0xCF, 0x4E, 0x69, 0x01])
TM = mesh_pb2.MeshPacket.TransportMechanism


def xor_hash(b):
    h = 0
    for x in b:
        h ^= x
    return h


def channel_hash(name, key):
    return xor_hash(name.encode()) ^ xor_hash(key)


def aes_ctr(key, frm, pid, data):
    nonce = struct.pack("<IIII", pid, 0, frm, 0)
    algo = algorithms.AES(key)
    c = Cipher(algo, modes.CTR(nonce)).encryptor()
    return c.update(data) + c.finalize()


def frame(b):
    return b"\x94\xc3" + struct.pack(">H", len(b)) + b


class Deframer:
    def __init__(self):
        self.buf = bytearray()

    def feed(self, data):
        self.buf += data
        out = []
        while True:
            i = self.buf.find(b"\x94\xc3")
            if i < 0:
                self.buf = self.buf[-1:] if self.buf[-1:] == b"\x94" else bytearray()
                return out
            if i:
                del self.buf[:i]
            if len(self.buf) < 4:
                return out
            n = struct.unpack(">H", self.buf[2:4])[0]
            if n > 512:
                del self.buf[:2]
                continue
            if len(self.buf) < 4 + n:
                return out
            out.append(bytes(self.buf[4 : 4 + n]))
            del self.buf[: 4 + n]


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.c = collections.Counter()

    def inc(self, k, n=1):
        with self.lock:
            self.c[k] += n


class Proxy:

    def __init__(self, tcp_port, link, stats, baud=0, usb_cdc=False):
        self.stats = stats
        self.baud = baud
        self.usb_cdc = usb_cdc
        self.tcp_port = tcp_port
        self.link = link
        self.connect()
        self.make_pty()
        self.wlock = threading.Lock()
        self.air_tx = []
        self.air_cb = None
        self.to_radio_log = []
        self.from_radio_types = collections.Counter()
        self.alive = True
        threading.Thread(target=self._up, daemon=True).start()
        threading.Thread(target=self._down, args=(self.sock,), daemon=True).start()

    def make_pty(self):
        self.master, self.slave = pty.openpty()
        tty.setraw(self.slave)
        tty.setraw(self.master)
        if os.path.lexists(self.link):
            os.unlink(self.link)
        os.symlink(os.ttyname(self.slave), self.link)

    def connect(self):
        for _ in range(100):
            try:
                self.sock = socket.create_connection(("127.0.0.1", self.tcp_port))
                break
            except OSError:
                time.sleep(0.2)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def reconnect_after_reboot(self):
        self.stats.inc("reboots")
        if self.usb_cdc:
            os.unlink(self.link)
            m, sl = self.master, self.slave
            self.master = -1
            time.sleep(0.3)
            os.close(sl)
            os.close(m)
            time.sleep(1.5)
        time.sleep(0.5)
        self.connect()
        threading.Thread(target=self._down, args=(self.sock,), daemon=True).start()
        if self.usb_cdc:
            self.make_pty()
        else:
            os.write(self.master, frame(mesh_pb2.FromRadio(rebooted=True).SerializeToString()))

    def _send_tcp(self, payload):
        with self.wlock:
            try:
                self.sock.sendall(frame(payload))
            except OSError:
                self.stats.inc("tcp_send_failed")

    def _up(self):
        d = Deframer()
        while self.alive:
            m = self.master
            try:
                if not select.select([m], [], [], 0.2)[0] or m != self.master:
                    continue
                data = os.read(m, 4096)
            except (OSError, ValueError):
                time.sleep(0.05)
                continue
            for f in d.feed(data):
                tr = mesh_pb2.ToRadio()
                try:
                    tr.ParseFromString(f)
                    kind = tr.WhichOneof("payload_variant")
                except Exception:
                    kind = "garbage"
                self.stats.inc("to_radio." + str(kind))
                if kind == "packet":
                    self.to_radio_log.append((time.time(), tr.packet))
                elif kind == "mqttClientProxyMessage":
                    env = mqtt_pb2.ServiceEnvelope()
                    env.ParseFromString(tr.mqttClientProxyMessage.data)
                    self.to_radio_log.append((time.time(), env.packet))
                self._send_tcp(f)

    def _down(self, sock):
        d = Deframer()
        while True:
            try:
                data = sock.recv(65536)
            except OSError:
                data = b""
            if not data:
                self.stats.inc("tcp_closed")
                self.reconnect_after_reboot()
                return
            for f in d.feed(data):
                fr = mesh_pb2.FromRadio()
                try:
                    fr.ParseFromString(f)
                except Exception:
                    self.stats.inc("from_radio.garbage")
                    continue
                kind = fr.WhichOneof("payload_variant")
                if kind == "packet" and fr.packet.HasField("decoded") and fr.packet.decoded.portnum == portnums_pb2.SIMULATOR_APP:
                    p = mesh_pb2.MeshPacket()
                    p.CopyFrom(fr.packet)
                    c = mesh_pb2.Compressed()
                    c.ParseFromString(p.decoded.payload)
                    p.decoded.portnum = c.portnum
                    p.decoded.payload = c.data
                    self.air_tx.append((time.time(), p))
                    self.stats.inc("air_tx")
                    if self.air_cb:
                        self.air_cb(p)
                    continue
                self.stats.inc("from_radio." + str(kind))
                out = frame(f)
                if self.baud:
                    time.sleep(len(out) * 10 / self.baud)
                try:
                    os.write(self.master, out)
                except OSError:
                    self.stats.inc("pty_write_failed")

    def inject_air(self, frm, to, pid, portnum, payload, channel=0, hop_limit=3, want_ack=False, hop_start=None):
        c = mesh_pb2.Compressed(portnum=portnum, data=payload)
        p = mesh_pb2.MeshPacket()
        setattr(p, "from", frm)
        p.to = to
        p.id = pid
        p.channel = channel
        p.hop_limit = hop_limit
        p.hop_start = hop_limit if hop_start is None else hop_start
        p.want_ack = want_ack
        p.rx_snr = 7.5
        p.rx_rssi = -60
        p.decoded.portnum = portnums_pb2.SIMULATOR_APP
        p.decoded.payload = c.SerializeToString()
        self._send_tcp(mesh_pb2.ToRadio(packet=p).SerializeToString())
        self.stats.inc("air_rx_injected")


class Lan:

    def __init__(self, group, port, iface, stats):
        self.group, self.port, self.stats = group, port, stats
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        s.bind(("", port))
        s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(group) + socket.inet_aton(iface))
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(iface))
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
        self.s = s
        self.rx = []
        self.sent_ids = set()
        self.sent_raw = set()
        self.cb = None
        threading.Thread(target=self._rx, daemon=True).start()

    def _rx(self):
        while True:
            data, addr = self.s.recvfrom(2048)
            if data in self.sent_raw:
                continue
            p = mesh_pb2.MeshPacket()
            try:
                p.ParseFromString(data)
            except Exception:
                self.stats.inc("lan_rx_garbage")
                continue
            if p.id in self.sent_ids:
                self.stats.inc("lan_rx_own_echo")
                continue
            self.rx.append((time.time(), p))
            self.stats.inc("lan_rx")
            if self.cb:
                self.cb(p)

    def send(self, frm, to, pid, portnum, payload, key, chash, hop_limit=3):
        data = mesh_pb2.Data(portnum=portnum, payload=payload).SerializeToString()
        p = mesh_pb2.MeshPacket()
        setattr(p, "from", frm)
        p.to = to
        p.id = pid
        p.channel = chash
        p.hop_limit = hop_limit
        p.hop_start = hop_limit
        p.encrypted = aes_ctr(key, frm, pid, data)
        p.transport_mechanism = TM.Value("TRANSPORT_MULTICAST_UDP")
        self.sent_ids.add(pid)
        raw = p.SerializeToString()
        self.sent_raw.add(raw)
        self.s.sendto(raw, (self.group, self.port))
        self.stats.inc("lan_tx")


def make_payload(portnum, tag, seq, frm):
    if portnum == portnums_pb2.POSITION_APP:
        return mesh_pb2.Position(latitude_i=seq, longitude_i=frm & 0x7FFFFFFF, time=1700000000 + seq).SerializeToString()
    if portnum == portnums_pb2.TELEMETRY_APP:
        from meshtastic.protobuf import telemetry_pb2

        t = telemetry_pb2.Telemetry(time=1700000000 + seq)
        t.device_metrics.uptime_seconds = seq
        t.device_metrics.battery_level = frm & 0x3F
        return t.SerializeToString()
    return f"{tag}#{seq} from {frm:#x}".encode()


def configure_node(tcp_port):
    from meshtastic.protobuf import admin_pb2, config_pb2

    sock = socket.create_connection(("127.0.0.1", tcp_port))
    d = Deframer()
    sock.sendall(frame(mesh_pb2.ToRadio(want_config_id=42).SerializeToString()))
    my_node = lora = None
    sock.settimeout(10)
    while True:
        for f in d.feed(sock.recv(65536)):
            fr = mesh_pb2.FromRadio()
            fr.ParseFromString(f)
            kind = fr.WhichOneof("payload_variant")
            if kind == "my_info":
                my_node = fr.my_info.my_node_num
            elif kind == "config" and fr.config.HasField("lora"):
                lora = fr.config.lora
            elif kind == "config_complete_id":
                break
        else:
            continue
        break
    if lora.region != config_pb2.Config.LoRaConfig.US:
        lora.region = config_pb2.Config.LoRaConfig.US
        admin = admin_pb2.AdminMessage(set_config=config_pb2.Config(lora=lora))
        p = mesh_pb2.MeshPacket(to=my_node)
        p.decoded.portnum = portnums_pb2.ADMIN_APP
        p.decoded.payload = admin.SerializeToString()
        sock.sendall(frame(mesh_pb2.ToRadio(packet=p).SerializeToString()))
        time.sleep(3)
    sock.close()
    time.sleep(1)


def decrypt_lan(p, key):
    if p.HasField("decoded"):
        return p.decoded
    d = mesh_pb2.Data()
    d.ParseFromString(aes_ctr(key, getattr(p, "from"), p.id, p.encrypted))
    return d


def main():
    ap = argparse.ArgumentParser()
    repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    ap.add_argument("--meshtasticd", default="meshtasticd", help="native firmware build (pio run -e native)")
    ap.add_argument("--bridge", default=os.path.join(repo, "target", "release", "meshtastic-serial-udp"))
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--tcp-port", type=int, default=4410)
    ap.add_argument("--group", default="224.0.0.69")
    ap.add_argument("--udp-port", type=int, default=4403)
    ap.add_argument("--scenario", default="all")
    ap.add_argument("--duration", type=float, default=60)
    ap.add_argument("--rate", type=float, default=1.0, help="packets/sec per direction")
    ap.add_argument("--senders", type=int, default=5)
    ap.add_argument("--portnum", default="TEXT_MESSAGE_APP")
    ap.add_argument("--baud", type=int, default=0)
    ap.add_argument("--usb-cdc", action="store_true", help="radio reboots make the serial device disappear")
    ap.add_argument("--ingress", default=None)
    ap.add_argument("--bridge-log", default="info")
    ap.add_argument("--reboot-at", type=float, default=None, help="restart the node this many seconds into the run")
    ap.add_argument("--no-configure", action="store_true", help="leave the node factory-fresh (no LoRa region)")
    ap.add_argument("--lan-hash", type=int, default=None, help="channel hash LAN clients put on packets (default: correct LongFast hash)")
    args = ap.parse_args()

    os.makedirs(args.workdir, exist_ok=True)
    stats = Stats()
    node_log = open(os.path.join(args.workdir, "node.log"), "w")
    node_cmd = [args.meshtasticd, "-s", "-d", os.path.join(args.workdir, "fs"), "-h", "11:22:33:44:55:66", "-p", str(args.tcp_port)]
    node = subprocess.Popen(node_cmd, stdout=node_log, stderr=subprocess.STDOUT)

    def restart_node():
        nonlocal node
        node.terminate()
        node.wait()
        node = subprocess.Popen(node_cmd, stdout=node_log, stderr=subprocess.STDOUT)

    args.restart_node = restart_node
    time.sleep(4)
    if not args.no_configure:
        configure_node(args.tcp_port)
    link = os.path.join(args.workdir, "ttyBRIDGE")
    proxy = Proxy(args.tcp_port, link, stats, baud=args.baud, usb_cdc=args.usb_cdc)
    lan = Lan(args.group, args.udp_port, "127.0.0.1", stats)
    bridge_log = open(os.path.join(args.workdir, "bridge.log"), "w")
    env = dict(os.environ, RUST_LOG=args.bridge_log)
    bridge = subprocess.Popen(
        [args.bridge, "--port", link, "--udp-addr", args.group, "--udp-port", str(args.udp_port), "--interface", "127.0.0.1"]
        + (["--ingress", args.ingress] if args.ingress else []),
        stdout=bridge_log,
        stderr=subprocess.STDOUT,
        env=env,
    )

    def cleanup(*_):
        bridge.terminate()
        node.terminate()

    signal.signal(signal.SIGTERM, lambda *a: (cleanup(), sys.exit(1)))
    try:
        t0 = time.time()
        while time.time() - t0 < 90:
            if "bridge running" in open(os.path.join(args.workdir, "bridge.log")).read():
                break
            if bridge.poll() is not None:
                print("bridge exited early", file=sys.stderr)
                return 2
            time.sleep(0.2)
        else:
            print("bridge never came up", file=sys.stderr)
            return 2
        time.sleep(1)
        result = run_scenario(args, proxy, lan, stats, bridge)
        result["stats"] = dict(stats.c)
        result["bridge_alive"] = bridge.poll() is None
        result["node_alive"] = node.poll() is None
        print(json.dumps(result, indent=1, default=str))
        with open(os.path.join(args.workdir, "result.json"), "w") as f:
            json.dump(result, f, indent=1, default=str)
    finally:
        cleanup()
    return 0


MY_NODE = 0x33445566
KEY = DEFAULT_PSK
HASH_LONGFAST = channel_hash("LongFast", KEY)


def run_scenario(args, proxy, lan, stats, bridge):
    portnum = portnums_pb2.PortNum.Value(args.portnum)
    senders = [0x0A000000 + i for i in range(1, args.senders + 1)]
    res = {"scenario": args.scenario, "rate": args.rate, "duration": args.duration, "portnum": args.portnum}

    sent_air = {}
    sent_lan = {}
    lan_got = {}
    air_got = {}
    lock = threading.Lock()

    def on_lan(p):
        with lock:
            lan_got.setdefault(p.id, (time.time(), p))

    def on_air(p):
        with lock:
            air_got.setdefault(p.id, (time.time(), p))

    lan.cb = on_lan
    proxy.air_cb = on_air

    do_air = args.scenario in ("all", "lora2udp", "bidir")
    do_lan = args.scenario in ("all", "udp2lora", "bidir", "dupes")
    do_dupes = args.scenario == "dupes"
    pending_dupes = []
    interval = 1.0 / args.rate if args.rate > 0 else 1
    end = time.time() + args.duration
    seq = 0
    next_t = time.time()
    timeline = []
    reboot_t = time.time() + args.reboot_at if args.reboot_at is not None else None
    while time.time() < end:
        now = time.time()
        if reboot_t and now >= reboot_t:
            reboot_t = None
            args.restart_node()
            continue
        if now < next_t:
            time.sleep(min(next_t - now, 0.05))
            continue
        next_t += interval
        seq += 1
        frm = random.choice(senders)
        if do_air:
            pid = random.getrandbits(31) | 1
            payload = make_payload(portnum, "air", seq, frm)
            proxy.inject_air(frm, 0xFFFFFFFF, pid, portnum, payload)
            sent_air[pid] = (time.time(), frm, payload)
        if do_lan:
            pid = random.getrandbits(31) | 1
            payload = make_payload(portnum, "lan", seq, frm + 0x100)
            lan.send(frm + 0x100, 0xFFFFFFFF, pid, portnum, payload, KEY, HASH_LONGFAST if args.lan_hash is None else args.lan_hash)
            sent_lan[pid] = (time.time(), frm + 0x100, payload)
            if do_dupes:
                pending_dupes.append((time.time() + 0.5, frm + 0x100, pid, payload))
        while pending_dupes and pending_dupes[0][0] <= time.time():
            _, dfrm, dpid, dpayload = pending_dupes.pop(0)
            proxy.inject_air(dfrm, 0xFFFFFFFF, dpid, portnum, dpayload, hop_limit=2, hop_start=3)
        if bridge.poll() is not None:
            res["bridge_died_at"] = time.time() - (end - args.duration)
            break
    time.sleep(20)

    if do_air:
        delivered = 0
        bad_hash = 0
        bad_decrypt = 0
        bad_from = 0
        lat = []
        buckets = collections.defaultdict(lambda: [0, 0])
        t_start = min(v[0] for v in sent_air.values()) if sent_air else 0
        for pid, (t, frm, payload) in sent_air.items():
            b = int((t - t_start) // max(args.duration / 10, 1))
            buckets[b][0] += 1
            if pid not in lan_got:
                continue
            tg, p = lan_got[pid]
            if p.channel != HASH_LONGFAST:
                bad_hash += 1
            try:
                d = decrypt_lan(p, KEY)
                ok = d.payload == payload
            except Exception:
                ok = False
            if not ok:
                bad_decrypt += 1
            if getattr(p, "from") != frm:
                bad_from += 1
            delivered += 1
            buckets[b][1] += 1
            lat.append(tg - t)
        lat.sort()
        res["lora2udp"] = {
            "sent": len(sent_air),
            "delivered": delivered,
            "wrong_channel_hash": bad_hash,
            "undecryptable_with_LongFast_hash_key": bad_decrypt,
            "wrong_from": bad_from,
            "lat_p50": lat[len(lat) // 2] if lat else None,
            "lat_max": lat[-1] if lat else None,
            "timeline_sent_delivered": [buckets[k] for k in sorted(buckets)],
            "sample_rx_channel_hash": next(iter(lan_got.values()))[1].channel if lan_got else None,
            "expected_hash": HASH_LONGFAST,
            "unexpected_lan_rx": [
                (getattr(p, "from"), p.id, p.WhichOneof("payload_variant"))
                for pid, (t, p) in list(lan_got.items())[:2000]
                if pid not in sent_air
            ][:10],
            "unexpected_lan_rx_count": sum(1 for pid in lan_got if pid not in sent_air),
        }
    if do_lan:
        delivered = 0
        same_from = 0
        payload_ok = 0
        buckets = collections.defaultdict(lambda: [0, 0])
        t_start = min(v[0] for v in sent_lan.values()) if sent_lan else 0
        for pid, (t, frm, payload) in sent_lan.items():
            b = int((t - t_start) // max(args.duration / 10, 1))
            buckets[b][0] += 1
            if pid not in air_got:
                continue
            _, p = air_got[pid]
            delivered += 1
            buckets[b][1] += 1
            if getattr(p, "from") == frm:
                same_from += 1
            if p.decoded.payload == payload:
                payload_ok += 1
        to_radio_pkts = sum(1 for _, p in proxy.to_radio_log if p.id in sent_lan)
        air_tx_per_id = collections.Counter(p.id for _, p in proxy.air_tx if p.id in sent_lan)
        res["udp2lora_transmissions_per_packet"] = dict(collections.Counter(air_tx_per_id.values()))
        res["lan_echoes_of_own_packets"] = stats.c["lan_rx_own_echo"]
        res["udp2lora"] = {
            "sent": len(sent_lan),
            "bridge_wrote_to_serial": to_radio_pkts,
            "transmitted_over_lora": delivered,
            "kept_original_from": same_from,
            "payload_intact": payload_ok,
            "timeline_sent_delivered": [buckets[k] for k in sorted(buckets)],
            "sample_air_from": [hex(getattr(p, "from")) for _, p in list(air_got.values())[:3]],
        }
    return res


if __name__ == "__main__":
    sys.exit(main())
