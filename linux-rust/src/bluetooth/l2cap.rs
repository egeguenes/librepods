//! Cross-platform L2CAP `SOCK_SEQPACKET` channel (BR/EDR, PSM based).
//!
//! * Linux: thin wrapper around BlueR's `l2cap::SeqPacket` (BlueZ kernel sockets).
//! * FreeBSD: native netgraph Bluetooth sockets
//!   (`PF_BLUETOOTH` / `SOCK_SEQPACKET` / `BLUETOOTH_PROTO_L2CAP`), driven by
//!   tokio's `AsyncFd`.
//!
//! Errors are plain `std::io::Error`s so callers can match on
//! `io::ErrorKind` instead of OS-specific errno values (e.g. `ENOTCONN` is 107
//! on Linux but 57 on FreeBSD).

use crate::bluetooth::Address;
use std::io;
use std::time::Duration;

pub struct L2capChannel {
    inner: imp::Channel,
}

impl L2capChannel {
    /// Open an L2CAP connection-oriented channel to `addr` on `psm` and wait
    /// until it is fully established (or `timeout` elapses).
    pub async fn connect(addr: Address, psm: u16, timeout: Duration) -> io::Result<Self> {
        let inner = tokio::time::timeout(timeout, imp::Channel::connect(addr, psm))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "L2CAP connection timeout"))??;
        Ok(Self { inner })
    }

    pub async fn send(&self, data: &[u8]) -> io::Result<usize> {
        self.inner.send(data).await
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf).await
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use crate::bluetooth::Address;
    use bluer::AddressType;
    use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
    use log::error;
    use std::io;
    use std::time::Duration;

    const POLL_INTERVAL: Duration = Duration::from_millis(200);

    pub struct Channel(SeqPacket);

    impl Channel {
        pub async fn connect(addr: Address, psm: u16) -> io::Result<Self> {
            let target_sa = SocketAddr::new(addr.into(), AddressType::BrEdr, psm);
            let socket = Socket::new_seq_packet()?;
            let seq_packet = socket.connect(target_sa).await?;

            // BlueZ may report the socket as connected before the channel has
            // been assigned a CID; wait until it is fully established.
            loop {
                match seq_packet.peer_addr() {
                    Ok(peer) if peer.cid != 0 => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotConnected => {
                        error!("Peer has disconnected during connection setup.");
                        return Err(e);
                    }
                    Err(e) => error!("Error getting peer address: {}", e),
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Ok(Self(seq_packet))
        }

        pub async fn send(&self, data: &[u8]) -> io::Result<usize> {
            self.0.send(data).await
        }

        pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.recv(buf).await
        }
    }
}

#[cfg(target_os = "freebsd")]
mod imp {
    use crate::bluetooth::Address;
    use libc::{c_int, c_void, socklen_t};
    use std::io;
    use std::mem::size_of;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    /// `<netgraph/bluetooth/include/ng_btsocket.h>`
    const BLUETOOTH_PROTO_L2CAP: c_int = 135;
    /// `<netgraph/bluetooth/include/ng_hci.h>`
    const BDADDR_BREDR: u8 = 0;

    /// `struct sockaddr_l2cap` (14 bytes; psm@2, bdaddr@4, cid@10, type@12).
    #[repr(C)]
    #[derive(Default)]
    struct SockaddrL2cap {
        l2cap_len: u8,
        l2cap_family: u8,
        /// Little endian.
        l2cap_psm: u16,
        l2cap_bdaddr: [u8; 6],
        l2cap_cid: u16,
        l2cap_bdaddr_type: u8,
    }

    const _: () = assert!(size_of::<SockaddrL2cap>() == 14);

    impl SockaddrL2cap {
        fn new(bdaddr: [u8; 6], psm: u16) -> Self {
            Self {
                l2cap_len: size_of::<Self>() as u8,
                l2cap_family: libc::AF_BLUETOOTH as u8,
                l2cap_psm: psm.to_le(),
                l2cap_bdaddr: bdaddr,
                l2cap_cid: 0,
                l2cap_bdaddr_type: BDADDR_BREDR,
            }
        }
    }

    pub struct Channel(AsyncFd<OwnedFd>);

    fn cvt(ret: c_int) -> io::Result<c_int> {
        if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
    }

    fn cvt_size(ret: isize) -> io::Result<usize> {
        if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret as usize) }
    }

    impl Channel {
        pub async fn connect(addr: Address, psm: u16) -> io::Result<Self> {
            // Same sequence as bthidd(8): non-blocking socket, bind to ANY,
            // connect (EINPROGRESS), wait for writability, check SO_ERROR.
            let raw = cvt(unsafe {
                libc::socket(
                    libc::PF_BLUETOOTH,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    BLUETOOTH_PROTO_L2CAP,
                )
            })
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::EAFNOSUPPORT)
                    || e.raw_os_error() == Some(libc::EPROTONOSUPPORT)
                {
                    io::Error::new(
                        e.kind(),
                        "Bluetooth sockets unavailable; is ng_btsocket loaded \
                         and the bluetooth service running?",
                    )
                } else {
                    e
                }
            })?;
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };

            let local = SockaddrL2cap::new([0; 6], 0);
            cvt(unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    &local as *const _ as *const libc::sockaddr,
                    size_of::<SockaddrL2cap>() as socklen_t,
                )
            })?;

            let remote = SockaddrL2cap::new(addr.to_le_bytes(), psm);
            let ret = unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    &remote as *const _ as *const libc::sockaddr,
                    size_of::<SockaddrL2cap>() as socklen_t,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINPROGRESS) {
                    return Err(err);
                }
            }

            let afd = AsyncFd::new(fd)?;
            let mut guard = afd.writable().await?;
            guard.retain_ready();
            drop(guard);

            let mut so_err: c_int = 0;
            let mut len = size_of::<c_int>() as socklen_t;
            cvt(unsafe {
                libc::getsockopt(
                    afd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut so_err as *mut _ as *mut c_void,
                    &mut len,
                )
            })?;
            if so_err != 0 {
                return Err(io::Error::from_raw_os_error(so_err));
            }
            Ok(Self(afd))
        }

        pub async fn send(&self, data: &[u8]) -> io::Result<usize> {
            self.0
                .async_io(Interest::WRITABLE, |fd| {
                    cvt_size(unsafe {
                        libc::send(fd.as_raw_fd(), data.as_ptr() as *const c_void, data.len(), 0)
                    })
                })
                .await
        }

        pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.0
                .async_io(Interest::READABLE, |fd| {
                    cvt_size(unsafe {
                        libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len(), 0)
                    })
                })
                .await
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
compile_error!("LibrePods currently supports Linux (BlueZ) and FreeBSD (netgraph) only.");
