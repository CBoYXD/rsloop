//! Raw socket I/O with immediate completion and loop-local readiness waits.
//! Python socket methods remain authoritative (including subclass overrides).
//! The detached reactor only handles an owned duplicate and Rust wake state;
//! it never calls Python while the runtime is borrowed.

use std::{
    io,
    mem::ManuallyDrop,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use futures::{future::poll_fn, task::AtomicWaker};
use pyo3::{
    exceptions::{PyOSError, PyRuntimeError, PyValueError},
    prelude::*,
    types::PyTuple,
};

use super::PyLoop;
use crate::{
    engine::{CallbackArgs, CallbackKind, LoopCommand, LoopCore, ReadyCallback},
    fd_ops,
    vibeio::{InnerRawHandle, ReadinessOp, RegistrationMode},
};

pub(super) enum SocketAction {
    Recv(usize),
    RecvInto(Py<PyAny>),
    SendAll {
        data: Py<PyAny>,
        offset: usize,
        len: usize,
    },
    Accept,
}

impl SocketAction {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "SocketAction"))]
    fn writable(&self) -> bool {
        matches!(self, Self::SendAll { .. })
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "SocketAction"))]
    fn attempt(&mut self, py: Python<'_>, socket: &Py<PyAny>) -> PyResult<Option<Py<PyAny>>> {
        let result = match self {
            Self::Recv(count) => socket.call_method1(py, crate::python_names::recv(py), (*count,)),
            Self::RecvInto(buffer) => {
                socket.call_method1(py, crate::python_names::recv_into(py), (buffer.bind(py),))
            }
            Self::Accept => socket
                .call_method0(py, crate::python_names::accept(py))
                .and_then(|accepted| {
                    accepted
                        .bind(py)
                        .get_item(0)?
                        .call_method1(crate::python_names::setblocking(py), (false,))?;
                    Ok(accepted)
                }),
            Self::SendAll { data, offset, len } => {
                // Bound synchronous progress so a large writable socket cannot
                // monopolize the loop. Only the remaining memoryview is sliced;
                // partial sends never copy the unsent payload into Python
                // bytes.
                for _ in 0..16 {
                    if *offset == *len {
                        return Ok(Some(py.None()));
                    }
                    let chunk = if *offset == 0 {
                        data.bind(py).clone()
                    } else {
                        // Export only for this synchronous send. Retaining a
                        // memoryview across the wait would prevent the caller
                        // resizing its buffer immediately after cancellation.
                        // SAFETY: data is a live Python object while attached;
                        // CPython returns a new reference or sets an exception.
                        let view = unsafe {
                            Bound::from_owned_ptr_or_err(
                                py,
                                pyo3::ffi::PyMemoryView_FromObject(data.as_ptr()),
                            )
                        }?;
                        let bytes = view.call_method1("cast", ("B",))?;
                        bytes.get_item(pyo3::types::PySlice::new(
                            py,
                            *offset as isize,
                            *len as isize,
                            1,
                        ))?
                    };
                    match socket.call_method1(py, crate::python_names::send(py), (chunk,)) {
                        Ok(count) => {
                            let count: usize = count.extract(py)?;
                            if count == 0 || count > len.saturating_sub(*offset) {
                                return Err(PyOSError::new_err(
                                    "socket.send returned invalid progress",
                                ));
                            }
                            *offset += count;
                        }
                        Err(error) => return retry_or_error(py, error),
                    }
                }
                return Ok(if *offset == *len {
                    Some(py.None())
                } else {
                    None
                });
            }
        };
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) => retry_or_error(py, error),
        }
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn retry_or_error(py: Python<'_>, error: PyErr) -> PyResult<Option<Py<PyAny>>> {
    if fd_ops::is_retryable_socket_error(py, &error)? {
        Ok(None)
    } else {
        Err(error)
    }
}

#[derive(Default)]
struct WakeState {
    done: AtomicBool,
    waker: AtomicWaker,
    error: Mutex<Option<io::Error>>,
    request: Mutex<Option<Arc<ReadyCallback>>>,
}

#[pyclass(frozen)]
struct CancelWait(Arc<WakeState>);

#[pymethods]
impl CancelWait {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "CancelWait"))]
    fn __call__(&self, _future: &Bound<'_, PyAny>) {
        self.0.done.store(true, Ordering::Release);
        self.0.waker.wake();
    }
}

