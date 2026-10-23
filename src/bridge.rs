use std::collections::{HashSet, VecDeque};
use std::io::{self, Read};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use prost::Message;

use crate::crypto::{self, ChannelKey};
use crate::meshtastic_proto::mesh_packet::{PayloadVariant, TransportMechanism};
use crate::serial_framing::FrameReader;
use crate::udp;

const RECENT_PACKETS_CAPACITY: usize = 1024;
const SERIAL_READ_TIMEOUT: Duration = Duration::from_millis(500);
const UDP_READ_TIMEOUT: Duration = Duration::from_millis(500);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_NONCE_NODEINFO_PING: u32 = 1;
const BROADCAST_ADDR: u32 = 0xFFFF_FFFF;
pub const MQTT_PROXY_GATEWAY_ID: &str = "!meshtastic-serial-udp";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Ingress {
    #[value(name = "mqtt-proxy")]
    MqttProxy,
    Phone,
}

pub enum BridgeEvent {
    SerialFrame(Vec<u8>),
    UdpPacket(Vec<u8>),
    SerialError(io::Error),
    UdpError(io::Error),
    Tick,
}

pub enum SessionEnd {
    RadioRebooted(Box<dyn serialport::SerialPort>),
    SerialLost(io::Error),
    UdpFailed(io::Error),
}

pub struct BridgeConfig {
    pub multicast_addr: Ipv4Addr,
    pub udp_port: u16,
    pub ingress: Ingress,
    pub my_node_num: u32,
}

pub struct RecentPackets {
    order: VecDeque<(u32, u32)>,
    set: HashSet<(u32, u32)>,
    capacity: usize,
}

impl RecentPackets {
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    #[cfg(test)]
    pub fn contains(&self, from: u32, id: u32) -> bool {
        self.set.contains(&(from, id))
    }

    pub fn insert(&mut self, from: u32, id: u32) -> bool {
        if !self.set.insert((from, id)) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        self.order.push_back((from, id));
        true
    }
}

pub struct Bridge {
    serial: Box<dyn serialport::SerialPort>,
    udp_socket: UdpSocket,
    config: BridgeConfig,
    recent: RecentPackets,
    channels: Vec<ChannelKey>,
}

impl Bridge {
    pub fn new(
        serial: Box<dyn serialport::SerialPort>,
        udp_socket: UdpSocket,
        config: BridgeConfig,
        channels: Vec<ChannelKey>,
    ) -> Self {
        Self {
            serial,
            udp_socket,
            config,
            recent: RecentPackets::new(RECENT_PACKETS_CAPACITY),
            channels,
        }
    }

