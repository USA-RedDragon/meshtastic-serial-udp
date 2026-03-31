use std::io::{self};

use prost::Message;

use crate::meshtastic_proto;
use crate::serial_framing::{self, FrameReader};

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Perform the serial handshake: send want_config_id and wait for matching config_complete_id.
pub fn handshake(serial: &mut dyn serialport::SerialPort) -> io::Result<()> {
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

    let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
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
