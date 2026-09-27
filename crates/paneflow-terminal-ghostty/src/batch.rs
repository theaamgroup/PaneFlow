//! Batched multi-key reads over libghostty's `*_get_multi` entry points.
//!
//! Every libghostty handle that exposes a keyed `get` also exposes a
//! `get_multi` with the same shape: an array of keys and a parallel array of
//! destination pointers, filled in one call. One round trip instead of one
//! per field matters on the snapshot path, which runs per cell per frame.
//!
//! The slot count is a const parameter so the key and pointer arrays live on
//! the stack: a heap allocation per cell would cost more than the FFI calls
//! this saves.
//!
//! A non-success result is a runtime FFI failure. Success that writes fewer
//! values than requested breaks the batch contract and is an ABI mismatch.

use std::ffi::c_void;

use paneflow_libghostty_sys as sys;

use crate::{GhosttyError, Result};

/// One key and the storage its value lands in.
#[derive(Clone, Copy)]
pub(crate) struct Slot<K: Copy> {
    key: K,
    value: *mut c_void,
}

impl<K: Copy> Slot<K> {
    /// Bind `key` to `destination`.
    ///
    /// # Safety
    ///
    /// `destination` must have exactly the output type libghostty documents
    /// for `key`, and must stay live until the batch call returns.
    pub(crate) unsafe fn new<T>(key: K, destination: &mut T) -> Self {
        Self {
            key,
            value: (destination as *mut T).cast(),
        }
    }
}

/// The `get_multi` shape shared by every keyed libghostty handle.
pub(crate) type GetMultiFn<H, K> =
    unsafe extern "C" fn(H, usize, *const K, *mut *mut c_void, *mut usize) -> sys::GhosttyResult;

/// Run one batched read.
///
/// # Safety
///
/// `handle` must be live, `call` must be the `get_multi` belonging to that
/// handle's key type, and every slot must satisfy [`Slot::new`]'s contract.
pub(crate) unsafe fn get_multi<H: Copy, K: Copy + std::fmt::Debug, const N: usize>(
    operation: &'static str,
    handle: H,
    call: GetMultiFn<H, K>,
    slots: [Slot<K>; N],
) -> Result<()> {
    if N == 0 {
        return Ok(());
    }
    let keys = slots.map(|slot| slot.key);
    let mut values = slots.map(|slot| slot.value);
    let mut written = 0usize;
    // SAFETY: the two arrays hold exactly `N` entries each, the caller
    // guarantees the handle and destination types, and `written` is valid
    // writable storage.
    let result = unsafe { call(handle, N, keys.as_ptr(), values.as_mut_ptr(), &mut written) };
    if result != sys::GhosttyResult_GHOSTTY_SUCCESS {
        return Err(GhosttyError::Ffi {
            operation,
            code: result,
        });
    }
    if written != N {
        return Err(GhosttyError::AbiMismatch(format!(
            "{operation} wrote {written} of {N} values"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn invalid_value(
        _: usize,
        _: usize,
        _: *const i32,
        _: *mut *mut c_void,
        written: *mut usize,
    ) -> sys::GhosttyResult {
        unsafe {
            *written = 0;
        }
        sys::GhosttyResult_GHOSTTY_INVALID_VALUE
    }

    #[test]
    fn get_multi_reports_runtime_failures_as_ffi() {
        let mut destination = 0i32;
        // SAFETY: the stub ignores the dummy handle and the destination, and
        // `destination` stays live until the call returns.
        let error = unsafe {
            get_multi(
                "stub_get_multi",
                0usize,
                invalid_value,
                [Slot::new(1, &mut destination)],
            )
        }
        .expect_err("an invalid value must fail the batch");
        assert!(matches!(error, GhosttyError::Ffi { .. }));
    }
}
