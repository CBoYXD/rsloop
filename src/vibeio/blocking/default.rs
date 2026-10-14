use super::BlockingThreadPool;

/// A default implementation of `BlockingThreadPool` using `rusty_pool` crate.
pub struct DefaultBlockingThreadPool {
    inner: rusty_pool::ThreadPool,
}

impl DefaultBlockingThreadPool {
    /// Creates a new `DefaultBlockingThreadPool` with the default maximum
    /// number of threads.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "DefaultBlockingThreadPool")
    )]
    #[inline]
    pub fn new() -> Self {
        Self::with_max_threads(512)
    }

    /// Creates a new `DefaultBlockingThreadPool` with the specified maximum
    /// number of threads.
    ///
    /// # Panics
    ///
    /// Panics if the maximum is zero or exceeds the pool's supported size.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "DefaultBlockingThreadPool")
    )]
    #[inline]
    pub fn with_max_threads(num_threads: usize) -> Self {
        Self {
            // rusty_pool's default core size is the CPU count. Cap it too,
            // otherwise a valid small maximum panics on a multicore machine.
            inner: rusty_pool::Builder::new()
                .core_size(
                    std::thread::available_parallelism()
                        .map(usize::from)
                        .unwrap_or(1)
                        .min(num_threads),
                )
                .max_size(num_threads)
                .build(),
        }
    }
}

// rusty_pool's Task conversion can accept an existing boxed closure directly.
// Wrapping it in another closure would allocate a second Job box.
struct PooledJob(Box<dyn FnOnce() + Send + 'static>);
impl rusty_pool::Task<()> for PooledJob {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<PooledJob as rusty_pool :: Task < () >>")
    )]
    fn run(self) {
        (self.0)();
    }
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<PooledJob as rusty_pool :: Task < () >>")
    )]
    fn into_fn(self) -> Option<Box<dyn FnOnce() + Send + 'static>> {
        Some(self.0)
    }
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<PooledJob as rusty_pool :: Task < () >>")
    )]
    fn is_fn(&self) -> bool {
        true
    }
}

impl BlockingThreadPool for DefaultBlockingThreadPool {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<DefaultBlockingThreadPool as BlockingThreadPool>")
    )]
    #[inline]
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        self.inner.execute(PooledJob(task));
    }
}

impl Default for DefaultBlockingThreadPool {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<DefaultBlockingThreadPool as Default>")
    )]
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_worker_pool_executes_tasks_on_multicore_hosts() {
        let pool = DefaultBlockingThreadPool::with_max_threads(1);
        let (send, receive) = std::sync::mpsc::channel();
        pool.spawn(Box::new(move || send.send(42).unwrap()));
        assert_eq!(
            receive
                .recv_timeout(crate::vibeio::test_support::WATCHDOG)
                .unwrap(),
            42
        );
    }
}