#[pyclass]
struct SocketOperation {
    core: Arc<LoopCore>,
    socket: Py<PyAny>,
    future: Py<PyAny>,
    action: SocketAction,
    context: Py<PyAny>,
    wake: Arc<WakeState>,
    duplicate: Option<Arc<socket2::Socket>>,
    service_started: bool,
}

#[pymethods]
impl SocketOperation {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "SocketOperation"))]
    fn __call__(slf: Py<Self>, py: Python<'_>) -> PyResult<()> {
        poll_operation(py, slf)
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn start<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    socket: Py<PyAny>,
    mut action: SocketAction,
) -> PyResult<Bound<'py, PyAny>> {
    // An immediate syscall must never block the owning loop. This is also
    // checked for wrapped sockets so their methods retain the same contract.
    if socket
        .call_method0(py, crate::python_names::gettimeout(py))?
        .extract::<Option<f64>>(py)?
        != Some(0.0)
    {
        return Err(PyValueError::new_err("the socket must be non-blocking"));
    }
    let loop_obj = slf.clone_ref(py).into_any();
    let future = if let Some(future) = super::tasks::try_fast_create_future(py, &loop_obj)? {
        future
    } else {
        loop_obj.call_method0(py, "create_future")?
    };
    match action.attempt(py, &socket) {
        Ok(Some(value)) => {
            future.call_method1(py, crate::python_names::set_result(py), (value,))?;
        }
        Err(error) => {
            future.call_method1(
                py,
                crate::python_names::set_exception(py),
                (error.into_value(py),),
            )?;
        }
        Ok(None) => {
            let core = slf.borrow(py).core.clone();
            let (context, _) = crate::context::capture_context(py, None)?;
            let wake = Arc::new(WakeState::default());
            let operation = Py::new(
                py,
                SocketOperation {
                    core: core.clone(),
                    socket,
                    future: future.clone_ref(py),
                    action,
                    context,
                    wake: wake.clone(),
                    duplicate: None,
                    service_started: false,
                },
            )?;
            future.call_method1(
                py,
                crate::python_names::add_done_callback(py),
                (Py::new(py, CancelWait(wake))?,),
            )?;
            if core.on_runtime_thread() {
                arm_wait(py, operation)?;
            } else {
                // run_until_complete(loop.sock_recv(...)) constructs the Future
                // before a runtime exists. Its first retry starts on that run.
                core.schedule_callback(
                    py,
                    CallbackKind::Soon,
                    operation.into_any(),
                    PyTuple::empty(py).unbind(),
                    None,
                )?;
            }
        }
    }
    Ok(future.into_bound(py))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn poll_operation(py: Python<'_>, operation: Py<SocketOperation>) -> PyResult<()> {
    let mut op = operation.borrow_mut(py);
    if op
        .future
        .call_method0(py, crate::python_names::done(py))?
        .extract::<bool>(py)?
    {
        return Ok(());
    }
    let error = op
        .wake
        .error
        .lock()
        .expect("poisoned socket wait error")
        .take();
    let result = if let Some(error) = error {
        Err(PyErr::from(error))
    } else {
        let socket = op.socket.clone_ref(py);
        op.action.attempt(py, &socket)
    };
    match result {
        Ok(Some(value)) => {
            op.future
                .call_method1(py, crate::python_names::set_result(py), (value,))?;
        }
        Err(error) => {
            op.future.call_method1(
                py,
                crate::python_names::set_exception(py),
                (error.into_value(py),),
            )?;
        }
        Ok(None) => {
            drop(op);
            arm_wait(py, operation)?;
        }
    }
    Ok(())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn duplicate_socket(py: Python<'_>, socket: &Py<PyAny>) -> PyResult<socket2::Socket> {
    let fd = fd_ops::fileobj_to_fd(py, socket.bind(py))?;
    #[cfg(unix)]
    let borrowed = {
        use std::os::fd::FromRawFd;
        let fd = i32::try_from(fd).map_err(|_| PyOSError::new_err("invalid socket descriptor"))?;
        // SAFETY: the Python socket owns fd. ManuallyDrop prevents closing it,
        // including when try_clone fails. Only the duplicate becomes owned.
        ManuallyDrop::new(unsafe { socket2::Socket::from_raw_fd(fd) })
    };
    #[cfg(windows)]
    let borrowed = {
        use std::os::windows::io::FromRawSocket;
        // SAFETY: same ownership contract as the Unix branch; RawFd stores
        // SOCKET's bit pattern, which may use the signed integer's high bit.
        ManuallyDrop::new(unsafe { socket2::Socket::from_raw_socket(fd as _) })
    };
    Ok(borrowed.try_clone()?)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn arm_wait(py: Python<'_>, operation: Py<SocketOperation>) -> PyResult<()> {
    let mut op = operation.borrow_mut(py);
    if op.duplicate.is_none() {
        match duplicate_socket(py, &op.socket) {
            Ok(socket) => op.duplicate = Some(Arc::new(socket)),
            Err(error) => {
                op.future.call_method1(
                    py,
                    crate::python_names::set_exception(py),
                    (error.into_value(py),),
                )?;
                return Ok(());
            }
        }
    }
    let duplicate = op
        .duplicate
        .as_ref()
        .expect("initialized wait socket")
        .clone();
    let core = op.core.clone();
    let wake = op.wake.clone();
    let writable = op.action.writable();
    let context = op.context.clone_ref(py);
    let future = op.future.clone_ref(py);
    let start_service = !op.service_started;
    op.service_started = true;
    drop(op);
    let ready = Arc::new(ReadyCallback::from_args(
        core.next_callback_id(),
        CallbackKind::Soon,
        operation.into_any(),
        CallbackArgs::None,
        context,
        true,
    ));
    *wake.request.lock().expect("poisoned socket wait request") = Some(ready);
    wake.waker.wake();
    if start_service
        && !core.spawn_io(wait_socket(
            core.clone(),
            duplicate,
            writable,
            wake.clone(),
            RequestCleanup(wake.clone()),
        ))
    {
        let pending = wake
            .request
            .lock()
            .expect("poisoned socket wait request")
            .take();
        drop(pending);
        future.call_method1(
            py,
            crate::python_names::set_exception(py),
            (PyRuntimeError::new_err("socket wait has no running loop").into_value(py),),
        )?;
    }
    Ok(())
}

/// Break a queued callback -> operation -> wait-state reference cycle even
/// when the service future is cancelled before its first poll.
struct RequestCleanup(Arc<WakeState>);
impl Drop for RequestCleanup {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<RequestCleanup as Drop>")
    )]
    fn drop(&mut self) {
        let pending = self
            .0
            .request
            .lock()
            .expect("poisoned socket wait request")
            .take();
        drop(pending);
    }
}