    /// Run the main bridge loop. Spawns two reader threads and dispatches on main thread.
    pub fn run(mut self) -> io::Result<SessionEnd> {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        let udp_reader = self.udp_socket.try_clone()?;
        udp_reader.set_read_timeout(Some(UDP_READ_TIMEOUT))?;
        let mut serial_reader = self.serial.try_clone()?;
        serial_reader.set_timeout(SERIAL_READ_TIMEOUT)?;

        let mut threads = Vec::new();

        let tx_serial = tx.clone();
        let stop_serial = Arc::clone(&stop);
        // Serial reader thread
        threads.push(
            thread::Builder::new()
                .name("serial-reader".into())
                .spawn(move || {
                    let mut reader = FrameReader::new();
                    let mut buf = [0u8; 256];
                    while !stop_serial.load(Ordering::Relaxed) {
                        match serial_reader.read(&mut buf) {
                            Ok(0) => {
                                let _ = tx_serial.send(BridgeEvent::SerialError(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "serial port closed",
                                )));
                                return;
                            }
                            Ok(n) => {
                                for frame in reader.feed_bytes(&buf[..n]) {
                                    if tx_serial.send(BridgeEvent::SerialFrame(frame)).is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => continue,
                            Err(e) => {
                                let _ = tx_serial.send(BridgeEvent::SerialError(e));
                                return;
                            }
                        }
                    }
                })?,
        );

        let tx_udp = tx.clone();
        let stop_udp = Arc::clone(&stop);
        // UDP reader thread
        threads.push(
            thread::Builder::new()
                .name("udp-reader".into())
                .spawn(move || {
                    let mut buf = [0u8; 600];
                    while !stop_udp.load(Ordering::Relaxed) {
                        match udp_reader.recv_from(&mut buf) {
                            Ok((n, addr)) => {
                                log::trace!("UDP recv {n} bytes from {addr}");
                                if tx_udp.send(BridgeEvent::UdpPacket(buf[..n].to_vec())).is_err() {
                                    return;
                                }
                            }
                            Err(ref e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(e) => {
                                let _ = tx_udp.send(BridgeEvent::UdpError(e));
                                return;
                            }
                        }
                    }
                })?,
        );

        let stop_heartbeat = Arc::clone(&stop);
        // Heartbeat timer thread
        threads.push(
            thread::Builder::new()
                .name("heartbeat-timer".into())
                .spawn(move || {
                    let mut next = Instant::now() + HEARTBEAT_INTERVAL;
                    while !stop_heartbeat.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(250));
                        if Instant::now() >= next {
                            next += HEARTBEAT_INTERVAL;
                            if tx.send(BridgeEvent::Tick).is_err() {
                                return;
                            }
                        }
                    }
                })?,
        );

        let mut heartbeat_nonce: u32 = HEARTBEAT_NONCE_NODEINFO_PING;
        // Main dispatch loop — owns serial write handle + UDP send socket
        let end = loop {
            let Ok(event) = rx.recv() else {
                break Some(SessionEnd::SerialLost(io::Error::new(
                    io::ErrorKind::Other,
                    "all reader threads exited",
                )));
            };
            match event {
                BridgeEvent::SerialFrame(payload) => {
                    if self.handle_serial_frame(&payload) {
                        log::warn!("radio reported a reboot, redoing handshake");
                        break None;
                    }
                }
                BridgeEvent::UdpPacket(data) => self.handle_udp_packet(&data),
                BridgeEvent::SerialError(e) => {
                    log::error!("serial error: {e}");
                    break Some(SessionEnd::SerialLost(e));
                }
                BridgeEvent::UdpError(e) => {
                    log::error!("UDP error: {e}");
                    break Some(SessionEnd::UdpFailed(e));
                }
                BridgeEvent::Tick => {
                    heartbeat_nonce = heartbeat_nonce.wrapping_add(1);
                    if heartbeat_nonce == HEARTBEAT_NONCE_NODEINFO_PING {
                        heartbeat_nonce = heartbeat_nonce.wrapping_add(1);
                    }
                    let hb = crate::meshtastic_proto::ToRadio {
                        payload_variant: Some(
                            crate::meshtastic_proto::to_radio::PayloadVariant::Heartbeat(
                                crate::meshtastic_proto::Heartbeat {
                                    nonce: heartbeat_nonce,
                                },
                            ),
                        ),
                    };
                    let payload = hb.encode_to_vec();
                    let frame = crate::serial_framing::frame_payload(&payload);
                    match self.serial.write_all(&frame) {
                        Ok(()) => log::debug!("sent serial heartbeat (nonce={heartbeat_nonce})"),
                        Err(e) => log::error!("failed to send heartbeat: {e}"),
                    }
                }
            }
        };

        stop.store(true, Ordering::Relaxed);
        drop(rx);
        for t in threads {
            let _ = t.join();
        }
        Ok(end.unwrap_or(SessionEnd::RadioRebooted(self.serial)))
    }

    fn handle_serial_frame(&mut self, payload: &[u8]) -> bool {
        let mut packet = match crate::serial::decode_frame(payload) {
            crate::serial::RadioFrame::Packet(p) => p,
            crate::serial::RadioFrame::Rebooted => return true,
            crate::serial::RadioFrame::Other => return false,
        };
        log::debug!("bridge: decoded serial packet id={:#010x}", packet.id);

        // Duplicate suppression: skip packets that originated from UDP multicast
        if packet.transport_mechanism == TransportMechanism::TransportMulticastUdp as i32
            || packet.transport_mechanism == TransportMechanism::TransportMqtt as i32
        {
            log::debug!("skipping LAN-originated echo (id={:#010x})", packet.id);
            return false;
        }

        if packet.pki_encrypted {
            log::debug!("skipping PKI packet (id={:#010x})", packet.id);
            return false;
        }

        // Track this id so that if the UDP multicast echoes back, handle_udp_packet rejects it
        self.recent.insert(packet.from, packet.id);

        // Encrypt decoded payload before sending to UDP (so Raven sees standard encrypted packets)
        self.encrypt_packet(&mut packet);

        // Re-serialize the MeshPacket and send to UDP multicast
        let data = packet.encode_to_vec();
        match udp::send_multicast(
            &self.udp_socket,
            &data,
            self.config.multicast_addr,
            self.config.udp_port,
        ) {
            Ok(_) => log::debug!(
                "serial->UDP: forwarded packet id={:#010x} ({} bytes)",
                packet.id,
                data.len()
            ),
            Err(e) => log::error!("failed to send UDP multicast: {e}"),
        }
        false
    }

