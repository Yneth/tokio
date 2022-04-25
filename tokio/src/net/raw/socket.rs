use std::convert::TryFrom;
use std::fmt;
use std::io;
use std::net::{self, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::task::{Context, Poll};

use socket2::{Domain, Protocol};

use crate::io::{Interest, PollEvented, ReadBuf, Ready};
use crate::net::{to_socket_addrs, ToSocketAddrs};

cfg_io_util! {
    use bytes::BufMut;
}

cfg_net! {
    pub struct NetRawSocket {
        io: PollEvented<mio::net::NetRawSocket>,
    }
}

impl NetRawSocket {
    pub async fn new(domain: Domain, protocol: Option<Protocol>) -> io::Result<NetRawSocket> {
        let sys = mio::net::NetRawSocket::new(domain, protocol)?;
        sys.set_nonblocking(true)?;
        let io = PollEvented::new(sys)?;
        Ok(Self { io })
    }

    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        let mut addrs = to_socket_addrs(target).await?;

        match addrs.next() {
            Some(target) => self.send_to_addr(buf, target).await,
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no addresses to send data to",
            )),
        }
    }

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addrs = to_socket_addrs(addr).await?;
        let mut last_err = None;

        for addr in addrs {
            match self.io.connect(addr) {
                Ok(_) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        let event = self.io.registration().readiness(interest).await?;
        Ok(event.ready)
    }


    pub async fn writable(&self) -> io::Result<()> {
        self.ready(Interest::WRITABLE).await?;
        Ok(())
    }


    pub fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.registration().poll_write_ready(cx).map_ok(|_| ())
    }


    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.io
            .registration()
            .async_io(Interest::WRITABLE, || self.io.send(buf))
            .await
    }


    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.io
            .registration()
            .poll_write_io(cx, || self.io.send(buf))
    }

    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        self.io
            .registration()
            .try_io(Interest::WRITABLE, || self.io.send(buf))
    }

    /// Waits for the socket to become readable.
    ///
    /// This function is equivalent to `ready(Interest::READABLE)` and is usually
    /// paired with `try_recv()`.
    ///
    /// The function may complete without the socket being readable. This is a
    /// false-positive and attempting a `try_recv()` will return with
    /// `io::ErrorKind::WouldBlock`.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. Once a readiness event occurs, the method
    /// will continue to return immediately until the readiness event is
    /// consumed by an attempt to read that fails with `WouldBlock` or
    /// `Poll::Pending`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio::net::NetRawSocket;
    /// use std::io;
    ///
    /// #[tokio::main]
    /// async fn main() -> io::Result<()> {
    ///     // Connect to a peer
    ///     let socket = NetRawSocket::bind("127.0.0.1:8080").await?;
    ///     socket.connect("127.0.0.1:8081").await?;
    ///
    ///     loop {
    ///         // Wait for the socket to be readable
    ///         socket.readable().await?;
    ///
    ///         // The buffer is **not** included in the async task and will
    ///         // only exist on the stack.
    ///         let mut buf = [0; 1024];
    ///
    ///         // Try to recv data, this may still fail with `WouldBlock`
    ///         // if the readiness event is a false positive.
    ///         match socket.try_recv(&mut buf) {
    ///             Ok(n) => {
    ///                 println!("GOT {:?}", &buf[..n]);
    ///                 break;
    ///             }
    ///             Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
    ///                 continue;
    ///             }
    ///             Err(e) => {
    ///                 return Err(e);
    ///             }
    ///         }
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    pub async fn readable(&self) -> io::Result<()> {
        self.ready(Interest::READABLE).await?;
        Ok(())
    }

    /// Attempts to send data on the socket to a given address.
    ///
    /// Note that on multiple calls to a `poll_*` method in the send direction, only the
    /// `Waker` from the `Context` passed to the most recent call will be scheduled to
    /// receive a wakeup.
    ///
    /// # Return value
    ///
    /// The function returns:
    ///
    /// * `Poll::Pending` if the socket is not ready to write
    /// * `Poll::Ready(Ok(n))` `n` is the number of bytes sent.
    /// * `Poll::Ready(Err(e))` if an error is encountered.
    ///
    /// # Errors
    ///
    /// This function may encounter any standard I/O error except `WouldBlock`.
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        self.io
            .registration()
            .poll_write_io(cx, || self.io.send_to(buf, target))
    }

    /// Tries to send data on the socket to the given address, but if the send is
    /// blocked this will return right away.
    ///
    /// This function is usually paired with `writable()`.
    ///
    /// # Returns
    ///
    /// If successful, returns the number of bytes sent
    ///
    /// Users should ensure that when the remote cannot receive, the
    /// [`ErrorKind::WouldBlock`] is properly handled. An error can also occur
    /// if the IP version of the socket does not match that of `target`.
    ///
    /// [`ErrorKind::WouldBlock`]: std::io::ErrorKind::WouldBlock
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::NetRawSocket;
    /// use std::error::Error;
    /// use std::io;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error>> {
    ///     let socket = NetRawSocket::bind("127.0.0.1:8080").await?;
    ///
    ///     let dst = "127.0.0.1:8081".parse()?;
    ///
    ///     loop {
    ///         socket.writable().await?;
    ///
    ///         match socket.try_send_to(&b"hello world"[..], dst) {
    ///             Ok(sent) => {
    ///                 println!("sent {} bytes", sent);
    ///                 break;
    ///             }
    ///             Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
    ///                 // Writable false positive.
    ///                 continue;
    ///             }
    ///             Err(e) => return Err(e.into()),
    ///         }
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.io
            .registration()
            .try_io(Interest::WRITABLE, || self.io.send_to(buf, target))
    }

    async fn send_to_addr(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.io
            .registration()
            .async_io(Interest::WRITABLE, || self.io.send_to(buf, target))
            .await
    }

    /// Tries to read or write from the socket using a user-provided IO operation.
    ///
    /// If the socket is ready, the provided closure is called. The closure
    /// should attempt to perform IO operation from the socket by manually
    /// calling the appropriate syscall. If the operation fails because the
    /// socket is not actually ready, then the closure should return a
    /// `WouldBlock` error and the readiness flag is cleared. The return value
    /// of the closure is then returned by `try_io`.
    ///
    /// If the socket is not ready, then the closure is not called
    /// and a `WouldBlock` error is returned.
    ///
    /// The closure should only return a `WouldBlock` error if it has performed
    /// an IO operation on the socket that failed due to the socket not being
    /// ready. Returning a `WouldBlock` error in any other situation will
    /// incorrectly clear the readiness flag, which can cause the socket to
    /// behave incorrectly.
    ///
    /// The closure should not perform the IO operation using any of the methods
    /// defined on the Tokio `NetRawSocket` type, as this will mess with the
    /// readiness flag and can cause the socket to behave incorrectly.
    ///
    /// Usually, [`readable()`], [`writable()`] or [`ready()`] is used with this function.
    ///
    /// [`readable()`]: NetRawSocket::readable()
    /// [`writable()`]: NetRawSocket::writable()
    /// [`ready()`]: NetRawSocket::ready()
    pub fn try_io<R>(
        &self,
        interest: Interest,
        f: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<R> {
        self.io
            .registration()
            .try_io(interest, || self.io.try_io(f))
    }

    /// Gets the value of the `SO_BROADCAST` option for this socket.
    ///
    /// For more information about this option, see [`set_broadcast`].
    ///
    /// [`set_broadcast`]: method@Self::set_broadcast
    pub fn broadcast(&self) -> io::Result<bool> {
        self.io.broadcast()
    }

    /// Sets the value of the `SO_BROADCAST` option for this socket.
    ///
    /// When enabled, this socket is allowed to send packets to a broadcast
    /// address.
    pub fn set_broadcast(&self, on: bool) -> io::Result<()> {
        self.io.set_broadcast(on)
    }

    /// Gets the value of the `IP_MULTICAST_LOOP` option for this socket.
    ///
    /// For more information about this option, see [`set_multicast_loop_v4`].
    ///
    /// [`set_multicast_loop_v4`]: method@Self::set_multicast_loop_v4
    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        self.io.multicast_loop_v4()
    }

    /// Sets the value of the `IP_MULTICAST_LOOP` option for this socket.
    ///
    /// If enabled, multicast packets will be looped back to the local socket.
    ///
    /// # Note
    ///
    /// This may not have any affect on IPv6 sockets.
    pub fn set_multicast_loop_v4(&self, on: bool) -> io::Result<()> {
        self.io.set_multicast_loop_v4(on)
    }

    /// Gets the value of the `IP_MULTICAST_TTL` option for this socket.
    ///
    /// For more information about this option, see [`set_multicast_ttl_v4`].
    ///
    /// [`set_multicast_ttl_v4`]: method@Self::set_multicast_ttl_v4
    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        self.io.multicast_ttl_v4()
    }

    /// Sets the value of the `IP_MULTICAST_TTL` option for this socket.
    ///
    /// Indicates the time-to-live value of outgoing multicast packets for
    /// this socket. The default value is 1 which means that multicast packets
    /// don't leave the local network unless explicitly requested.
    ///
    /// # Note
    ///
    /// This may not have any affect on IPv6 sockets.
    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
        self.io.set_multicast_ttl_v4(ttl)
    }

    /// Gets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    ///
    /// For more information about this option, see [`set_multicast_loop_v6`].
    ///
    /// [`set_multicast_loop_v6`]: method@Self::set_multicast_loop_v6
    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        self.io.multicast_loop_v6()
    }

    /// Sets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    ///
    /// Controls whether this socket sees the multicast packets it sends itself.
    ///
    /// # Note
    ///
    /// This may not have any affect on IPv4 sockets.
    pub fn set_multicast_loop_v6(&self, on: bool) -> io::Result<()> {
        self.io.set_multicast_loop_v6(on)
    }

    /// Gets the value of the `IP_TTL` option for this socket.
    ///
    /// For more information about this option, see [`set_ttl`].
    ///
    /// [`set_ttl`]: method@Self::set_ttl
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio::net::NetRawSocket;
    /// # use std::io;
    ///
    /// # async fn dox() -> io::Result<()> {
    /// let sock = NetRawSocket::bind("127.0.0.1:8080").await?;
    ///
    /// println!("{:?}", sock.ttl()?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn ttl(&self) -> io::Result<u32> {
        self.io.ttl()
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    ///
    /// This value sets the time-to-live field that is used in every packet sent
    /// from this socket.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio::net::NetRawSocket;
    /// # use std::io;
    ///
    /// # async fn dox() -> io::Result<()> {
    /// let sock = NetRawSocket::bind("127.0.0.1:8080").await?;
    /// sock.set_ttl(60)?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.io.set_ttl(ttl)
    }

    /// Executes an operation of the `IP_ADD_MEMBERSHIP` type.
    ///
    /// This function specifies a new multicast group for this socket to join.
    /// The address must be a valid multicast address, and `interface` is the
    /// address of the local interface with which the system should join the
    /// multicast group. If it's equal to `INADDR_ANY` then an appropriate
    /// interface is chosen by the system.
    pub fn join_multicast_v4(&self, multiaddr: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        self.io.join_multicast_v4(&multiaddr, &interface)
    }

    pub fn set_header_included(&self, on: bool) -> io::Result<()> {
        self.io.set_header_included(on)
    }

    /// Executes an operation of the `IPV6_ADD_MEMBERSHIP` type.
    ///
    /// This function specifies a new multicast group for this socket to join.
    /// The address must be a valid multicast address, and `interface` is the
    /// index of the interface to join/leave (or 0 to indicate any interface).
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        self.io.join_multicast_v6(multiaddr, interface)
    }

    /// Executes an operation of the `IP_DROP_MEMBERSHIP` type.
    ///
    /// For more information about this option, see [`join_multicast_v4`].
    ///
    /// [`join_multicast_v4`]: method@Self::join_multicast_v4
    pub fn leave_multicast_v4(&self, multiaddr: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        self.io.leave_multicast_v4(&multiaddr, &interface)
    }

    /// Executes an operation of the `IPV6_DROP_MEMBERSHIP` type.
    ///
    /// For more information about this option, see [`join_multicast_v6`].
    ///
    /// [`join_multicast_v6`]: method@Self::join_multicast_v6
    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        self.io.leave_multicast_v6(multiaddr, interface)
    }

    /// Returns the value of the `SO_ERROR` option.
    ///
    /// # Examples
    /// ```
    /// use tokio::net::NetRawSocket;
    /// use std::io;
    ///
    /// #[tokio::main]
    /// async fn main() -> io::Result<()> {
    ///     // Create a socket
    ///     let socket = NetRawSocket::bind("0.0.0.0:8080").await?;
    ///
    ///     if let Ok(Some(err)) = socket.take_error() {
    ///         println!("Got error: {:?}", err);
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.io.take_error()
    }
}

impl fmt::Debug for NetRawSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.io.fmt(f)
    }
}

#[cfg(all(unix))]
mod sys {
    use std::os::unix::prelude::*;

    use super::NetRawSocket;

    impl AsRawFd for NetRawSocket {
        fn as_raw_fd(&self) -> RawFd {
            self.io.as_raw_fd()
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::os::windows::prelude::*;

    use super::NetRawSocket;

    impl AsRawSocket for NetRawSocket {
        fn as_raw_socket(&self) -> RawSocket {
            self.io.as_raw_socket()
        }
    }
}
