//! Handing freed heap back to the operating system.
//!
//! Rust frees promptly; glibc does not. `free()` returns a block to glibc's own
//! arenas, and glibc only returns those arenas to the kernel when the top of the
//! heap happens to be contiguous free space above `M_TRIM_THRESHOLD`. A big
//! allocation that came from `mmap` goes back at `free()`, but the ONNX Runtime
//! session behind [`crate::embed`] is thousands of medium allocations rather
//! than a few huge ones, so almost none of it qualifies. Dropping the session
//! makes the memory reusable BY THIS PROCESS and invisible to it: RSS barely
//! moves, and RSS is what the pod's memory limit and every dashboard read.
//!
//! Measured on a hosted tenant's pod (Linux glibc, fp32 bge-small), one process
//! from boot:
//!
//! ```text
//!   after start, before any embed      ~25 MB
//!   session loaded                    +195 MB
//!   after one backfill batch pass     +324 MB   (~520 MB resident)
//!   session dropped                   ~126 MB   <- ~400 MB reusable, 0 returned
//!   + malloc_trim(0)                   ~40 MB   <- the rest goes back
//! ```
//!
//! jemalloc was measured as an alternative and does NOT do this on its own
//! (it settled at 132-145 MB after the same drop), so the fix is the explicit
//! trim, not an allocator swap.
//!
//! IT IS WORTH CALLING WITH THE SESSION STILL LOADED, which is not obvious: the
//! big number above comes from dropping the session first, so a trim that
//! cannot do that looks pointless. Measured on the same box, it is not. After a
//! batch-8 pass with the session still resident the trim took fp32 from +324 MB
//! to +290 MB, and after a batch-1 varied pass from +44 MB to +11 MB. That is
//! the ingest transient (flattened bodies, tokenizer scratch) going back, and
//! it is why the sync passes call this without touching the embedder.
//!
//! The call is cheap but not free (it walks the arena free lists), so it belongs
//! at the end of a big, occasional pass: an embedder unload, a backfill, a
//! catch-up, a vector backfill that embedded something. AT MOST ONCE PER POLL
//! TICK, never once per message.

/// Ask the allocator to return free heap to the kernel. A no-op everywhere but
/// glibc, which is the only allocator that both keeps the memory and offers a
/// way to ask for it back; the daemon's pods are Debian trixie, so the one
/// platform where this matters is the one platform it compiles on.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` is a glibc entry point that only walks the
    // allocator's own free lists and releases pages nothing holds. It touches no
    // memory this process can still reach, takes a plain byte pad (0 = keep no
    // slack), and returns 1/0 for "did something" which no caller acts on.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// [`trim`] off the async runtime.
///
/// Walking glibc's arena free lists is millisecond-scale rather than instant,
/// and every caller is inside a poll tick on the daemon's one runtime, so this
/// hops to a blocking thread exactly as the embedder's reaper does. A join
/// error is ignored: the trim is an optimisation and there is nothing sensible
/// to do about a cancelled one.
pub async fn trim_off_runtime() {
    let _ = tokio::task::spawn_blocking(trim).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn trim_is_callable_on_every_platform() {
        // The point of the test is that this LINKS: the cfg split means a
        // non-glibc build takes a different body, and a typo in either arm only
        // shows up when something calls it.
        super::trim();
        super::trim();
    }

    #[tokio::test]
    async fn the_off_runtime_wrapper_links_too() {
        // Same reason as above, one layer out: the `spawn_blocking` hop is the
        // shape every sync call site uses.
        super::trim_off_runtime().await;
    }
}