    fn handle_udp_packet(&mut self, data: &[u8]) {
        let Some(mut packet) = udp::decode_packet(data) else {
            return;
        };

        if !matches!(packet.payload_variant, Some(PayloadVariant::Encrypted(_))) {
            log::debug!("dropping non-encrypted UDP packet (id={:#010x})", packet.id);
            return;
        }
        if packet.from == 0 || packet.from == self.config.my_node_num {
            log::debug!("dropping UDP packet claiming to be from us (id={:#010x})", packet.id);
            return;
        }

        // Duplicate suppression: skip packets we recently forwarded (from serial or UDP)
        if !self.recent.insert(packet.from, packet.id) {
            log::debug!("skipping recently-seen packet echo (id={:#010x})", packet.id);
            return;
        }

        let result = match self.config.ingress {
            Ingress::MqttProxy => {
                let Some(channel_id) = self.downlink_channel_id(&packet) else {
                    log::debug!(
                        "no channel with hash {} for UDP packet id={:#010x}, dropping",
                        packet.channel,
                        packet.id
                    );
                    return;
                };
                let envelope = crate::meshtastic_proto::ServiceEnvelope {
                    packet: Some(packet),
                    channel_id,
                    gateway_id: MQTT_PROXY_GATEWAY_ID.to_string(),
                };
                crate::serial::write_mqtt_proxy(&mut *self.serial, envelope)
            }
            Ingress::Phone => {
                // Decrypt encrypted payload before sending to serial (radio expects decoded packets)
                if !self.decrypt_packet(&mut packet) {
                    log::debug!(
                        "could not decrypt UDP packet with channel hash {}, dropping",
                        packet.channel
                    );
                    return;
                }
                // Stamp origin so firmware and downstream bridges know this came from UDP
                packet.transport_mechanism = TransportMechanism::TransportMulticastUdp as i32;
                crate::serial::write_packet(&mut *self.serial, packet)
            }
        };
        match result {
            Ok(()) => log::debug!("UDP->serial: forwarded packet"),
            Err(e) => log::error!("failed to write to serial: {e}"),
        }
    }

    fn downlink_channel_id(&self, packet: &crate::meshtastic_proto::MeshPacket) -> Option<String> {
        if let Some(ck) = self.channels.iter().find(|c| c.hash == packet.channel) {
            return Some(ck.name.clone());
        }
        if packet.pki_encrypted || (packet.channel == 0 && packet.to != BROADCAST_ADDR) {
            return Some("PKI".to_string());
        }
        None
    }

    /// Encrypt a decoded packet: convert payload_variant from Decoded to Encrypted,
    /// and replace the channel index with the channel hash.
    fn encrypt_packet(&self, packet: &mut crate::meshtastic_proto::MeshPacket) {
        let decoded_bytes = match &packet.payload_variant {
            Some(PayloadVariant::Decoded(data)) => data.encode_to_vec(),
            _ => return, // already encrypted or empty — pass through
        };

        let channel_index = packet.channel;
        let Some(ck) = self.channels.iter().find(|c| c.index == channel_index) else {
            log::warn!(
                "no channel key for index {}, sending decoded",
                channel_index
            );
            return;
        };

        if ck.key.is_empty() {
            return; // no encryption configured for this channel
        }

        let ciphertext = crypto::encrypt_aes_ctr(&ck.key, packet.from, packet.id, &decoded_bytes);
        packet.payload_variant = Some(PayloadVariant::Encrypted(ciphertext));
        packet.channel = ck.hash;
        log::debug!(
            "encrypted packet: channel index {} -> hash {}",
            channel_index,
            ck.hash
        );
    }