#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
async fn wait_socket(
    core: Arc<LoopCore>,
    socket: Arc<socket2::Socket>,
    writable: bool,
    wake: Arc<WakeState>,
    _cleanup: RequestCleanup,
) {
    #[cfg(unix)]
    let raw = {
        use std::os::fd::AsRawFd;
        socket.as_raw_fd()
    };
    #[cfg(windows)]
    let raw = {
        use std::os::windows::io::AsRawSocket;
        crate::vibeio::RawOsHandle::Socket(socket.as_raw_socket())
    };
    let interest = if writable {
        mio::Interest::WRITABLE
    } else {
        mio::Interest::READABLE
    };
    let handle = InnerRawHandle::new_with_mode(raw, interest, RegistrationMode::Poll);
    loop {
        let ready = poll_fn(|cx| {
            wake.waker.register(cx.waker());
            if wake.done.load(Ordering::Acquire) {
                return Poll::Ready(None);
            }
            match wake
                .request
                .lock()
                .expect("poisoned socket wait request")
                .take()
            {
                Some(ready) => Poll::Ready(Some(ready)),
                None => Poll::Pending,
            }
        })
        .await;
        let Some(ready) = ready else {
            return;
        };
        let result = match &handle {
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            Ok(handle) => {
                let mut armed = false;
                poll_fn(|cx| {
                    wake.waker.register(cx.waker());
                    if wake.done.load(Ordering::Acquire) {
                        return Poll::Ready(Ok(false));
                    }
                    if armed {
                        return Poll::Ready(Ok(true));
                    }
                    let mut op = if writable {
                        ReadinessOp::new_writable(handle)
                    } else {
                        ReadinessOp::new_readable(handle)
                    };
                    armed = true;
                    handle
                        .poll_op_poll(cx, &mut op)
                        .map(|result| result.map(|()| true))
                })
                .await
            }
        };
        if wake.done.load(Ordering::Acquire) {
            return;
        }
        let failed = result.is_err();
        if let Err(error) = result {
            *wake.error.lock().expect("poisoned socket wait error") = Some(error);
        }
        if core
            .send_command(LoopCommand::ScheduleReady(ready))
            .is_err()
            || failed
        {
            return;
        }
        // The registration and duplicate survive the Python retry. A new
        // request rearms this service; cancellation releases both immediately.
    }
}
