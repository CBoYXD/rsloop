//! Concrete I/O endpoints a transport can be attached to.
//!
//! A transport is created from a TCP socket, a Unix socket, or a pipe/file, and
//! its reader and writer halves are handed separate owned values. These enums
//! keep the per-target `match` in one place so the reader, writer, and TLS
//! paths can stay generic over the endpoint kind.
//!
//! `LazyWriterTarget` exists because most connections never need a dedicated
//! writer thread: the duplicate descriptor is only materialised once a write
//! actually has to be queued.

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
use std::{
    io::{self, Read},
    net::{Shutdown, TcpStream as StdTcpStream},
    sync::Arc,
    thread,
    time::Duration,
};

use pyo3::prelude::*;

#[cfg(unix)]
use super::duplicate_unix_direct_writer;
#[cfg(unix)]
use super::platform::unix_raw_fd;
use super::{
    duplicate_configured_tcp_stream,
    platform::{file_raw_fd, tcp_stream_raw_fd},
    write_queue::WriterReceiver,
};
use crate::fd_ops;

pub(super) enum TaskedDirectWriter {
    Tcp(Arc<StdTcpStream>),
    #[cfg(unix)]
    Unix(Arc<StdUnixStream>),
}

impl TaskedDirectWriter {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "TaskedDirectWriter")
    )]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "TaskedDirectWriter")
    )]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "TaskedDirectWriter")
    )]
    pub(super) fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Write),
        }
    }
}

pub enum ReaderTarget {
    File(std::fs::File),
    Tcp(Arc<StdTcpStream>),
    #[cfg(unix)]
    Unix(Arc<StdUnixStream>),
}

impl ReaderTarget {
    /// Read directly into spare capacity. These concrete OS APIs initialize
    /// exactly the returned prefix and never inspect uninitialized bytes.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "ReaderTarget"))]
    pub(super) fn read_into_vec(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        buf.clear();
        let capacity = buf.capacity();
        if capacity == 0 {
            return Ok(0);
        }
        let destination = buf.spare_capacity_mut().as_mut_ptr().cast::<u8>();
        #[cfg(unix)]
        let read = {
            let count = capacity.min(isize::MAX as usize);
            // SAFETY: the endpoint owns a live fd and the Vec exclusively
            // owns this writable, possibly uninitialized range. read never
            // reads from that range or retains the pointer.
            let result = unsafe { libc::read(self.fd() as _, destination.cast(), count) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            result as usize
        };
        #[cfg(windows)]
        let read = match self {
            Self::Tcp(stream) => {
                use std::os::windows::io::AsRawSocket;

                use windows_sys::Win32::Networking::WinSock::{WSAGetLastError, recv};
                // SAFETY: the socket and exclusive destination remain live
                // throughout this synchronous call; recv writes at most count.
                let result = unsafe {
                    recv(
                        stream.as_raw_socket() as _,
                        destination,
                        capacity.min(i32::MAX as usize) as i32,
                        0,
                    )
                };
                if result < 0 {
                    // SAFETY: reads only the calling thread's Winsock error
                    // slot.
                    return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
                }
                result as usize
            }
            Self::File(file) => {
                use std::os::windows::io::AsRawHandle;

                use windows_sys::Win32::{
                    Foundation::{ERROR_BROKEN_PIPE, GetLastError},
                    Storage::FileSystem::ReadFile,
                };
                let mut count = 0;
                // SAFETY: file is synchronous, so null OVERLAPPED retains no
                // pointer; count and the exclusive Vec range are writable.
                let result = unsafe {
                    ReadFile(
                        file.as_raw_handle() as _,
                        destination,
                        capacity.min(u32::MAX as usize) as u32,
                        &mut count,
                        std::ptr::null_mut(),
                    )
                };
                if result == 0 {
                    // SAFETY: reads only the current thread's last-error slot.
                    let error = unsafe { GetLastError() };
                    if error == ERROR_BROKEN_PIPE {
                        0
                    } else {
                        return Err(io::Error::from_raw_os_error(error as i32));
                    }
                } else {
                    count as usize
                }
            }
        };
        // SAFETY: the synchronous OS operation initialized exactly this
        // prefix, bounded above by the Vec's writable spare capacity.
        unsafe {
            buf.set_len(read);
        }
        Ok(read)
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "ReaderTarget"))]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::File(file) => file_raw_fd(file),
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "ReaderTarget"))]
    #[cfg(windows)]
    pub(super) fn pollable(&self) -> bool {
        !matches!(self, Self::File(_))
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "ReaderTarget"))]
    #[cfg(not(windows))]
    pub(super) fn pollable(&self) -> bool {
        true
    }
}

impl Read for ReaderTarget {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<ReaderTarget as Read>")
    )]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(buf),
            Self::Tcp(stream) => stream.as_ref().read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.as_ref().read(buf),
        }
    }
}

pub(super) enum WriterTarget {
    File(std::fs::File),
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
    Sink(io::Sink),
}

pub(super) struct LazyWriterConfig {
    pub(super) target: LazyWriterTarget,
    pub(super) writer_rx: WriterReceiver,
}

pub(super) enum LazyWriterTarget {
    Tcp(fd_ops::RawFd),
    #[cfg(unix)]
    Unix(fd_ops::RawFd),
}