    /// Decrypt an encrypted packet: convert payload_variant from Encrypted to Decoded,
    /// and replace the channel hash with the local channel index.
    fn decrypt_packet(&self, packet: &mut crate::meshtastic_proto::MeshPacket) -> bool {
        let ciphertext = match &packet.payload_variant {
            Some(PayloadVariant::Encrypted(data)) => data.clone(),
            _ => return true,
        };

        let channel_hash = packet.channel;

        // Find matching channel by hash. Try each matching channel's key.
        for ck in &self.channels {
            if ck.hash != channel_hash {
                continue;
            }
            if ck.key.is_empty() {
                continue;
            }

            let plaintext =
                crypto::decrypt_aes_ctr(&ck.key, packet.from, packet.id, &ciphertext);

            // Try to decode as a Data message to verify decryption succeeded
            if crate::meshtastic_proto::Data::decode(plaintext.as_slice()).is_ok() {
                packet.payload_variant = Some(PayloadVariant::Decoded(
                    crate::meshtastic_proto::Data::decode(plaintext.as_slice()).unwrap(),
                ));
                packet.channel = ck.index;
                log::debug!(
                    "decrypted packet: channel hash {} -> index {}",
                    channel_hash,
                    ck.index
                );
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meshtastic_proto;
    use crate::serial_framing;
    use crate::test_util::MockSerialPort;
    use serialport::SerialPort as _;

    const MY_NODE: u32 = 0x3344_5566;

    fn longfast() -> ChannelKey {
        crypto::channel_key_from_proto(0, "LongFast", &[1])
    }

    fn make_mesh_packet(id: u32) -> meshtastic_proto::MeshPacket {
        meshtastic_proto::MeshPacket {
            id,
            from: 0x1234,
            to: 0xFFFFFFFF,
            ..Default::default()
        }
    }

    fn make_udp_packet(id: u32, text: &str) -> meshtastic_proto::MeshPacket {
        let ck = longfast();
        let mut p = make_mesh_packet(id);
        let data = meshtastic_proto::Data {
            portnum: meshtastic_proto::PortNum::TextMessageApp.into(),
            payload: text.as_bytes().to_vec(),
            ..Default::default()
        };
        p.payload_variant = Some(PayloadVariant::Encrypted(crypto::encrypt_aes_ctr(
            &ck.key,
            p.from,
            p.id,
            &data.encode_to_vec(),
        )));
        p.channel = ck.hash;
        p.hop_limit = 3;
        p.hop_start = 3;
        p
    }

    fn from_radio_packet(packet: meshtastic_proto::MeshPacket) -> Vec<u8> {
        meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(meshtastic_proto::from_radio::PayloadVariant::Packet(packet)),
        }
        .encode_to_vec()
    }

    fn written_to_radio(mock: &MockSerialPort) -> Vec<meshtastic_proto::ToRadio> {
        let mut reader = serial_framing::FrameReader::new();
        reader
            .feed_bytes(&mock.take_written())
            .iter()
            .map(|f| meshtastic_proto::ToRadio::decode(f.as_slice()).unwrap())
            .collect()
    }

    /// Create a Bridge with a MockSerialPort and a localhost UDP pair.
    /// Returns (bridge, mock_serial, udp_receiver).
    /// The bridge's UDP socket sends to the receiver's address.
    fn test_bridge(ingress: Ingress) -> (Bridge, MockSerialPort, UdpSocket) {
        let mock = MockSerialPort::new();
        let serial_clone = mock.try_clone().unwrap();

        // Receiver socket — binds to a random port on localhost
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();

        // Sender socket — the bridge will send_to the receiver's address
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();

        let config = BridgeConfig {
            multicast_addr: Ipv4Addr::LOCALHOST,
            udp_port: receiver_addr.port(),
            ingress,
            my_node_num: MY_NODE,
        };

        let bridge = Bridge::new(serial_clone, sender, config, vec![longfast()]);

        (bridge, mock, receiver)
    }

    fn assert_nothing_received(receiver: &UdpSocket) {
        let mut buf = [0u8; 600];
        assert!(receiver.recv_from(&mut buf).is_err(), "should not have sent anything to UDP");
    }


    #[test]
    fn test_recent_packets_eviction() {
        let mut recent = RecentPackets::new(4);
        for id in 0..5 {
            assert!(recent.insert(1, id));
        }
        // First ID (0) should have been evicted
        assert!(!recent.contains(1, 0));
        assert!(recent.contains(1, 4));
        assert_eq!(recent.order.len(), 4);
        assert_eq!(recent.set.len(), 4);
    }

    #[test]
    fn test_recent_packets_dedup() {
        let mut recent = RecentPackets::new(4);
        assert!(recent.insert(1, 42));
        assert!(!recent.insert(1, 42));
        assert!(recent.insert(2, 42));
        assert_eq!(recent.order.len(), 2);
    }

    // --- handle_serial_frame tests ---

    #[test]
    fn test_serial_frame_forwards_to_udp_encrypted() {
        let (mut bridge, _mock, receiver) = test_bridge(Ingress::MqttProxy);

        let mut packet = make_mesh_packet(0xBEEF);
        packet.payload_variant = Some(PayloadVariant::Decoded(meshtastic_proto::Data {
            portnum: meshtastic_proto::PortNum::TextMessageApp.into(),
            payload: b"hi".to_vec(),
            ..Default::default()
        }));
        assert!(!bridge.handle_serial_frame(&from_radio_packet(packet)));

        let mut buf = [0u8; 600];
        let (n, _addr) = receiver.recv_from(&mut buf).expect("should receive UDP data");
        let received = meshtastic_proto::MeshPacket::decode(&buf[..n]).unwrap();
        assert_eq!(received.id, 0xBEEF);
        assert_eq!(received.from, 0x1234);
        assert_eq!(received.channel, 8);
        let Some(PayloadVariant::Encrypted(ct)) = received.payload_variant else {
            panic!("expected encrypted payload");
        };
        let pt = crypto::decrypt_aes_ctr(&longfast().key, 0x1234, 0xBEEF, &ct);
        assert_eq!(meshtastic_proto::Data::decode(pt.as_slice()).unwrap().payload, b"hi");
    }

    #[test]
    fn test_serial_frame_skips_lan_echoes() {
        let (mut bridge, _mock, receiver) = test_bridge(Ingress::MqttProxy);

        for transport in [
            TransportMechanism::TransportMulticastUdp,
            TransportMechanism::TransportMqtt,
        ] {
            let mut packet = make_mesh_packet(0xCAFE);
            packet.transport_mechanism = transport as i32;
            bridge.handle_serial_frame(&from_radio_packet(packet));
        }

        assert_nothing_received(&receiver);
    }

    #[test]
    fn test_serial_frame_skips_pki() {
        let (mut bridge, _mock, receiver) = test_bridge(Ingress::MqttProxy);

        let mut packet = make_mesh_packet(0xCAFE);
        packet.pki_encrypted = true;
        bridge.handle_serial_frame(&from_radio_packet(packet));

        assert_nothing_received(&receiver);
    }

    #[test]
    fn test_serial_frame_ignores_non_packet() {
        let (mut bridge, _mock, receiver) = test_bridge(Ingress::MqttProxy);

        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::ConfigCompleteId(42),
            ),
        };
        assert!(!bridge.handle_serial_frame(&from_radio.encode_to_vec()));

        assert_nothing_received(&receiver);
    }

