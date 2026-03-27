// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! True concurrent multi-transfer USB bulk reader.
//!
//! Mirrors librtlsdr's `rtlsdr_read_async`: all N transfers are submitted
//! simultaneously via `libusb_submit_transfer`, and each completion callback
//! immediately resubmits the same transfer.  At all times N-1 or N USB bulk
//! transfers are in-flight, so the RTL-SDR hardware FIFO never sees a gap
//! and cannot overflow between transfers.
//!
//! The single-transfer approach (`read_sync` loop) leaves the bus idle while
//! the CPU allocates a Vec and enqueues it — typically 100–500 µs per chunk —
//! which is long enough for the 280 KB/s hardware FIFO to overflow at 2 MS/s.

use crate::error::Result;
use libc::timeval;
use libusb1_sys::{
    constants::*, libusb_alloc_transfer, libusb_cancel_transfer, libusb_context,
    libusb_device_handle, libusb_fill_bulk_transfer, libusb_free_transfer,
    libusb_handle_events_timeout, libusb_submit_transfer, libusb_transfer,
};
use log::{debug, warn};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

// ---------------------------------------------------------------------------
// Shared state passed as `user_data` through every libusb transfer callback.
// All callback invocations happen on the event-loop thread (the same thread
// that calls `libusb_handle_events_timeout`), so interior mutability via
// Atomics is sufficient and no mutex contention occurs.
// ---------------------------------------------------------------------------

pub(crate) struct TransferState {
    pub tx: mpsc::SyncSender<Result<Vec<u8>>>,
    pub stop: Arc<AtomicBool>,
    /// Total buffer count — used as the error-count threshold for dev_lost.
    pub buf_num: usize,
    xfer_errors: AtomicUsize,
    pub dev_lost: AtomicBool,
    /// Number of in-flight transfers that have not yet fired their callback
    /// (or have been resubmitted).  When this reaches zero after we have
    /// set `stop`, it is safe to free the transfer structs and buffers.
    pub pending: AtomicUsize,
}

// Safety: TransferState is only ever accessed from the single event-loop
// thread; the Atomics handle the stop/dev_lost flags written from outside.
unsafe impl Send for TransferState {}
unsafe impl Sync for TransferState {}

// ---------------------------------------------------------------------------
// libusb completion callback — called on the event-loop thread
// ---------------------------------------------------------------------------

