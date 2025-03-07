use std::{io, net::SocketAddr};

#[cfg(all(target_os = "linux", feature = "iouring"))]
use io_uring::{opcode, types};
#[cfg(unix)]
use {crate::net::unix::SocketAddr as UnixSocketAddr, socket2::SockAddr};
#[cfg(all(windows, any(feature = "legacy", feature = "poll-io")))]
use {
    crate::syscall, std::os::windows::io::AsRawSocket,
    windows_sys::Win32::Networking::WinSock::send,
};
#[cfg(all(unix, any(feature = "legacy", feature = "poll-io")))]
use {crate::syscall_u32, std::os::unix::prelude::AsRawFd};

use super::{super::shared_fd::SharedFd, Op, OpAble};
#[cfg(any(feature = "legacy", feature = "poll-io"))]
use crate::driver::ready::Direction;
use crate::{buf::IoBuf, BufResult};

pub(crate) struct Send<T> {
    /// Holds a strong ref to the FD, preventing the file from being closed
    /// while the operation is in-flight.
    #[allow(unused)]
    fd: SharedFd,

    pub(crate) buf: T,
}

impl<T: IoBuf> Op<Send<T>> {
    pub(crate) fn send(fd: SharedFd, buf: T) -> io::Result<Self> {
        Op::submit_with(Send { fd, buf })
    }

    #[allow(unused)]
    pub(crate) fn send_raw(fd: &SharedFd, buf: T) -> Send<T> {
        Send {
            fd: fd.clone(),
            buf,
        }
    }

    pub(crate) async fn write(self) -> BufResult<usize, T> {
        let complete = self.await;
        (complete.meta.result.map(|v| v as _), complete.data.buf)
    }
}

impl<T: IoBuf> OpAble for Send<T> {
    #[cfg(all(target_os = "linux", feature = "iouring"))]
    fn uring_op(&mut self) -> io_uring::squeue::Entry {
        #[allow(deprecated)]
        #[cfg(feature = "zero-copy")]
        fn zero_copy_flag_guard<T: IoBuf>(buf: &T) -> libc::c_int {
            // TODO: use libc const after supported.
            const MSG_ZEROCOPY: libc::c_int = 0x4000000;
            // According to Linux's documentation, zero copy introduces extra overhead and
            // is only considered effective for at writes over around 10 KB.
            // see also: https://www.kernel.org/doc/html/v4.16/networking/msg_zerocopy.html
            const MSG_ZEROCOPY_THRESHOLD: usize = 10 * 1024 * 1024;
            if buf.bytes_init() >= MSG_ZEROCOPY_THRESHOLD {
                libc::MSG_NOSIGNAL as libc::c_int | MSG_ZEROCOPY
            } else {
                libc::MSG_NOSIGNAL as libc::c_int
            }
        }

        #[cfg(feature = "zero-copy")]
        let flags = zero_copy_flag_guard(&self.buf);
        #[cfg(not(feature = "zero-copy"))]
        #[allow(deprecated)]
        let flags = libc::MSG_NOSIGNAL as libc::c_int;

        opcode::Send::new(
            types::Fd(self.fd.raw_fd()),
            self.buf.read_ptr(),
            self.buf.bytes_init() as _,
        )
        .flags(flags)
        .build()
    }

    #[cfg(any(feature = "legacy", feature = "poll-io"))]
    #[inline]
    fn legacy_interest(&self) -> Option<(Direction, usize)> {
        self.fd
            .registered_index()
            .map(|idx| (Direction::Write, idx))
    }

    #[cfg(all(any(feature = "legacy", feature = "poll-io"), unix))]
    fn legacy_call(&mut self) -> io::Result<u32> {
        let fd = self.fd.as_raw_fd();
        #[cfg(target_os = "linux")]
        #[allow(deprecated)]
        let flags = libc::MSG_NOSIGNAL as _;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;

        syscall_u32!(send(
            fd,
            self.buf.read_ptr() as _,
            self.buf.bytes_init(),
            flags
        ))
    }

    #[cfg(all(any(feature = "legacy", feature = "poll-io"), windows))]
    fn legacy_call(&mut self) -> io::Result<u32> {
        let fd = self.fd.as_raw_socket();
        syscall!(
            send(fd as _, self.buf.read_ptr(), self.buf.bytes_init() as _, 0),
            PartialOrd::ge,
            0
        )
    }
}

pub(crate) struct SendMsg<T> {
    /// Holds a strong ref to the FD, preventing the file from being closed
    /// while the operation is in-flight.
    #[allow(unused)]
    fd: SharedFd,

    /// Reference to the in-flight buffer.
    pub(crate) buf: T,
    #[cfg(unix)]
    pub(crate) info: Box<(Option<SockAddr>, [libc::iovec; 1], libc::msghdr)>,
}

#[cfg(unix)]
impl<T: IoBuf> Op<SendMsg<T>> {
    pub(crate) fn send_msg(
        fd: SharedFd,
        buf: T,
        socket_addr: Option<SocketAddr>,
    ) -> io::Result<Self> {
        let iovec = [libc::iovec {
            iov_base: buf.read_ptr() as *const _ as *mut _,
            iov_len: buf.bytes_init(),
        }];
        let mut info: Box<(Option<SockAddr>, [libc::iovec; 1], libc::msghdr)> =
            Box::new((socket_addr.map(Into::into), iovec, unsafe {
                std::mem::zeroed()
            }));

        info.2.msg_iov = info.1.as_mut_ptr();
        info.2.msg_iovlen = 1;

        match info.0.as_ref() {
            Some(socket_addr) => {
                info.2.msg_name = socket_addr.as_ptr() as *mut libc::c_void;
                info.2.msg_namelen = socket_addr.len();
            }
            None => {
                info.2.msg_name = std::ptr::null_mut();
                info.2.msg_namelen = 0;
            }
        }

        Op::submit_with(SendMsg { fd, buf, info })
    }