impl LazyWriterTarget {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "LazyWriterTarget"))]
    pub(super) fn materialize(self) -> PyResult<WriterTarget> {
        match self {
            Self::Tcp(fd) => duplicate_configured_tcp_stream(fd).map(WriterTarget::Tcp),
            #[cfg(unix)]
            Self::Unix(fd) => duplicate_unix_direct_writer(fd).map(WriterTarget::Unix),
        }
    }
}

impl WriterTarget {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterTarget"))]
    pub(super) fn fd(&self) -> Option<fd_ops::RawFd> {
        match self {
            Self::File(file) => Some(file_raw_fd(file)),
            Self::Tcp(stream) => Some(tcp_stream_raw_fd(stream)),
            #[cfg(unix)]
            Self::Unix(stream) => Some(unix_raw_fd(stream.as_raw_fd())),
            Self::Sink(_) => None,
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterTarget"))]
    #[cfg(windows)]
    pub(super) fn pollable(&self) -> bool {
        !matches!(self, Self::File(_))
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterTarget"))]
    #[cfg(not(windows))]
    pub(super) fn pollable(&self) -> bool {
        true
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterTarget"))]
    pub(super) fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Write),
            Self::File(_) | Self::Sink(_) => Ok(()),
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterTarget"))]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
            Self::File(_) | Self::Sink(_) => Ok(()),
        }
    }
}

pub(super) enum StreamKind {
    Tcp(StdTcpStream, i32),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl StreamKind {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "StreamKind"))]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::Tcp(stream, _) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "StreamKind"))]
    #[cfg(windows)]
    pub(super) fn pollable(&self) -> bool {
        true
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "StreamKind"))]
    #[cfg(not(windows))]
    pub(super) fn pollable(&self) -> bool {
        true
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "StreamKind"))]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream, _) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
        }
    }
}

impl Read for StreamKind {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<StreamKind as Read>")
    )]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream, _) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl io::Write for StreamKind {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<StreamKind as io :: Write>")
    )]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream, _) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<StreamKind as io :: Write>")
    )]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream, _) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

impl io::Write for WriterTarget {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<WriterTarget as io :: Write>")
    )]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(buf),
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
            Self::Sink(sink) => sink.write(buf),
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<WriterTarget as io :: Write>")
    )]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Sink(sink) => sink.flush(),
        }
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn shutdown_tcp_stream(stream: &StdTcpStream, how: Shutdown) -> io::Result<()> {
    for attempt in 0..=100 {
        match stream.shutdown(how) {
            Ok(()) => return Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(err) if is_no_buffer_space(&err) && attempt < 100 => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("bounded shutdown retry loop always returns")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn shutdown_unix_stream(stream: &StdUnixStream, how: Shutdown) -> io::Result<()> {
    for attempt in 0..=100 {
        match stream.shutdown(how) {
            Ok(()) => return Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(err) if is_no_buffer_space(&err) && attempt < 100 => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("bounded shutdown retry loop always returns")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
pub(super) fn is_no_buffer_space_code(raw_os_error: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        raw_os_error == Some(libc::ENOBUFS)
    }
    #[cfg(windows)]
    {
        raw_os_error == Some(10_055) // WSAENOBUFS
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
fn is_no_buffer_space(err: &io::Error) -> bool {
    is_no_buffer_space_code(err.raw_os_error())
}

#[cfg(kani)]
mod verification {
    use super::is_no_buffer_space_code;

    #[kani::proof]
    fn merge_no_buffer_space_code_is_exact() {
        let raw_os_error: Option<i32> = kani::any();

        #[cfg(unix)]
        assert_eq!(
            is_no_buffer_space_code(raw_os_error),
            raw_os_error == Some(libc::ENOBUFS)
        );
        #[cfg(windows)]
        assert_eq!(
            is_no_buffer_space_code(raw_os_error),
            raw_os_error == Some(10_055)
        );
    }
}
#[cfg(test)]
mod shutdown_tests {
    #[cfg(unix)]
    #[test]
    fn no_buffer_space_is_retryable_during_shutdown() {
        assert!(super::is_no_buffer_space(
            &std::io::Error::from_raw_os_error(libc::ENOBUFS)
        ));
    }
}

#[cfg(all(test, unix))]
mod read_buffer_tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn read_initializes_only_received_prefix_and_clears_stale_bytes() {
        let (reader, mut writer) = StdUnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut target = ReaderTarget::Unix(Arc::new(reader));
        let mut buffer = vec![99; 16384];
        let address = buffer.as_ptr();
        writer.write_all(b"first").unwrap();
        assert_eq!(target.read_into_vec(&mut buffer).unwrap(), 5);
        assert_eq!(buffer, b"first");
        writer.write_all(b"x").unwrap();
        assert_eq!(target.read_into_vec(&mut buffer).unwrap(), 1);
        assert_eq!(buffer, b"x");
        assert_eq!(buffer.as_ptr(), address);
        assert_eq!(
            target.read_into_vec(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(buffer.is_empty());
        drop(writer);
        assert_eq!(target.read_into_vec(&mut buffer).unwrap(), 0);
        assert!(buffer.is_empty());
    }
}