pub(crate) extern "system" fn transfer_callback(transfer: *mut libusb_transfer) {
    // Safety: `transfer` is a valid pointer provided by libusb; `user_data`
    // is a `*const TransferState` that outlives the event loop.
    unsafe {
        let xfer = &*transfer;
        let state = &*(xfer.user_data as *const TransferState);

        match xfer.status {
            s if s == LIBUSB_TRANSFER_COMPLETED => {
                // Deliver data to consumer.  try_send is non-blocking: if the
                // consumer falls behind we drop the chunk rather than stalling
                // the event loop (which would delay resubmission and cause FIFO
                // overflow — exactly what we are trying to avoid).
                let data = std::slice::from_raw_parts(
                    xfer.buffer as *const u8,
                    xfer.actual_length as usize,
                );
                if state.tx.try_send(Ok(data.to_vec())).is_err() {
                    debug!(
                        "async_transfer: consumer slow, dropped chunk ({} bytes)",
                        xfer.actual_length
                    );
                }
                state.xfer_errors.store(0, Ordering::Relaxed);

                if !state.stop.load(Ordering::Relaxed) {
                    if libusb_submit_transfer(transfer) != 0 {
                        warn!("async_transfer: resubmit failed");
                        state.pending.fetch_sub(1, Ordering::Relaxed);
                    }
                    // pending unchanged — transfer resubmitted
                } else {
                    state.pending.fetch_sub(1, Ordering::Relaxed);
                }
            }
            s if s == LIBUSB_TRANSFER_CANCELLED => {
                state.pending.fetch_sub(1, Ordering::Relaxed);
            }
            s if s == LIBUSB_TRANSFER_NO_DEVICE => {
                warn!("async_transfer: device lost");
                state.dev_lost.store(true, Ordering::Relaxed);
                state.pending.fetch_sub(1, Ordering::Relaxed);
            }
            other => {
                let errs = state.xfer_errors.fetch_add(1, Ordering::Relaxed) + 1;
                if errs >= state.buf_num {
                    warn!("async_transfer: too many errors ({}), device lost", errs);
                    state.dev_lost.store(true, Ordering::Relaxed);
                }
                if !state.stop.load(Ordering::Relaxed) && !state.dev_lost.load(Ordering::Relaxed) {
                    if libusb_submit_transfer(transfer) != 0 {
                        state.pending.fetch_sub(1, Ordering::Relaxed);
                    }
                } else {
                    warn!("async_transfer: transfer error status={}", other);
                    state.pending.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point — runs the event loop on the calling thread.
// Call this from a dedicated std::thread.
// Returns when `stop` is set or the device is lost.
// ---------------------------------------------------------------------------

/// Run the concurrent-transfer USB streaming loop.
///
/// - `ctx` / `dev_handle` — raw libusb pointers; must be valid for the
///   lifetime of this call (i.e. the owning `RtlSdr` must live at least as
///   long as this function).
/// - `buf_num` — number of concurrent transfers (default: 15).
/// - `buf_len` — bytes per transfer (default: 262144; must be a multiple of 512).
/// - `stop` — set to `true` from another thread to request graceful shutdown.
///
/// Returns the channel receiver from which callers read raw CU8 chunks.
pub(crate) fn run(
    ctx: *mut libusb_context,
    dev_handle: *mut libusb_device_handle,
    buf_num: usize,
    buf_len: usize,
    stop: Arc<AtomicBool>,
    tx: mpsc::SyncSender<Result<Vec<u8>>>,
) {
    // -----------------------------------------------------------------------
    // Allocate transfer structs and data buffers
    // -----------------------------------------------------------------------
    let mut xfers: Vec<*mut libusb_transfer> = Vec::with_capacity(buf_num);
    // Keep the backing Vecs alive so their data pointers stay valid.
    let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(buf_num);

    for _ in 0..buf_num {
        let xfer = unsafe { libusb_alloc_transfer(0) };
        if xfer.is_null() {
            warn!("async_transfer: libusb_alloc_transfer failed");
            // Clean up what we allocated so far and abort.
            for &x in &xfers {
                unsafe { libusb_free_transfer(x) };
            }
            let _ = tx.try_send(Err(crate::error::RtlsdrError::RtlsdrErr(
                "libusb_alloc_transfer failed".into(),
            )));
            return;
        }
        xfers.push(xfer);
        bufs.push(vec![0u8; buf_len]);
    }

    // -----------------------------------------------------------------------
    // Build callback state on the heap; pin it for the duration of streaming.
    // -----------------------------------------------------------------------
    let state = Box::new(TransferState {
        tx,
        stop: stop.clone(),
        buf_num,
        xfer_errors: AtomicUsize::new(0),
        dev_lost: AtomicBool::new(false),
        pending: AtomicUsize::new(buf_num),
    });
    let state_ptr: *mut TransferState = Box::into_raw(state);

    // -----------------------------------------------------------------------
    // Fill and submit all N transfers simultaneously
    // -----------------------------------------------------------------------
    let mut submitted = 0usize;
    for i in 0..buf_num {
        unsafe {
            libusb_fill_bulk_transfer(
                xfers[i],
                dev_handle,
                0x81, // bulk IN endpoint
                bufs[i].as_mut_ptr(),
                buf_len as _,
                transfer_callback,
                state_ptr as *mut c_void,
                0, // no timeout — let the event loop control pacing
            );
            let rc = libusb_submit_transfer(xfers[i]);
            if rc != 0 {
                warn!("async_transfer: submit {} failed (rc={})", i, rc);
                // Decrement pending for this transfer since its callback will
                // never fire.
                (*state_ptr).pending.fetch_sub(1, Ordering::Relaxed);
            } else {
                submitted += 1;
            }
        }
    }

    if submitted == 0 {
        unsafe {
            let _ = Box::from_raw(state_ptr);
        }
        for &x in &xfers {
            unsafe { libusb_free_transfer(x) };
        }
        return;
    }

    // -----------------------------------------------------------------------
    // Event loop — process USB completions until stopped or device lost
    // -----------------------------------------------------------------------
    let tv_1s = timeval {
        tv_sec: 1,
        tv_usec: 0,
    };

    loop {
        unsafe {
            libusb_handle_events_timeout(ctx, &tv_1s);
        }
        let state_ref = unsafe { &*state_ptr };
        if stop.load(Ordering::Relaxed) || state_ref.dev_lost.load(Ordering::Relaxed) {
            break;
        }
    }

    // -----------------------------------------------------------------------
    // Graceful shutdown: cancel all in-flight transfers and drain callbacks
    // -----------------------------------------------------------------------
    // Signal callbacks to not resubmit.
    stop.store(true, Ordering::Relaxed);

    // Cancel every transfer.  libusb_cancel_transfer is asynchronous — the
    // actual CANCELLED callbacks come via the event loop below.
    for &xfer in &xfers {
        unsafe {
            libusb_cancel_transfer(xfer);
        }
    }

    // Drain until all pending callbacks have fired.
    let tv_100ms = timeval {
        tv_sec: 0,
        tv_usec: 100_000,
    };
    let mut drain_iters = 0u32;
    loop {
        let pending = unsafe { (*state_ptr).pending.load(Ordering::Relaxed) };
        if pending == 0 {
            break;
        }
        // Safety net: after 5 seconds of draining, give up.
        if drain_iters >= 50 {
            warn!(
                "async_transfer: shutdown timeout, {} transfers still pending",
                pending
            );
            break;
        }
        unsafe {
            libusb_handle_events_timeout(ctx, &tv_100ms);
        }
        drain_iters += 1;
    }

    // -----------------------------------------------------------------------
    // Free transfer structs (buffers are freed when `bufs` drops below)
    // -----------------------------------------------------------------------
    for &xfer in &xfers {
        unsafe { libusb_free_transfer(xfer) };
    }

    // Recover and drop the callback state.
    unsafe {
        drop(Box::from_raw(state_ptr));
    }

    // `bufs` drops here, freeing all data buffers.
}
