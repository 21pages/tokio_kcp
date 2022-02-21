use std::{
    io::ErrorKind,
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use kcp::KcpResult;
use socket2::{Domain, Socket, Type};
use tokio::net::{lookup_host, UdpSocket};

#[inline]
pub fn now_millis() -> u32 {
    let start = SystemTime::now();
    let since_the_epoch = start.duration_since(UNIX_EPOCH).expect("time went afterwards");
    (since_the_epoch.as_secs() * 1000 + since_the_epoch.subsec_millis() as u64 / 1_000_000) as u32
}

fn new_socket(addr: SocketAddr, reuse: bool) -> Result<UdpSocket, std::io::Error> {
    let socket = match addr {
        SocketAddr::V4(..) => Socket::new(Domain::ipv4(), Type::dgram(), None),
        SocketAddr::V6(..) => Socket::new(Domain::ipv6(), Type::dgram(), None),
    }?;
    if reuse {
        // windows has no reuse_port, but it's reuse_address
        // almost equals to unix's reuse_port + reuse_address,
        // though may introduce nondeterministic behavior
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_reuse_address(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.set_read_timeout(Some(std::time::Duration::from_millis(100)))?;
    socket.bind(&addr.into())?;
    Ok(UdpSocket::from_std(socket.into_udp_socket())?)
}

#[allow(clippy::never_loop)]
pub async fn new_reuse<T: tokio::net::ToSocketAddrs>(addr: T) -> KcpResult<UdpSocket> {
    for addr in lookup_host(addr).await? {
        return Ok(new_socket(addr, true)?);
    }
    Err(kcp::Error::IoError(std::io::Error::new(
        ErrorKind::AddrNotAvailable,
        "could not resolve to any address",
    )))
}
