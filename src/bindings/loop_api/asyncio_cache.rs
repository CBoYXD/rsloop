//! Cached lookups into the `asyncio` module.
//!
//! `Task`, `Future`, and `_get_running_loop` are resolved once per process and
//! reused, because the loop touches them on the hottest paths (`create_task`
//! runs for every coroutine step). The keyword-name tuples exist for the same
//! reason: `PyObject_Vectorcall` needs a `kwnames` tuple, and rebuilding it per
//! call would allocate on every task creation.

#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
use pyo3::types::PyTuple;
use pyo3::{prelude::*, sync::PyOnceLock, types::PyModule};

use super::ffi_helpers;
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
use crate::python_names;

macro_rules! cached_function {
    ($name:ident, $module:literal, $attribute:literal) => {
        #[cfg_attr(feature = "profile", hotpath::measure)]
        pub(super) fn $name(py: Python<'_>) -> PyResult<&Py<PyAny>> {
            static CACHE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
            CACHE.get_or_try_init(py, || Ok(py.import($module)?.getattr($attribute)?.unbind()))
        }
    };
}
cached_function!(isfuture_fn, "asyncio", "isfuture");
cached_function!(ensure_future_fn, "asyncio", "ensure_future");
cached_function!(get_asyncgen_hooks_fn, "sys", "get_asyncgen_hooks");
cached_function!(set_asyncgen_hooks_fn, "sys", "set_asyncgen_hooks");
cached_function!(
    firstiter_hook_fn,
    "rsloop._loop",
    "_asyncgen_firstiter_hook"
);
cached_function!(
    finalizer_hook_fn,
    "rsloop._loop",
    "_asyncgen_finalizer_hook"
);
cached_function!(future_done_stop_fn, "rsloop._loop", "future_done_stop");

#[pyfunction]
fn loop_reference_gone(_reference: &Bound<'_, PyAny>) {}

#[pyclass(module = "rsloop._loop")]
struct LoopBoundCallback {
    loop_ref: Py<pyo3::types::PyWeakrefReference>,
    function: Py<PyAny>,
}
#[pymethods]
impl LoopBoundCallback {
    fn __call__(&self, py: Python<'_>, arg: &Bound<'_, PyAny>) -> PyResult<()> {
        use pyo3::types::PyWeakrefMethods;
        if let Some(loop_obj) = self.loop_ref.bind(py).upgrade() {
            self.function.call1(py, (loop_obj, arg))?;
        }
        Ok(())
    }
}

pub(super) fn loop_bound_callback(
    py: Python<'_>,
    core: &crate::engine::LoopCore,
    loop_obj: &Py<PyAny>,
    index: usize,
) -> PyResult<Py<PyAny>> {
    if let Some(callbacks) = &core
        .state
        .lock()
        .expect("poisoned loop state")
        .bound_callbacks
    {
        return Ok(callbacks[index].clone_ref(py));
    }
    static WEAKREF_CALLBACK: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let weakref_callback = WEAKREF_CALLBACK.get_or_try_init(py, || {
        Ok::<_, PyErr>(
            pyo3::wrap_pyfunction!(loop_reference_gone, py)?
                .unbind()
                .into_any(),
        )
    })?;
    let functions = [
        firstiter_hook_fn(py)?,
        finalizer_hook_fn(py)?,
        future_done_stop_fn(py)?,
    ];
    let mut callbacks = Vec::with_capacity(3);
    for function in functions {
        callbacks.push(
            Py::new(
                py,
                LoopBoundCallback {
                    loop_ref: pyo3::types::PyWeakrefReference::new_with(
                        loop_obj.bind(py),
                        weakref_callback.bind(py),
                    )?
                    .unbind(),
                    function: function.clone_ref(py),
                },
            )?
            .into_any(),
        );
    }
    let callbacks: [Py<PyAny>; 3] = callbacks.try_into().ok().expect("three callbacks");
    let mut state = core.state.lock().expect("poisoned loop state");
    let callbacks = state.bound_callbacks.get_or_insert(callbacks);
    Ok(callbacks[index].clone_ref(py))
}

pub(super) fn ensure_future<'py>(
    py: Python<'py>,
    future: &Py<PyAny>,
    loop_obj: &Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    static KEYWORDS: PyOnceLock<Py<pyo3::types::PyTuple>> = PyOnceLock::new();
    let keywords = KEYWORDS.get_or_try_init(py, || {
        pyo3::types::PyTuple::new(py, [pyo3::intern!(py, "loop")]).map(Bound::unbind)
    })?;
    let args = [future.as_ptr(), loop_obj.as_ptr()];
    // SAFETY: callable, values and keyword tuple are live under Python. One
    // positional and one keyword value match the two-element argument array.
    unsafe {
        Bound::from_owned_ptr_or_err(
            py,
            pyo3::ffi::PyObject_Vectorcall(
                ensure_future_fn(py)?.as_ptr(),
                args.as_ptr(),
                1,
                keywords.as_ptr(),
            ),
        )
    }
}