    pub(crate) async fn wait(self) -> BufResult<usize, T> {
        let complete = self.await;
        let res = complete.meta.result.map(|v| v as _);
        let buf = complete.data.buf;
        (res, buf)
    }
}

#[cfg(windows)]
impl<T: IoBuf> Op<SendMsg<T>> {
    #[allow(unused_variables)]
    pub(crate) fn send_msg(
        fd: SharedFd,
        buf: T,
        socket_addr: Option<SocketAddr>,
    ) -> io::Result<Self> {
        unimplemented!()
    }

    pub(crate) async fn wait(self) -> BufResult<usize, T> {
        unimplemented!()
    }
}

impl<T: IoBuf> OpAble for SendMsg<T> {
    #[cfg(all(target_os = "linux", feature = "iouring"))]
    fn uring_op(&mut self) -> io_uring::squeue::Entry {
        #[allow(deprecated)]
        const FLAGS: u32 = libc::MSG_NOSIGNAL as u32;
        opcode::SendMsg::new(types::Fd(self.fd.raw_fd()), &mut self.info.2 as *mut _)
            .flags(FLAGS)
            .build()
    }

    #[cfg(any(feature = "legacy", feature = "poll-io"))]
    #[inline]
    fn legacy_interest(&self) -> Option<(Direction, usize)> {
        self.fd
            .registered_index()
            .map(|idx| (Direction::Write, idx))
    }

    #[cfg(all(any(feature = "legacy", feature = "poll-io"), unix))]
    fn legacy_call(&mut self) -> io::Result<u32> {
        #[cfg(target_os = "linux")]
        #[allow(deprecated)]
        const FLAGS: libc::c_int = libc::MSG_NOSIGNAL as libc::c_int;
        #[cfg(not(target_os = "linux"))]
        const FLAGS: libc::c_int = 0;
        let fd = self.fd.as_raw_fd();
        syscall_u32!(sendmsg(fd, &mut self.info.2 as *mut _, FLAGS))
    }

    #[cfg(all(any(feature = "legacy", feature = "poll-io"), windows))]
    fn legacy_call(&mut self) -> io::Result<u32> {
        let _fd = self.fd.as_raw_socket();
        unimplemented!();
    }
}

#[cfg(unix)]
pub(crate) struct SendMsgUnix<T> {
    /// Holds a strong ref to the FD, preventing the file from being closed
    /// while the operation is in-flight.
    #[allow(unused)]
    fd: SharedFd,

    /// Reference to the in-flight buffer.
    pub(crate) buf: T,
    pub(crate) info: Box<(Option<UnixSocketAddr>, [libc::iovec; 1], libc::msghdr)>,
}

#[cfg(unix)]
impl<T: IoBuf> Op<SendMsgUnix<T>> {
    pub(crate) fn send_msg_unix(
        fd: SharedFd,
        buf: T,
        socket_addr: Option<UnixSocketAddr>,
    ) -> io::Result<Self> {
        let iovec = [libc::iovec {
            iov_base: buf.read_ptr() as *const _ as *mut _,
            iov_len: buf.bytes_init(),
        }];
        let mut info: Box<(Option<UnixSocketAddr>, [libc::iovec; 1], libc::msghdr)> =
            Box::new((socket_addr.map(Into::into), iovec, unsafe {
                std::mem::zeroed()
            }));

        info.2.msg_iov = info.1.as_mut_ptr();
        info.2.msg_iovlen = 1;

        match info.0.as_ref() {
            Some(socket_addr) => {
                info.2.msg_name = socket_addr.as_ptr() as *mut libc::c_void;
                info.2.msg_namelen = socket_addr.len();
            }
            None => {
                info.2.msg_name = std::ptr::null_mut();
                info.2.msg_namelen = 0;
            }
        }

        Op::submit_with(SendMsgUnix { fd, buf, info })
    }

    pub(crate) async fn wait(self) -> BufResult<usize, T> {
        let complete = self.await;
        let res = complete.meta.result.map(|v| v as _);
        let buf = complete.data.buf;
        (res, buf)
    }
}

#[cfg(unix)]
impl<T: IoBuf> OpAble for SendMsgUnix<T> {
    #[cfg(all(target_os = "linux", feature = "iouring"))]
    fn uring_op(&mut self) -> io_uring::squeue::Entry {
        #[allow(deprecated)]
        const FLAGS: u32 = libc::MSG_NOSIGNAL as u32;
        opcode::SendMsg::new(types::Fd(self.fd.raw_fd()), &mut self.info.2 as *mut _)
            .flags(FLAGS)
            .build()
    }

    #[cfg(any(feature = "legacy", feature = "poll-io"))]
    #[inline]
    fn legacy_interest(&self) -> Option<(Direction, usize)> {
        self.fd
            .registered_index()
            .map(|idx| (Direction::Write, idx))
    }

    #[cfg(any(feature = "legacy", feature = "poll-io"))]
    #[inline]
    fn legacy_call(&mut self) -> io::Result<u32> {
        #[cfg(target_os = "linux")]
        #[allow(deprecated)]
        const FLAGS: libc::c_int = libc::MSG_NOSIGNAL as libc::c_int;
        #[cfg(not(target_os = "linux"))]
        const FLAGS: libc::c_int = 0;
        let fd = self.fd.as_raw_fd();
        syscall_u32!(sendmsg(fd, &mut self.info.2 as *mut _, FLAGS))
    }
}
