use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

pub fn setup_multicast_socket(
    multicast_addr: Ipv4Addr,
    port: u16,
    interface: Option<Ipv4Addr>,
) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;

    let iface = interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
    socket.join_multicast_v4(&multicast_addr, &iface)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(false)?;

    Ok(UdpSocket::from(socket))
}

/// Decode raw UDP bytes as a MeshPacket.
pub fn decode_packet(data: &[u8]) -> Option<crate::meshtastic_proto::MeshPacket> {
    use prost::Message;
    match crate::meshtastic_proto::MeshPacket::decode(data) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("failed to decode MeshPacket from UDP: {e}");
            None
        }
    }
}

pub fn send_multicast(
    socket: &UdpSocket,
    data: &[u8],
    multicast_addr: Ipv4Addr,
    port: u16,
) -> io::Result<usize> {
    socket.send_to(data, SocketAddrV4::new(multicast_addr, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn make_mesh_packet(id: u32) -> crate::meshtastic_proto::MeshPacket {
        crate::meshtastic_proto::MeshPacket {
            id,
            from: 0xABCD,
            to: 0xFFFFFFFF,
            ..Default::default()
        }
    }

    #[test]
    fn test_udp_decode_valid() {
        let packet = make_mesh_packet(0x1234);
        let bytes = packet.encode_to_vec();
        let result = decode_packet(&bytes);
        assert!(result.is_some());
        let decoded = result.unwrap();
        assert_eq!(decoded.id, 0x1234);
        assert_eq!(decoded.from, 0xABCD);
        assert_eq!(decoded.to, 0xFFFFFFFF);
    }

    #[test]
    fn test_udp_decode_malformed() {
        assert!(decode_packet(&[0xFF, 0xFE, 0xFD, 0xFC]).is_none());
    }

    #[test]
    fn test_udp_decode_empty() {
        // Empty bytes decode to a default MeshPacket (all zero fields) — prost considers this valid
        let result = decode_packet(&[]);
        // An empty buffer decodes to a default MeshPacket with id=0
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, 0);
    }
}
