use std::io;

use prost::Message;

use crate::meshtastic_proto;
use crate::serial_framing::{self, FrameReader};

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Decode a serial frame payload (FromRadio) and extract the inner MeshPacket, if present.
pub fn decode_packet(payload: &[u8]) -> Option<meshtastic_proto::MeshPacket> {
    match meshtastic_proto::FromRadio::decode(payload) {
        Ok(from_radio) => match from_radio.payload_variant {
            Some(meshtastic_proto::from_radio::PayloadVariant::Packet(p)) => Some(p),
            Some(_) => {
                log::debug!("ignoring non-packet FromRadio variant");
                None
            }
            None => None,
        },
        Err(e) => {
            log::warn!("failed to decode FromRadio: {e}");
            None
        }
    }
}

/// Wrap a MeshPacket in ToRadio, frame it, and write to the serial port.
pub fn write_packet(
    serial: &mut dyn serialport::SerialPort,
    packet: meshtastic_proto::MeshPacket,
) -> io::Result<()> {
    let to_radio = meshtastic_proto::ToRadio {
        payload_variant: Some(meshtastic_proto::to_radio::PayloadVariant::Packet(packet)),
    };
    let payload = to_radio.encode_to_vec();
    let frame = serial_framing::frame_payload(&payload);
    serial.write_all(&frame)
}

/// Perform the serial handshake with the default 30-second timeout.
pub fn handshake(serial: &mut dyn serialport::SerialPort) -> io::Result<()> {
    handshake_with_timeout(serial, HANDSHAKE_TIMEOUT)
}

