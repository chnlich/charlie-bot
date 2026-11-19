//! The server's global heap allocator.
//!
//! mimalloc returns freed pages to the OS, so the server's resident memory follows its live data. glibc keeps freed
//! chunks inside its per-thread arenas: on 2026-11-17 the server held 31.5 GiB of RSS. `dev/alloc-probe` runs one
//! append workload on each allocator and reports the RSS after the workload goes quiet.
//!
//! The `use-mimalloc` feature selects mimalloc and is on by default. A build with `--no-default-features` links glibc
//! malloc, and `configure` does nothing.

#[cfg(feature = "use-mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The `mi_option_purge_delay` index in mimalloc's `mi_option_e` (`c_src/mimalloc/v3/include/mimalloc.h` of
/// libmimalloc-sys 0.1.49). The option has no Rust constant in the bindings.
#[cfg(feature = "use-mimalloc")]
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

/// Sets mimalloc's purge delay to zero, so freed pages return to the OS at once instead of after its default 1000 ms.
/// Call it first in `main`, before the server starts any thread.
#[cfg(feature = "use-mimalloc")]
pub fn configure() {
  // SAFETY: mi_option_set writes one entry of mimalloc's option table; it takes no pointer and allocates nothing.
  unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) }
}

#[cfg(not(feature = "use-mimalloc"))]
pub fn configure() {}
