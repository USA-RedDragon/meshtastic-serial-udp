use std::io::{self, Read};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::mpsc;
use std::thread;

use prost::Message;

use crate::serial_framing::FrameReader;
use crate::udp;

const SERIAL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Value of MeshPacket.TransportMechanism.TRANSPORT_MULTICAST_UDP
const TRANSPORT_MULTICAST_UDP: i32 = 6;

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
        if packet.transport_mechanism == TRANSPORT_MULTICAST_UDP {
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
                "serial→UDP: forwarded packet id={:#010x} ({} bytes)",
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

        match crate::serial::write_packet(&mut *self.serial, packet) {
            Ok(()) => log::debug!("UDP→serial: forwarded packet"),
            Err(e) => log::error!("failed to write to serial: {e}"),
        }
    }
}