/// Perform the serial handshake: send want_config_id and wait for matching config_complete_id.
pub fn handshake_with_timeout(
    serial: &mut dyn serialport::SerialPort,
    timeout: std::time::Duration,
) -> io::Result<()> {
    let config_id: u32 = rand::random();
    log::info!("starting handshake with want_config_id={config_id}");

    let to_radio = meshtastic_proto::ToRadio {
        payload_variant: Some(
            meshtastic_proto::to_radio::PayloadVariant::WantConfigId(config_id),
        ),
    };
    let payload = to_radio.encode_to_vec();
    let frame = serial_framing::frame_payload(&payload);
    serial.write_all(&frame)?;

    let deadline = std::time::Instant::now() + timeout;
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 1];

    loop {
        if std::time::Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "handshake timed out waiting for config_complete_id",
            ));
        }

        match serial.read(&mut buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "serial port closed during handshake",
                ));
            }
            Ok(_) => {
                if let Some(payload) = reader.feed(buf[0]) {
                    match meshtastic_proto::FromRadio::decode(payload.as_slice()) {
                        Ok(from_radio) => {
                            if let Some(variant) = from_radio.payload_variant {
                                match variant {
                                    meshtastic_proto::from_radio::PayloadVariant::ConfigCompleteId(id) => {
                                        if id == config_id {
                                            log::info!("handshake complete");
                                            return Ok(());
                                        }
                                        log::warn!("config_complete_id mismatch: got {id}, expected {config_id}");
                                    }
                                    _ => {
                                        log::debug!("handshake: ignoring non-config FromRadio");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("handshake: failed to decode FromRadio: {e}");
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_framing;
    use crate::test_util::MockSerialPort;
    use serialport::SerialPort as _;

    fn make_mesh_packet(id: u32) -> meshtastic_proto::MeshPacket {
        meshtastic_proto::MeshPacket {
            id,
            from: 0x1234,
            to: 0xFFFFFFFF,
            channel: 0,
            ..Default::default()
        }
    }

    // --- decode_packet tests ---

    #[test]
    fn test_decode_packet_valid() {
        let packet = make_mesh_packet(42);
        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::Packet(packet.clone()),
            ),
        };
        let bytes = from_radio.encode_to_vec();
        let result = decode_packet(&bytes);
        assert!(result.is_some());
        let decoded = result.unwrap();
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.from, 0x1234);
        assert_eq!(decoded.to, 0xFFFFFFFF);
    }

    #[test]
    fn test_decode_packet_non_packet_variant() {
        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::ConfigCompleteId(123),
            ),
        };
        let bytes = from_radio.encode_to_vec();
        assert!(decode_packet(&bytes).is_none());
    }

    #[test]
    fn test_decode_packet_empty_payload() {
        let from_radio = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: None,
        };
        let bytes = from_radio.encode_to_vec();
        assert!(decode_packet(&bytes).is_none());
    }

    #[test]
    fn test_decode_packet_malformed() {
        assert!(decode_packet(&[0xFF, 0xFE, 0xFD, 0xFC]).is_none());
        assert!(decode_packet(&[]).is_none());
    }

    // --- write_packet tests ---

    #[test]
    fn test_write_packet_roundtrip() {
        let packet = make_mesh_packet(0xDEAD);
        let mut mock = MockSerialPort::new();

        write_packet(&mut mock, packet).unwrap();

        let written = mock.take_written();
        // Should be a framed ToRadio: [0x94, 0xC3, len_hi, len_lo, payload...]
        assert!(written.len() >= 4);
        assert_eq!(written[0], 0x94);
        assert_eq!(written[1], 0xC3);

        // Unframe
        let mut reader = serial_framing::FrameReader::new();
        let frames = reader.feed_bytes(&written);
        assert_eq!(frames.len(), 1);

        // Decode ToRadio
        let to_radio = meshtastic_proto::ToRadio::decode(frames[0].as_slice()).unwrap();
        match to_radio.payload_variant {
            Some(meshtastic_proto::to_radio::PayloadVariant::Packet(p)) => {
                assert_eq!(p.id, 0xDEAD);
                assert_eq!(p.from, 0x1234);
                assert_eq!(p.to, 0xFFFFFFFF);
            }
            other => panic!("expected Packet variant, got {other:?}"),
        }
    }

    // --- handshake tests ---

    #[test]
    fn test_handshake_success() {
        let mock = MockSerialPort::new();
        // Clone for handshake to use; original retains access to shared buffers
        let mut mock_clone = mock.try_clone().unwrap();

        let handle = std::thread::spawn(move || {
            handshake_with_timeout(&mut *mock_clone, std::time::Duration::from_secs(5))
        });

        // Wait for the handshake to write the ToRadio
        let mut written = Vec::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            written = mock.take_written();
            if !written.is_empty() {
                break;
            }
        }
        assert!(!written.is_empty(), "handshake should have written ToRadio");

        // Decode the written ToRadio to get the config_id
        let mut reader = serial_framing::FrameReader::new();
        let frames = reader.feed_bytes(&written);
        assert_eq!(frames.len(), 1);
        let to_radio = meshtastic_proto::ToRadio::decode(frames[0].as_slice()).unwrap();
        let config_id = match to_radio.payload_variant {
            Some(meshtastic_proto::to_radio::PayloadVariant::WantConfigId(id)) => id,
            other => panic!("expected WantConfigId, got {other:?}"),
        };

        // Inject matching ConfigCompleteId response
        let response = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::ConfigCompleteId(config_id),
            ),
        };
        let response_bytes = response.encode_to_vec();
        let framed_response = serial_framing::frame_payload(&response_bytes);
        mock.inject_read_data(&framed_response);

        let result = handle.join().unwrap();
        assert!(result.is_ok(), "handshake should succeed: {result:?}");
    }

    #[test]
    fn test_handshake_ignores_non_config() {
        let mock = MockSerialPort::new();
        let mut mock_clone = mock.try_clone().unwrap();

        let handle = std::thread::spawn(move || {
            handshake_with_timeout(&mut *mock_clone, std::time::Duration::from_secs(5))
        });

        // Wait for write
        let mut written = Vec::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            written = mock.take_written();
            if !written.is_empty() {
                break;
            }
        }
        assert!(!written.is_empty());

        let mut reader = serial_framing::FrameReader::new();
        let frames = reader.feed_bytes(&written);
        let to_radio = meshtastic_proto::ToRadio::decode(frames[0].as_slice()).unwrap();
        let config_id = match to_radio.payload_variant {
            Some(meshtastic_proto::to_radio::PayloadVariant::WantConfigId(id)) => id,
            other => panic!("expected WantConfigId, got {other:?}"),
        };

        // First inject a non-config response (Packet variant)
        let noise = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::Packet(make_mesh_packet(99)),
            ),
        };
        mock.inject_read_data(&serial_framing::frame_payload(&noise.encode_to_vec()));

        // Small delay, then inject the correct response
        std::thread::sleep(std::time::Duration::from_millis(100));
        let response = meshtastic_proto::FromRadio {
            id: 0,
            payload_variant: Some(
                meshtastic_proto::from_radio::PayloadVariant::ConfigCompleteId(config_id),
            ),
        };
        mock.inject_read_data(&serial_framing::frame_payload(&response.encode_to_vec()));

        let result = handle.join().unwrap();
        assert!(result.is_ok(), "handshake should succeed: {result:?}");
    }

    #[test]
    fn test_handshake_timeout() {
        let mock = MockSerialPort::new();
        let mut mock_boxed: Box<dyn serialport::SerialPort> = Box::new(mock);

        // Use a very short timeout
        let result =
            handshake_with_timeout(&mut *mock_boxed, std::time::Duration::from_secs(1));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
