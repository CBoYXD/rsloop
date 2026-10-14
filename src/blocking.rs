//! Adapter for blocking operations that cannot run on an async reactor thread.

/// Run blocking work on the shared pool. Panic isolation retains the previous
/// dropped-worker error contract without spawning a thread for every call.
#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
pub async fn run<T, F>(_name: impl Into<String>, task: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    smol::unblock(move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)))
        .await
        .map_err(|_| "blocking worker dropped".to_owned())
}
