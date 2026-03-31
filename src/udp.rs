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

pub fn send_multicast(
    socket: &UdpSocket,
    data: &[u8],
    multicast_addr: Ipv4Addr,
    port: u16,
) -> io::Result<usize> {
    socket.send_to(data, SocketAddrV4::new(multicast_addr, port))
}
