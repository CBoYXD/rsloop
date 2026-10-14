//! Completing nonblocking connects without taking ownership of Python's fd.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use futures::task::AtomicWaker;
use pyo3::{PyTraverseError, PyVisit, prelude::*};

use crate::{
    engine::{LoopCommand, LoopCore},
    fd_ops,
    vibeio::{InnerRawHandle, ReadinessOp, RegistrationMode},
};

#[derive(Default)]
pub(crate) struct ConnectWake {
    done: AtomicBool,
    waker: AtomicWaker,
}

#[pyclass]
struct ConnectDone {
    wake: Arc<ConnectWake>,
    socket: Option<Py<PyAny>>,
}
#[pymethods]
impl ConnectDone {
    fn __call__(&self, _future: &Bound<'_, PyAny>) {
        self.wake.done.store(true, Ordering::Release);
        self.wake.waker.wake();
    }
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(socket) = &self.socket {
            visit.call(socket)?;
        }
        Ok(())
    }
    fn __clear__(&mut self) {
        self.socket = None;
        self.wake.done.store(true, Ordering::Release);
        self.wake.waker.wake();
    }
}

/// Keep the original socket alive and wake the native wait on cancellation.
pub(crate) fn connect_wait_state(
    py: Python<'_>,
    future: &Py<PyAny>,
    socket: Option<Py<PyAny>>,
) -> PyResult<Arc<ConnectWake>> {
    let wake = Arc::new(ConnectWake::default());
    future.call_method1(
        py,
        crate::python_names::add_done_callback(py),
        (Py::new(
            py,
            ConnectDone {
                wake: wake.clone(),
                socket,
            },
        )?,),
    )?;
    Ok(wake)
}

#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
pub(crate) async fn run_connect_watch_task(
    core: Arc<LoopCore>,
    fd: fd_ops::RawFd,
    future: Py<PyAny>,
    wake: Option<Arc<ConnectWake>>,
    socket_keepalive: Option<Py<PyAny>>,
) {
    let wake = match wake {
        Some(wake) => wake,
        None => match Python::attach(|py| connect_wait_state(py, &future, None)) {
            Ok(wake) => wake,
            Err(_) => return,
        },
    };
    let wait_errno = 'wait: {
        let Ok(raw) = i32::try_from(fd) else {
            break 'wait libc::EBADF;
        };
        let handle = match InnerRawHandle::new_with_mode(
            raw,
            mio::Interest::WRITABLE,
            RegistrationMode::Poll,
        ) {
            Ok(handle) => handle,
            Err(error) => break 'wait error.raw_os_error().unwrap_or(libc::EBADF),
        };
        let mut armed = false;
        let result = std::future::poll_fn(|cx| {
            wake.waker.register(cx.waker());
            if wake.done.load(Ordering::Acquire) {
                return Poll::Ready(Ok(false));
            }
            if armed {
                return Poll::Ready(Ok(true));
            }
            armed = true;
            handle
                .poll_op_poll(cx, &mut ReadinessOp::new_writable(&handle))
                .map(|result| result.map(|()| true))
        })
        .await;
        match result {
            Ok(false) => return,
            Ok(true) => 0,
            Err(error) => error.raw_os_error().unwrap_or(libc::EBADF),
        }
    };
    // The native registration is gone before the last task-owned reference
    // can close the original socket, including when cancellation drops this
    // future before its next poll. The done callback keeps another reference
    // until Python has checked SO_ERROR and completed the Future.
    drop(socket_keepalive);
    if !wake.done.load(Ordering::Acquire) {
        let _ = core.send_command(LoopCommand::ConnectCompleted {
            future,
            fd,
            wait_errno,
        });
    }
}
