use std::net::{Ipv4Addr, UdpSocket};

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
}
