//! Shared RAII guard for the ingest commands' no-network (`--dry-run`) path.
//!
//! Both `ingest-basketwatch` and `ingest-lidl` select their fetch strategy from
//! the process-global `BUYWISE_NO_NETWORK` env var (the adapter entry points take
//! only a pool, with no `no_network` parameter). `--dry-run` forces that variable
//! on for the duration of a single run via [`NoNetworkGuard`], which restores the
//! prior value (or unsets it if it was absent) on drop — on both the success and
//! error paths. Centralising the guard here keeps the subtle restore/unset
//! discipline identical across the two commands.
//!
//! Concurrent calls in the *same* process still race on the shared variable for
//! the duration of a single run, so in-process tests that interleave dry-run and
//! live calls should still serialise themselves under a shared lock.

/// The process-global env var the ingest adapters read to select their fetch
/// strategy. `--dry-run` forces it on (scoped to a [`NoNetworkGuard`]).
pub const NO_NETWORK_VAR: &str = "BUYWISE_NO_NETWORK";

/// RAII guard that forces an ingest adapter's no-network path for the lifetime of
/// a dry-run, restoring `BUYWISE_NO_NETWORK` to its prior value (or unsetting it
/// if it was absent) on drop. This keeps the env mutation scoped to the run
/// rather than leaking it process-wide.
pub struct NoNetworkGuard {
    previous: Option<String>,
}

impl NoNetworkGuard {
    /// Set `BUYWISE_NO_NETWORK=1`, capturing the prior value so it can be restored
    /// when the guard drops.
    pub fn engage() -> Self {
        let previous = std::env::var(NO_NETWORK_VAR).ok();
        // SAFETY: mutating the process environment is `unsafe` in edition 2024
        // because it is not thread-safe. The CLI mutates this var only here,
        // and the guard restores the prior value on drop, so the window in
        // which the global is changed is bounded by a single ingest run.
        unsafe {
            std::env::set_var(NO_NETWORK_VAR, "1");
        }
        Self { previous }
    }
}

impl Drop for NoNetworkGuard {
    fn drop(&mut self) {
        // SAFETY: see `engage` — restoring the previous value (or unsetting it)
        // is the matching mutation that keeps the change scoped to this run.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(NO_NETWORK_VAR, value),
                None => std::env::remove_var(NO_NETWORK_VAR),
            }
        }
    }
}
