use std::collections::VecDeque;
use std::io::{self, Read};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::mpsc;
use std::thread;

use prost::Message;

use crate::meshtastic_proto::mesh_packet::TransportMechanism;
use crate::serial_framing::FrameReader;
use crate::udp;

const RECENT_IDS_CAPACITY: usize = 64;
const SERIAL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

pub enum BridgeEvent {
    SerialFrame(Vec<u8>),
    UdpPacket(Vec<u8>),
    SerialError(io::Error),
    UdpError(io::Error),
}

pub struct BridgeConfig {
    pub multicast_addr: Ipv4Addr,
    pub udp_port: u16,
}

pub struct Bridge {
    serial: Box<dyn serialport::SerialPort>,
    udp_socket: UdpSocket,
    config: BridgeConfig,
    recent_ids: VecDeque<u32>,
}

impl Bridge {
    pub fn new(
        serial: Box<dyn serialport::SerialPort>,
        udp_socket: UdpSocket,
        config: BridgeConfig,
    ) -> Self {
        Self {
            serial,
            udp_socket,
            config,
            recent_ids: VecDeque::with_capacity(RECENT_IDS_CAPACITY),
        }
    }

    /// Run the main bridge loop. Spawns two reader threads and dispatches on main thread.
    /// Returns on fatal error (serial disconnect, etc.).
    pub fn run(mut self) -> io::Result<()> {
        let (tx, rx) = mpsc::channel();

        // Clone handles BEFORE moving into threads
        let udp_reader = self.udp_socket.try_clone()?;
        let mut serial_reader = self.serial.try_clone()?;

        // Set timeout on the read-side clone
        serial_reader.set_timeout(SERIAL_READ_TIMEOUT)?;

        // Serial reader thread
        let tx_serial = tx.clone();
        thread::Builder::new()
            .name("serial-reader".into())
            .spawn(move || {
                let mut reader = FrameReader::new();
                let mut buf = [0u8; 256];
                loop {
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
            })?;

        // UDP reader thread
        thread::Builder::new()
            .name("udp-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 600];
                loop {
                    match udp_reader.recv_from(&mut buf) {
                        Ok((n, addr)) => {
                            log::trace!("UDP recv {n} bytes from {addr}");
                            if tx.send(BridgeEvent::UdpPacket(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(BridgeEvent::UdpError(e));
                            return;
                        }
                    }
                }
            })?;

        // Main dispatch loop — owns serial write handle + UDP send socket
        for event in rx {
            match event {
                BridgeEvent::SerialFrame(payload) => {
                    self.handle_serial_frame(&payload);
                }
                BridgeEvent::UdpPacket(data) => {
                    self.handle_udp_packet(&data);
                }
                BridgeEvent::SerialError(e) => {
                    log::error!("serial error: {e}");
                    return Err(e);
                }
                BridgeEvent::UdpError(e) => {
                    log::error!("UDP error: {e}");
                    return Err(e);
                }
            }
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            "all reader threads exited",
        ))
    }

    fn handle_serial_frame(&mut self, payload: &[u8]) {
        let Some(packet) = crate::serial::decode_packet(payload) else {
            return;
        };

        // Duplicate suppression: skip packets that originated from UDP multicast
        if packet.transport_mechanism == TransportMechanism::TransportMulticastUdp as i32 {
            log::debug!("skipping UDP-originated echo (id={:#010x})", packet.id);
            return;
        }

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
    }

    fn handle_udp_packet(&mut self, data: &[u8]) {
        let Some(packet) = udp::decode_packet(data) else {
            return;
        };

        // Duplicate suppression: skip packets we recently sent to serial
        if self.recent_ids.contains(&packet.id) {
            log::debug!("skipping recently-sent packet echo (id={:#010x})", packet.id);
            return;
        }

        // Track this id
        if self.recent_ids.len() >= RECENT_IDS_CAPACITY {
            self.recent_ids.pop_front();
        }
        self.recent_ids.push_back(packet.id);

        match crate::serial::write_packet(&mut *self.serial, packet) {
            Ok(()) => log::debug!("UDP->serial: forwarded packet"),
            Err(e) => log::error!("failed to write to serial: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meshtastic_proto;
    use crate::serial_framing;
    use crate::test_util::MockSerialPort;
    use serialport::SerialPort as _;

    fn make_mesh_packet(id: u32) -> meshtastic_proto::MeshPacket {
        meshtastic_proto::MeshPacket {
            id,
            from: 0x1234,
            to: 0xFFFFFFFF,
            ..Default::default()
        }
    }

    /// Create a Bridge with a MockSerialPort and a localhost UDP pair.
    /// Returns (bridge, mock_serial, udp_receiver).
    /// The bridge's UDP socket sends to the receiver's address.
    fn test_bridge() -> (Bridge, MockSerialPort, UdpSocket) {
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
        };

        let bridge = Bridge {
            serial: serial_clone,
            udp_socket: sender,
            config,
            recent_ids: VecDeque::with_capacity(RECENT_IDS_CAPACITY),
        };

        (bridge, mock, receiver)
    }

    // --- Existing dedup tests ---

    #[test]
    fn test_recent_ids_eviction() {
        let mut ids: VecDeque<u32> = VecDeque::with_capacity(RECENT_IDS_CAPACITY);
        for i in 0..=RECENT_IDS_CAPACITY as u32 {
            if ids.len() >= RECENT_IDS_CAPACITY {
                ids.pop_front();
            }
            ids.push_back(i);
        }
        // First ID (0) should have been evicted
        assert!(!ids.contains(&0));
        // Last ID should be present
        assert!(ids.contains(&(RECENT_IDS_CAPACITY as u32)));
        assert_eq!(ids.len(), RECENT_IDS_CAPACITY);
    }

    #[test]
    fn test_recent_ids_dedup() {
        let mut ids: VecDeque<u32> = VecDeque::with_capacity(RECENT_IDS_CAPACITY);
        ids.push_back(42);
        ids.push_back(99);
        assert!(ids.contains(&42));
        assert!(ids.contains(&99));
        assert!(!ids.contains(&100));
    }

    // --- handle_serial_frame tests ---

    #[test]
    fn test_serial_frame_forwards_to_udp() {
        let (mut bridge, _mock, receiver) = test_bridge();

        let packet = make_mesh_packet(0xBEEF);
        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::Packet(packet),
            ),
        };
        let payload = from_radio.encode_to_vec();

        bridge.handle_serial_frame(&payload);

        // Verify something arrived on the receiver
        let mut buf = [0u8; 600];
        let (n, _addr) = receiver.recv_from(&mut buf).expect("should receive UDP data");

        // Decode as MeshPacket
        let received = meshtastic_proto::MeshPacket::decode(&buf[..n]).unwrap();
        assert_eq!(received.id, 0xBEEF);
        assert_eq!(received.from, 0x1234);
    }

    #[test]
    fn test_serial_frame_skips_udp_echo() {
        let (mut bridge, _mock, receiver) = test_bridge();

        let mut packet = make_mesh_packet(0xCAFE);
        packet.transport_mechanism =
            crate::meshtastic_proto::mesh_packet::TransportMechanism::TransportMulticastUdp as i32;

        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::Packet(packet),
            ),
        };
        let payload = from_radio.encode_to_vec();

