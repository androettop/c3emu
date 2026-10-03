//! Wall clock and sleeping. WebAssembly (wasm32-unknown-unknown) has no std::time
//! clock and cannot block: there the clock is the page's performance.now() and a sleep
//! is recorded for the web driver, which yields to the browser for that long.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(not(target_arch = "wasm32"))]
pub fn sleep(d: std::time::Duration) {
    std::thread::sleep(d)
}

/// Native sleeps really sleep: nothing is left pending.
#[cfg(not(target_arch = "wasm32"))]
pub fn take_sleep() -> f64 {
    0.0
}

#[cfg(target_arch = "wasm32")]
pub use web::*;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::Cell;
    use std::time::Duration;

    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        /// Milliseconds from the page (performance.now()).
        fn c3_now_ms() -> f64;
    }

    #[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
    pub struct Instant(f64);

    impl Instant {
        pub fn now() -> Self {
            Instant(unsafe { c3_now_ms() })
        }

        pub fn elapsed(&self) -> Duration {
            Instant::now() - *self
        }
    }

    impl std::ops::Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            Instant(self.0 + d.as_secs_f64() * 1000.0)
        }
    }

    impl std::ops::Sub for Instant {
        type Output = Duration;
        fn sub(self, o: Instant) -> Duration {
            Duration::from_secs_f64(((self.0 - o.0) / 1000.0).max(0.0))
        }
    }

    thread_local! {
        static SLEPT: Cell<f64> = const { Cell::new(0.0) };
    }

    /// Record that the emulator wants to sleep `d` (see take_sleep).
    pub fn sleep(d: Duration) {
        SLEPT.with(|s| s.set(s.get() + d.as_secs_f64() * 1000.0));
    }

    /// Milliseconds of sleep requested since the last call.
    pub fn take_sleep() -> f64 {
        SLEPT.with(|s| s.replace(0.0))
    }
}
