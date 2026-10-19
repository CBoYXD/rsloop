//! Delivering subprocess lifecycle events to the Python protocol.
//!
//! Each callback runs inside the transport's `contextvars.Context` when one was
//! supplied. Text mode is applied here too: `pipe_data_received` decodes bytes
//! with the configured encoding (and translates newlines) before handing them
//! over, so the protocol sees `str` exactly as `subprocess` in text mode would.
//!
//! `connection_lost` is guarded by `connection_lost_called` and by the pipe /
//! exit bookkeeping in `ProcessState`, because both the waiter thread and the
//! last closing pipe can reach this point.

use std::sync::Arc;

use pyo3::prelude::*;

use super::{PendingProcessEvent, ProcessState, ProcessTransportCore, PyProcessTransport};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessExitDecision {
    Duplicate,
    First { should_finish: bool },
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn process_connection_lost_eligible(exited: bool, open_pipes_empty: bool) -> bool {
    exited && open_pipes_empty
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn record_process_exit(
    exited: &mut bool,
    returncode: &mut Option<i32>,
    open_pipes_empty: bool,
    code: i32,
) -> ProcessExitDecision {
    if *exited {
        return ProcessExitDecision::Duplicate;
    }
    *exited = true;
    *returncode = Some(code);
    ProcessExitDecision::First {
        should_finish: process_connection_lost_eligible(true, open_pipes_empty),
    }
}

impl ProcessTransportCore {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn connection_made(&self, transport: Py<PyProcessTransport>) -> PyResult<()> {
        self.call_in_loop_context(|py| {
            self.call_protocol_method1(py, "connection_made", transport.into_any())?;
            Ok(())
        })
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn pipe_data_received_with_py(
        &self,
        py: Python<'_>,
        fd: i32,
        data: &[u8],
    ) -> PyResult<()> {
        let payload = if let Some(text_config) = &self.text_config {
            let decoded = pyo3::types::PyBytes::new(py, data)
                .call_method1("decode", (&text_config.encoding, &text_config.errors))?;
            if text_config.translate_newlines {
                decoded
                    .call_method1("replace", ("\r\n", "\n"))?
                    .call_method1("replace", ("\r", "\n"))?
                    .unbind()
                    .into_any()
            } else {
                decoded.unbind()
            }
        } else {
            pyo3::types::PyBytes::new(py, data).unbind().into_any()
        };
        self.call_protocol_method2(
            py,
            "pipe_data_received",
            fd.into_pyobject(py)?.unbind().into_any(),
            payload,
        )?;
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn pipe_data_received(self: &Arc<Self>, fd: i32, data: &[u8]) -> PyResult<()> {
        if !self.loop_core.on_runtime_thread() {
            self.enqueue_pending_event(PendingProcessEvent::PipeDataReceived {
                fd,
                data: Box::<[u8]>::from(data),
            });
            return Ok(());
        }

        self.call_in_loop_context(|py| self.pipe_data_received_with_py(py, fd, data))
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn pipe_connection_lost_value_with_py(
        &self,
        py: Python<'_>,
        fd: i32,
        exc: Option<PyErr>,
    ) -> PyResult<()> {
        let exc = exc.map(|err| err.value(py).clone().unbind().into_any());
        self.call_protocol_method2(
            py,
            "pipe_connection_lost",
            fd.into_pyobject(py)?.unbind().into_any(),
            exc.unwrap_or_else(|| py.None()),
        )?;
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn pipe_connection_lost_message(
        self: &Arc<Self>,
        fd: i32,
        exc: Option<String>,
    ) -> PyResult<()> {
        let pending_exit_code = {
            let mut state = self.state.lock().expect("poisoned process state");
            if !state.open_pipes.remove(&fd) {
                return Ok(());
            }
            let should_finish =
                process_connection_lost_eligible(state.exited, state.open_pipes.is_empty());
            let pending_exit_code = if fd == 0 {
                state.pending_exit_code.take()
            } else {
                None
            };
            // Publish the event while still holding the lifecycle lock. A
            // second reader must not observe the last pipe as closed and
            // enqueue ConnectionLost ahead of this pipe's EOF notification.
            // Loop-thread stdin callbacks use the same queue too: delivering
            // them inline could overtake an already queued stdout/stderr EOF.
            self.enqueue_pending_event(PendingProcessEvent::PipeConnectionLost { fd, exc });
            if should_finish {
                self.enqueue_connection_lost_locked(&mut state, None);
            }
            pending_exit_code
        };

        if let Some(code) = pending_exit_code {
            self.process_exited(code)?;
        }
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn pipe_connection_lost(
        self: &Arc<Self>,
        fd: i32,
        exc: Option<PyErr>,
    ) -> PyResult<()> {
        let exc = exc.map(|err| Python::attach(|py| err.value(py).to_string()));
        self.pipe_connection_lost_message(fd, exc)
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn process_exited_with_py(&self, py: Python<'_>, returncode: i32) -> PyResult<()> {
        let _ = returncode;
        self.call_protocol_method0(py, "process_exited")?;
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn process_exited(self: &Arc<Self>, returncode: i32) -> PyResult<()> {
        {
            let mut state = self.state.lock().expect("poisoned process state");
            let open_pipes_empty = state.open_pipes.is_empty();
            let mut exited = state.exited;
            let mut stored_returncode = state.returncode;
            let decision = record_process_exit(
                &mut exited,
                &mut stored_returncode,
                open_pipes_empty,
                returncode,
            );
            state.exited = exited;
            state.returncode = stored_returncode;
            let should_finish = match decision {
                ProcessExitDecision::Duplicate => return Ok(()),
                ProcessExitDecision::First { should_finish } => should_finish,
            };
            // Keep state publication and FIFO publication indivisible, just
            // like pipe closure. The last reader can finish concurrently.
            self.enqueue_pending_event(PendingProcessEvent::ProcessExited { returncode });
            if should_finish {
                self.enqueue_connection_lost_locked(&mut state, None);
            }
        }
        self.exit_notify.notify_all();
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn connection_lost_with_py(
        &self,
        py: Python<'_>,
        exc: Option<PyErr>,
    ) -> PyResult<()> {
        let arg = exc
            .map(|err| err.value(py).clone().unbind().into_any())
            .unwrap_or_else(|| py.None());
        let result = self.call_protocol_method1(py, "connection_lost", arg);
        // Subprocess protocols commonly store their transport too. Release
        // references even if the callback fails, outside the state lock so
        // Python finalizers can safely reenter the transport.
        let released = {
            let mut state = self.state.lock().expect("poisoned process state");
            state.context_needs_run = false;
            (
                std::mem::replace(&mut state.protocol, py.None()),
                std::mem::replace(&mut state.context, py.None()),
            )
        };
        drop(released);
        result.map(|_| ())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    fn enqueue_connection_lost_locked(
        self: &Arc<Self>,
        state: &mut ProcessState,
        exc: Option<String>,
    ) {
        if state.connection_lost_called {
            return;
        }
        state.connection_lost_called = true;
        state.closing = true;
        self.enqueue_pending_event(PendingProcessEvent::ConnectionLost { exc });
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn connection_lost_message(self: &Arc<Self>, exc: Option<String>) -> PyResult<()> {
        let mut state = self.state.lock().expect("poisoned process state");
        self.enqueue_connection_lost_locked(&mut state, exc);
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn connection_lost(self: &Arc<Self>, exc: Option<PyErr>) -> PyResult<()> {
        let exc = exc.map(|err| Python::attach(|py| err.value(py).to_string()));
        self.connection_lost_message(exc)
    }
}

#[cfg(kani)]
mod verification {
    use super::{ProcessExitDecision, process_connection_lost_eligible, record_process_exit};

    const PIPE_COUNT: usize = 3;
    const MODEL_OPERATIONS: usize = 6;

    #[kani::proof]
    #[kani::unwind(8)]
    fn extended_subprocess_lifecycle_is_idempotent_and_order_independent() {
        let operations: [u8; MODEL_OPERATIONS] = kani::any();
        let returncodes: [i8; MODEL_OPERATIONS] = kani::any();
        let mut open_pipes = (1_u8 << PIPE_COUNT) - 1;
        let mut pipe_close_count = [0_u8; PIPE_COUNT];
        let mut exited = false;
        let mut returncode = None;
        let mut process_exited_count = 0_u8;
        let mut connection_lost_called = false;

        for index in 0..MODEL_OPERATIONS {
            if operations[index] % 5 < 4 {
                let pipe = usize::from(operations[index] % 5);
                let before = open_pipes;
                if pipe < PIPE_COUNT {
                    let bit = 1_u8 << pipe;
                    if open_pipes & bit != 0 {
                        open_pipes &= !bit;
                        pipe_close_count[pipe] += 1;
                    }
                }
                assert!(open_pipes == before || open_pipes.count_ones() + 1 == before.count_ones());
            } else {
                let before_exited = exited;
                let before_returncode = returncode;
                match record_process_exit(
                    &mut exited,
                    &mut returncode,
                    open_pipes == 0,
                    i32::from(returncodes[index]),
                ) {
                    ProcessExitDecision::Duplicate => {
                        assert!(before_exited);
                        assert_eq!(returncode, before_returncode);
                    }
                    ProcessExitDecision::First { should_finish } => {
                        assert!(!before_exited);
                        process_exited_count += 1;
                        assert_eq!(
                            should_finish,
                            process_connection_lost_eligible(true, open_pipes == 0)
                        );
                    }
                }
            }

            for count in pipe_close_count {
                assert!(count <= 1);
            }
            assert!(process_exited_count <= 1);
            let eligible = process_connection_lost_eligible(exited, open_pipes == 0);
            if eligible {
                connection_lost_called = true;
            }
            assert_eq!(eligible, exited && open_pipes == 0);
            assert!(!connection_lost_called || eligible);
        }

        assert_eq!(process_exited_count, u8::from(exited));
        if exited {
            assert!(returncode.is_some());
        } else {
            assert!(returncode.is_none());
        }
        kani::cover!(exited && open_pipes != 0);
        kani::cover!(connection_lost_called);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    };

    use pyo3::{prelude::*, types::PyModule};

    use super::{ProcessExitDecision, process_connection_lost_eligible, record_process_exit};
    use crate::{
        async_event::AsyncEvent,
        engine::LoopCore,
        transport::process::{ProcessState, ProcessTransportCore},
    };

    #[test]
    fn loop_thread_process_exit_cannot_overtake_queued_pipe_loss() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class Protocol:\n    def __init__(self): self.events = []\n    def pipe_connection_lost(self, fd, exc): self.events.append(fd)\n    def process_exited(self): self.events.append('exited')\n    def connection_lost(self, exc): self.events.append('lost')\n",
                c"test_process_events.py",
                c"test_process_events",
            )
            .unwrap();
            let protocol = module
                .getattr("Protocol")
                .unwrap()
                .call0()
                .unwrap()
                .unbind();
            let loop_core = LoopCore::new();
            let (control_tx, _control_rx) = mpsc::channel();
            let core = Arc::new(ProcessTransportCore {
                loop_core: Arc::clone(&loop_core),
                loop_obj: py.None(),
                state: Mutex::new(ProcessState {
                    protocol: protocol.clone_ref(py),
                    context: py.None(),
                    context_needs_run: false,
                    pid: 0,
                    returncode: None,
                    closing: false,
                    exited: false,
                    pending_exit_code: Some(0),
                    connection_lost_called: false,
                    open_pipes: HashSet::from([0, 2]),
                    pipe_transports: HashMap::new(),
                }),
                text_config: None,
                control_tx,
                exit_notify: AsyncEvent::new(),
                pending_events: Mutex::new(VecDeque::new()),
                events_scheduled: AtomicBool::new(false),
            });

            // A reader thread has queued stderr EOF, but the loop has not
            // drained it when the stdin writer's connection_lost runs.
            core.pipe_connection_lost_message(2, None).unwrap();
            loop_core.mark_runtime_thread();
            core.pipe_connection_lost_message(0, None).unwrap();
            let before_drain = protocol
                .getattr(py, "events")
                .unwrap()
                .bind(py)
                .len()
                .unwrap();
            core.drain_pending_events_with_py(py).unwrap();
            let events = protocol
                .getattr(py, "events")
                .unwrap()
                .bind(py)
                .repr()
                .unwrap()
                .to_string();

            loop_core.clear_runtime_thread();
            drop(core);
            loop_core.close().unwrap();
            assert_eq!(before_drain, 0, "lifecycle callbacks must share one FIFO");
            assert_eq!(events, "[2, 0, 'exited', 'lost']");
        });
    }

    #[test]
    fn duplicate_process_exit_keeps_the_first_returncode() {
        let mut exited = false;
        let mut returncode = None;
        assert_eq!(
            record_process_exit(&mut exited, &mut returncode, false, 7),
            ProcessExitDecision::First {
                should_finish: false
            }
        );
        assert_eq!(
            record_process_exit(&mut exited, &mut returncode, true, 99),
            ProcessExitDecision::Duplicate
        );
        assert_eq!(returncode, Some(7));
        assert!(process_connection_lost_eligible(exited, true));
    }
}