    #[test]
    fn test_serial_frame_reports_reboot() {
        let (mut bridge, _mock, receiver) = test_bridge(Ingress::MqttProxy);

        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(meshtastic_proto::from_radio::PayloadVariant::Rebooted(true)),
        };
        assert!(bridge.handle_serial_frame(&from_radio.encode_to_vec()));

        assert_nothing_received(&receiver);
    }

    #[test]
    fn test_serial_frame_malformed() {
        let (mut bridge, _mock, receiver) = test_bridge(Ingress::MqttProxy);

        // Feed garbage — should not panic
        bridge.handle_serial_frame(&[0xFF, 0xFE, 0xFD]);

        assert_nothing_received(&receiver);
    }


    #[test]
    fn test_udp_packet_mqtt_proxy_keeps_sender() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        let packet = make_udp_packet(0xFACE, "hello");
        bridge.handle_udp_packet(&packet.encode_to_vec());

        let written = written_to_radio(&mock);
        assert_eq!(written.len(), 1);
        let Some(meshtastic_proto::to_radio::PayloadVariant::MqttClientProxyMessage(msg)) =
            &written[0].payload_variant
        else {
            panic!("expected MqttClientProxyMessage, got {:?}", written[0]);
        };
        let Some(meshtastic_proto::mqtt_client_proxy_message::PayloadVariant::Data(data)) =
            &msg.payload_variant
        else {
            panic!("expected data payload");
        };
        let envelope = meshtastic_proto::ServiceEnvelope::decode(data.as_slice()).unwrap();
        assert_eq!(envelope.channel_id, "LongFast");
        assert_eq!(envelope.gateway_id, MQTT_PROXY_GATEWAY_ID);
        assert_eq!(envelope.packet.unwrap(), packet);
    }

    #[test]
    fn test_udp_packet_mqtt_proxy_pki() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        let mut packet = make_udp_packet(0xFACE, "secret");
        packet.channel = 0;
        packet.to = MY_NODE;
        packet.pki_encrypted = true;
        bridge.handle_udp_packet(&packet.encode_to_vec());

        let written = written_to_radio(&mock);
        let Some(meshtastic_proto::to_radio::PayloadVariant::MqttClientProxyMessage(msg)) =
            &written[0].payload_variant
        else {
            panic!("expected MqttClientProxyMessage");
        };
        let Some(meshtastic_proto::mqtt_client_proxy_message::PayloadVariant::Data(data)) =
            &msg.payload_variant
        else {
            panic!("expected data payload");
        };
        let envelope = meshtastic_proto::ServiceEnvelope::decode(data.as_slice()).unwrap();
        assert_eq!(envelope.channel_id, "PKI");
    }

    #[test]
    fn test_udp_packet_unknown_channel_dropped() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        let mut packet = make_udp_packet(0xFACE, "hello");
        packet.channel = 77;
        bridge.handle_udp_packet(&packet.encode_to_vec());

        assert!(mock.take_written().is_empty());
    }

    #[test]
    fn test_udp_packet_rejects_decoded_and_spoofed() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        bridge.handle_udp_packet(&make_mesh_packet(1).encode_to_vec());
        let mut spoofed = make_udp_packet(2, "hi");
        spoofed.from = MY_NODE;
        bridge.handle_udp_packet(&spoofed.encode_to_vec());

        assert!(mock.take_written().is_empty());
    }

    #[test]
    fn test_udp_packet_dedup_skips_repeat() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        let data = make_udp_packet(0xAAAA, "x").encode_to_vec();

        bridge.handle_udp_packet(&data);
        assert!(!mock.take_written().is_empty());

        bridge.handle_udp_packet(&data);
        assert!(mock.take_written().is_empty(), "duplicate should be suppressed");
    }

    #[test]
    fn test_udp_packet_dedup_eviction() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        for i in 0..=RECENT_PACKETS_CAPACITY as u32 {
            bridge.handle_udp_packet(&make_udp_packet(i, "x").encode_to_vec());
        }
        mock.take_written();

        bridge.handle_udp_packet(&make_udp_packet(0, "x").encode_to_vec());
        assert!(!mock.take_written().is_empty(), "evicted packet should be forwarded again");
    }

    #[test]
    fn test_udp_packet_malformed() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        // Feed garbage — should not panic
        bridge.handle_udp_packet(&[0xFF, 0xFE, 0xFD]);

        assert!(mock.take_written().is_empty(), "should not write malformed data to serial");
    }

    // --- Echo-loop prevention tests ---

    #[test]
    fn test_serial_frame_adds_to_recent_preventing_udp_echo() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::MqttProxy);

        bridge.handle_serial_frame(&from_radio_packet(make_mesh_packet(0xDEAD)));

        bridge.handle_udp_packet(&make_udp_packet(0xDEAD, "x").encode_to_vec());

        assert!(mock.take_written().is_empty(), "UDP echo of serial packet should be rejected");
    }


    #[test]
    fn test_udp_packet_phone_ingress_drops_undecryptable() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::Phone);

        let mut packet = make_udp_packet(0xBEAD, "hello");
        packet.channel = 77;
        bridge.handle_udp_packet(&packet.encode_to_vec());

        assert!(mock.take_written().is_empty());
    }

    #[test]
    fn test_udp_packet_phone_ingress_decrypts_and_stamps() {
        let (mut bridge, mock, _receiver) = test_bridge(Ingress::Phone);

        bridge.handle_udp_packet(&make_udp_packet(0xBEAD, "hello").encode_to_vec());

        let written = written_to_radio(&mock);
        assert_eq!(written.len(), 1);
        match &written[0].payload_variant {
            Some(meshtastic_proto::to_radio::PayloadVariant::Packet(p)) => {
                assert_eq!(p.id, 0xBEAD);
                assert_eq!(p.channel, 0, "channel hash should be mapped to the channel index");
                assert_eq!(
                    p.transport_mechanism,
                    TransportMechanism::TransportMulticastUdp as i32,
                    "packet forwarded from UDP should be stamped with TransportMulticastUdp"
                );
                let Some(PayloadVariant::Decoded(d)) = &p.payload_variant else {
                    panic!("expected decoded payload");
                };
                assert_eq!(d.payload, b"hello");
            }
            other => panic!("expected Packet variant, got {other:?}"),
        }
    }
}
