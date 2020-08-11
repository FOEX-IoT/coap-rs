use std::io::{Read, Result, Write};
use std::time::Duration;
use std::net::*;

#[derive(Debug)]
pub struct UDPWrapper(UdpSocket);

impl UDPWrapper {
    pub fn new(udp: UdpSocket) -> Self {
        UDPWrapper(udp)
    }
    pub fn connect(address: &SocketAddr) -> Result<Self> {
        #[inline]
        fn inaddr_any() -> IpAddr {
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
        }

        #[inline]
        fn in6addr_any() -> IpAddr {
            IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0))
        }

        let bind_address = match address {
            SocketAddr::V4(_) => SocketAddr::new(inaddr_any(), 0),
            SocketAddr::V6(_) => SocketAddr::new(in6addr_any(), 0),
        };
        let socket = UdpSocket::bind(&bind_address)?;
        socket.connect(address)?;
        Ok(UDPWrapper(socket))
    }
    pub fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        self.0.recv_from(buf)
    }
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<()> {
        self.0.set_read_timeout(dur)
    }
    pub fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> Result<usize> {
        self.0.send_to(buf, addr)
    }
    pub fn try_clone(&self) -> Result<Self> {
        let clone = self.0.try_clone()?;
        Ok(Self(clone))
    }
}

impl Read for UDPWrapper {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.0.recv(buf)
    }
}

impl Write for UDPWrapper {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.0.send(buf)
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
