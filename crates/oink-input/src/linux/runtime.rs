//! Dedicated Tokio runtime for xdg-desktop-portal async work.

use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

pub(crate) fn handle() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("oink-input-portal")
            .build()
            .expect("failed to create portal runtime")
    })
}

pub(crate) fn block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    // Calling `Runtime::block_on` (or `tokio::task::block_in_place` from an OS
    // thread already driving `handle().block_on(...)`, such as `oink-input-capture`)
    // panics in Tokio when a runtime context is active on the current thread.
    // Waiting via a scoped helper thread works from any Tokio flavor or callback.
    if Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(|| handle().block_on(fut))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    } else {
        handle().block_on(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_block_on_from_capture_driver_thread_does_not_panic() {
        let value = handle().block_on(async { block_on(async { 42 }) });
        assert_eq!(value, 42);
    }
}