        bridge.handle_serial_frame(&payload);

        // Should NOT have sent anything
        let mut buf = [0u8; 600];
        let result = receiver.recv_from(&mut buf);
        assert!(result.is_err(), "should not receive anything for UDP echo");
    }

    #[test]
    fn test_serial_frame_ignores_non_packet() {
        let (mut bridge, _mock, receiver) = test_bridge();

        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::ConfigCompleteId(42),
            ),
        };
        let payload = from_radio.encode_to_vec();

        bridge.handle_serial_frame(&payload);

        let mut buf = [0u8; 600];
        let result = receiver.recv_from(&mut buf);
        assert!(result.is_err(), "should not forward non-packet variant");
    }

    #[test]
    fn test_serial_frame_malformed() {
        let (mut bridge, _mock, receiver) = test_bridge();

        // Feed garbage — should not panic
        bridge.handle_serial_frame(&[0xFF, 0xFE, 0xFD]);

        let mut buf = [0u8; 600];
        let result = receiver.recv_from(&mut buf);
        assert!(result.is_err(), "should not forward malformed data");
    }

    // --- handle_udp_packet tests ---

    #[test]
    fn test_udp_packet_forwards_to_serial() {
        let (mut bridge, mock, _receiver) = test_bridge();

        let packet = make_mesh_packet(0xFACE);
        let data = packet.encode_to_vec();

        bridge.handle_udp_packet(&data);

        let written = mock.take_written();
        assert!(!written.is_empty(), "should have written to serial");

        // Unframe
        let mut reader = serial_framing::FrameReader::new();
        let frames = reader.feed_bytes(&written);
        assert_eq!(frames.len(), 1);

        // Decode ToRadio
        let to_radio = meshtastic_proto::ToRadio::decode(frames[0].as_slice()).unwrap();
        match to_radio.payload_variant {
            Some(meshtastic_proto::to_radio::PayloadVariant::Packet(p)) => {
                assert_eq!(p.id, 0xFACE);
                assert_eq!(p.from, 0x1234);
            }
            other => panic!("expected Packet variant, got {other:?}"),
        }
    }

    #[test]
    fn test_udp_packet_dedup_skips_repeat() {
        let (mut bridge, mock, _receiver) = test_bridge();

        let packet = make_mesh_packet(0xAAAA);
        let data = packet.encode_to_vec();

        // First call — should write
        bridge.handle_udp_packet(&data);
        let written1 = mock.take_written();
        assert!(!written1.is_empty());

        // Second call with same ID — should NOT write
        bridge.handle_udp_packet(&data);
        let written2 = mock.take_written();
        assert!(written2.is_empty(), "duplicate ID should be suppressed");
    }

    #[test]
    fn test_udp_packet_dedup_eviction() {
        let (mut bridge, mock, _receiver) = test_bridge();

        // Fill 64 unique IDs
        for i in 0..RECENT_IDS_CAPACITY as u32 {
            let packet = make_mesh_packet(i);
            bridge.handle_udp_packet(&packet.encode_to_vec());
        }
        mock.take_written(); // drain

        // Resend ID 0 — it should have been evicted (still present, since we sent exactly 64)
        // Actually with exactly 64 IDs (0..63), capacity is full but no eviction yet.
        // Send one more to trigger eviction of ID 0.
        let extra = make_mesh_packet(999);
        bridge.handle_udp_packet(&extra.encode_to_vec());
        mock.take_written(); // drain

        // Now ID 0 has been evicted — resending it should forward
        let packet = make_mesh_packet(0);
        bridge.handle_udp_packet(&packet.encode_to_vec());
        let written = mock.take_written();
        assert!(!written.is_empty(), "evicted ID should be forwarded again");
    }

    #[test]
    fn test_udp_packet_malformed() {
        let (mut bridge, mock, _receiver) = test_bridge();

        // Feed garbage — should not panic
        bridge.handle_udp_packet(&[0xFF, 0xFE, 0xFD]);

        let written = mock.take_written();
        assert!(written.is_empty(), "should not write malformed data to serial");
    }
}
