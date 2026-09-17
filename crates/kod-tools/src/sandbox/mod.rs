//! Platform sandbox primitives beyond bwrap / Seatbelt.
//!
//! The `SandboxResolver` in `context.rs` owns the *selection* logic
//! (which backend on which host, plus the invocation shape a caller
//! spawns). This module owns the Landlock implementation — large
//! enough (kernel ABI probing, raw syscalls, the packed attr
//! structs) to deserve its own file, and platform-specific enough
//! that exposing it under a stable `sandbox::landlock::*` path keeps
//! the `#[cfg(target_os = "linux")]` gate from leaking into callers.
//!
//! On non-Linux platforms the `landlock` module is a stub whose
//! `probe_abi` always returns `None`. Callers can therefore write
//! `crate::sandbox::landlock::probe_abi()` unconditionally and let
//! the compiler elide the whole call away on macOS and Windows.

#[cfg(target_os = "linux")]
pub mod landlock;

#[cfg(not(target_os = "linux"))]
pub mod landlock {
    //! Stub for non-Linux platforms.

    /// No Linux kernel, no Landlock.
    pub fn probe_abi() -> Option<u32> {
        None
    }
}