struct PythonApiCaches {
    asyncio_task_cls: PyOnceLock<Py<PyAny>>,
    asyncio_future_cls: PyOnceLock<Py<PyAny>>,
    asyncio_get_running_loop_fn: PyOnceLock<Py<PyAny>>,
    asyncio_task_kwarg_support: PyOnceLock<TaskKwargSupport>,
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    asyncio_future_loop_kwnames: PyOnceLock<Py<PyTuple>>,
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    asyncio_task_loop_kwnames: PyOnceLock<Py<PyTuple>>,
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    asyncio_task_loop_name_kwnames: PyOnceLock<Py<PyTuple>>,
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    asyncio_task_loop_context_kwnames: PyOnceLock<Py<PyTuple>>,
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    asyncio_task_loop_name_context_kwnames: PyOnceLock<Py<PyTuple>>,
}

impl PythonApiCaches {
    const fn new() -> Self {
        Self {
            asyncio_task_cls: PyOnceLock::new(),
            asyncio_future_cls: PyOnceLock::new(),
            asyncio_get_running_loop_fn: PyOnceLock::new(),
            asyncio_task_kwarg_support: PyOnceLock::new(),
            #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
            asyncio_future_loop_kwnames: PyOnceLock::new(),
            #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
            asyncio_task_loop_kwnames: PyOnceLock::new(),
            #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
            asyncio_task_loop_name_kwnames: PyOnceLock::new(),
            #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
            asyncio_task_loop_context_kwnames: PyOnceLock::new(),
            #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
            asyncio_task_loop_name_context_kwnames: PyOnceLock::new(),
        }
    }
}

static PYTHON_API_CACHES: PythonApiCaches = PythonApiCaches::new();

/// Which keyword-only parameters this interpreter's `asyncio.Task` accepts.
pub(super) struct TaskKwargSupport {
    pub(super) name: bool,
    pub(super) context: bool,
    pub(super) eager_start: bool,
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn asyncio_task_cls(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    PYTHON_API_CACHES
        .asyncio_task_cls
        .get_or_try_init(py, || Ok(py.import("asyncio")?.getattr("Task")?.unbind()))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn asyncio_future_cls(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    PYTHON_API_CACHES
        .asyncio_future_cls
        .get_or_try_init(py, || Ok(py.import("asyncio")?.getattr("Future")?.unbind()))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn asyncio_get_running_loop_fn(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    PYTHON_API_CACHES
        .asyncio_get_running_loop_fn
        .get_or_try_init(py, || {
            Ok(py
                .import("asyncio.events")?
                .getattr("_get_running_loop")?
                .unbind())
        })
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn asyncio_task_kwarg_support(py: Python<'_>) -> PyResult<&'static TaskKwargSupport> {
    PYTHON_API_CACHES
        .asyncio_task_kwarg_support
        .get_or_try_init(py, || detect_asyncio_task_kwarg_support(py))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn detect_asyncio_task_kwarg_support(py: Python<'_>) -> PyResult<TaskKwargSupport> {
    let inspect = py.import("inspect")?;
    let Some(signature) = asyncio_task_signature(py, &inspect)? else {
        return Ok(TaskKwargSupport {
            name: true,
            context: false,
            eager_start: false,
        });
    };
    let parameters = signature.getattr("parameters")?;
    let keyword_only = inspect.getattr("Parameter")?.getattr("KEYWORD_ONLY")?;
    let mut support = TaskKwargSupport {
        name: false,
        context: false,
        eager_start: false,
    };

    for kwarg_name in ["name", "context", "eager_start"] {
        if has_keyword_only_parameter(&parameters, &keyword_only, kwarg_name)? {
            mark_task_kwarg_supported(&mut support, kwarg_name);
        }
    }

    Ok(support)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn asyncio_task_signature<'py>(
    py: Python<'py>,
    inspect: &Bound<'py, PyModule>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    match inspect
        .getattr("signature")?
        .call1((asyncio_task_cls(py)?.clone_ref(py),))
    {
        Ok(signature) => Ok(Some(signature)),
        Err(_) => Ok(None),
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn has_keyword_only_parameter(
    parameters: &Bound<'_, PyAny>,
    keyword_only: &Bound<'_, PyAny>,
    kwarg_name: &str,
) -> PyResult<bool> {
    let Ok(parameter) = parameters.get_item(kwarg_name) else {
        return Ok(false);
    };
    parameter.getattr("kind")?.eq(keyword_only)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn mark_task_kwarg_supported(support: &mut TaskKwargSupport, kwarg_name: &str) {
    match kwarg_name {
        "name" => support.name = true,
        "context" => support.context = true,
        "eager_start" => support.eager_start = true,
        _ => {}
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
pub(super) fn call_callable_noargs(py: Python<'_>, callable: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    ffi_helpers::call_noargs(py, callable)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
pub(super) fn call_callable_onearg(
    py: Python<'_>,
    callable: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    ffi_helpers::call_onearg(py, callable, arg)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
fn keyword_tuple<const N: usize>(
    slot: &'static PyOnceLock<Py<PyTuple>>,
    py: Python<'_>,
    names: [&Bound<'_, pyo3::types::PyString>; N],
) -> PyResult<&'static Py<PyTuple>> {
    slot.get_or_try_init(py, || Ok(PyTuple::new(py, names)?.unbind()))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(super) fn asyncio_future_loop_kwnames(py: Python<'_>) -> PyResult<&Py<PyTuple>> {
    keyword_tuple(
        &PYTHON_API_CACHES.asyncio_future_loop_kwnames,
        py,
        [python_names::loop_kw(py)],
    )
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(super) fn asyncio_task_kwnames_for_options(
    py: Python<'_>,
    include_name: bool,
    include_context: bool,
) -> PyResult<&Py<PyTuple>> {
    match (include_name, include_context) {
        (false, false) => keyword_tuple(
            &PYTHON_API_CACHES.asyncio_task_loop_kwnames,
            py,
            [python_names::loop_kw(py)],
        ),
        (true, false) => keyword_tuple(
            &PYTHON_API_CACHES.asyncio_task_loop_name_kwnames,
            py,
            [python_names::loop_kw(py), python_names::name_kw(py)],
        ),
        (false, true) => keyword_tuple(
            &PYTHON_API_CACHES.asyncio_task_loop_context_kwnames,
            py,
            [python_names::loop_kw(py), python_names::context_kw(py)],
        ),
        (true, true) => keyword_tuple(
            &PYTHON_API_CACHES.asyncio_task_loop_name_context_kwnames,
            py,
            [
                python_names::loop_kw(py),
                python_names::name_kw(py),
                python_names::context_kw(py),
            ],
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;

    #[test]
    fn python_api_caches_are_safe_under_concurrent_access() {
        const THREADS: usize = 8;

        crate::initialize_python_for_tests();

        let barrier = Arc::new(Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..25 {
                        Python::attach(|py| {
                            assert!(
                                asyncio_task_cls(py)
                                    .expect("asyncio.Task")
                                    .bind(py)
                                    .is_callable()
                            );
                            assert!(
                                asyncio_future_cls(py)
                                    .expect("asyncio.Future")
                                    .bind(py)
                                    .is_callable()
                            );
                            assert!(
                                asyncio_get_running_loop_fn(py)
                                    .expect("asyncio._get_running_loop")
                                    .bind(py)
                                    .is_callable()
                            );
                            let _ = asyncio_task_kwarg_support(py).expect("Task keyword support");

                            #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
                            {
                                assert_eq!(
                                    asyncio_future_loop_kwnames(py)
                                        .expect("Future keyword names")
                                        .bind(py)
                                        .len(),
                                    1
                                );
                                assert_eq!(
                                    asyncio_task_kwnames_for_options(py, true, true)
                                        .expect("Task keyword names")
                                        .bind(py)
                                        .len(),
                                    3
                                );
                            }
                        });
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("cache worker panicked");
        }
    }
}
