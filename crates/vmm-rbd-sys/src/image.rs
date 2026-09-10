//! An open RBD image, with the asynchronous I/O the storage engine needs.
//!
//! §5.4 wants submit/poll rather than blocking calls, which maps onto
//! librbd's `rbd_aio_*` family: each submission allocates a completion, and
//! polling checks which have finished. Completions are matched back to
//! requests by the engine's tag, carried here alongside the handle.

use crate::cluster::{cstring, IoCtx};
use crate::error::{check, RbdError, Result};
use crate::raw;
use std::sync::Arc;

/// An RBD image opened read-write.
pub struct Image {
    handle: raw::rbd_image_t,
    name: String,
    ioctx: Arc<IoCtx>,
}

// SAFETY: librbd images are thread-safe; the library serialises internally.
unsafe impl Send for Image {}
unsafe impl Sync for Image {}

impl Image {
    /// Open an image, or a snapshot of it when `snapshot` is given.
    pub fn open(ioctx: &Arc<IoCtx>, name: &str, snapshot: Option<&str>) -> Result<Self> {
        let c_name = cstring(name, "image name")?;
        let c_snapshot = snapshot.map(|s| cstring(s, "snapshot name")).transpose()?;
        let snapshot_ptr = c_snapshot
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(core::ptr::null());

        let mut handle: raw::rbd_image_t = core::ptr::null_mut();
        // SAFETY: the io context is open, both names are valid C strings or
        // null, and handle is a live out-parameter.
        let rc =
            unsafe { raw::rbd_open(ioctx.as_ptr(), c_name.as_ptr(), &mut handle, snapshot_ptr) };
        check("rbd_open", &format!("{}/{name}", ioctx.pool()), rc)?;

        Ok(Image {
            handle,
            name: name.to_string(),
            ioctx: Arc::clone(ioctx),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn pool(&self) -> &str {
        self.ioctx.pool()
    }

    /// Image size in bytes.
    pub fn size(&self) -> Result<u64> {
        let mut size: u64 = 0;
        // SAFETY: the image is open and size is a live out-parameter.
        let rc = unsafe { raw::rbd_get_size(self.handle, &mut size) };
        check("rbd_get_size", &self.name, rc)?;
        Ok(size)
    }

    /// Object size in bytes — the natural I/O granularity of the image.
    pub fn object_size(&self) -> Result<u64> {
        let mut info = raw::rbd_image_info_t::default();
        // SAFETY: info is a live struct and the size passed matches it.
        let rc = unsafe {
            raw::rbd_stat(
                self.handle,
                &mut info,
                core::mem::size_of::<raw::rbd_image_info_t>(),
            )
        };
        check("rbd_stat", &self.name, rc)?;
        Ok(info.obj_size)
    }

    /// Take a native RBD snapshot (§10.2).
    pub fn snapshot(&self, snapshot: &str) -> Result<()> {
        let c_snapshot = cstring(snapshot, "snapshot name")?;
        // SAFETY: the image is open and the name is a valid C string.
        let rc = unsafe { raw::rbd_snap_create(self.handle, c_snapshot.as_ptr()) };
        check("rbd_snap_create", &format!("{}@{snapshot}", self.name), rc)
    }

    /// Remove a snapshot.
    pub fn remove_snapshot(&self, snapshot: &str) -> Result<()> {
        let c_snapshot = cstring(snapshot, "snapshot name")?;
        // SAFETY: as above.
        let rc = unsafe { raw::rbd_snap_remove(self.handle, c_snapshot.as_ptr()) };
        check("rbd_snap_remove", &format!("{}@{snapshot}", self.name), rc)
    }

    /// Submit an asynchronous read into `buffer`.
    ///
    /// # Safety
    /// `buffer` must remain valid and untouched until the returned
    /// [`Completion`] reports finished. librbd writes into it from its own
    /// thread, so the caller must not read or move it before then.
    pub unsafe fn read_async(
        &self,
        offset: u64,
        buffer: *mut u8,
        len: usize,
    ) -> Result<Completion> {
        let completion = Completion::new()?;
        // SAFETY: the image is open, the completion was just created, and
        // the caller's contract covers the buffer.
        let rc = unsafe {
            raw::rbd_aio_read(
                self.handle,
                offset,
                len,
                buffer as *mut core::ffi::c_char,
                completion.handle,
            )
        };
        check("rbd_aio_read", &self.name, rc)?;
        Ok(completion)
    }

    /// Submit an asynchronous write from `buffer`.
    ///
    /// # Safety
    /// `buffer` must remain valid and unmodified until the returned
    /// [`Completion`] reports finished.
    pub unsafe fn write_async(
        &self,
        offset: u64,
        buffer: *const u8,
        len: usize,
    ) -> Result<Completion> {
        let completion = Completion::new()?;
        // SAFETY: as with read_async.
        let rc = unsafe {
            raw::rbd_aio_write(
                self.handle,
                offset,
                len,
                buffer as *const core::ffi::c_char,
                completion.handle,
            )
        };
        check("rbd_aio_write", &self.name, rc)?;
        Ok(completion)
    }

    /// Submit an asynchronous discard — the SCSI UNMAP path of §5.3.
    pub fn discard_async(&self, offset: u64, len: u64) -> Result<Completion> {
        let completion = Completion::new()?;
        // SAFETY: the image is open and the completion was just created.
        let rc = unsafe { raw::rbd_aio_discard(self.handle, offset, len, completion.handle) };
        check("rbd_aio_discard", &self.name, rc)?;
        Ok(completion)
    }

    /// Submit an asynchronous flush — SYNCHRONIZE CACHE (§5.3).
    pub fn flush_async(&self) -> Result<Completion> {
        let completion = Completion::new()?;
        // SAFETY: the image is open and the completion was just created.
        let rc = unsafe { raw::rbd_aio_flush(self.handle, completion.handle) };
        check("rbd_aio_flush", &self.name, rc)?;
        Ok(completion)
    }

    /// Blocking flush, for teardown where there is nothing left to poll.
    pub fn flush(&self) -> Result<()> {
        // SAFETY: the image is open.
        let rc = unsafe { raw::rbd_flush(self.handle) };
        check("rbd_flush", &self.name, rc)
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: the handle came from rbd_open and this is the only owner.
        unsafe { raw::rbd_close(self.handle) };
    }
}

/// One in-flight librbd operation.
pub struct Completion {
    handle: raw::rbd_completion_t,
}

// SAFETY: completions are handed between the submitting thread and librbd's
// own; librbd synchronises access to them.
unsafe impl Send for Completion {}
unsafe impl Sync for Completion {}

impl Completion {
    fn new() -> Result<Self> {
        let mut handle: raw::rbd_completion_t = core::ptr::null_mut();
        // A null callback means the caller polls instead of being called
        // back, which is what §5.4's submit/poll interface wants.
        // SAFETY: handle is a live out-parameter; a null callback and
        // context are documented as valid.
        let rc =
            unsafe { raw::rbd_aio_create_completion(core::ptr::null_mut(), None, &mut handle) };
        check("rbd_aio_create_completion", "completion", rc)?;
        if handle.is_null() {
            return Err(RbdError::new(
                "rbd_aio_create_completion",
                "completion",
                -libc::ENOMEM,
            ));
        }
        Ok(Completion { handle })
    }

    /// Has librbd finished with this operation?
    pub fn is_complete(&self) -> bool {
        // SAFETY: the completion is live.
        unsafe { raw::rbd_aio_is_complete(self.handle) != 0 }
    }

    /// The result, once complete: bytes transferred, or the error.
    ///
    /// Reading this before [`is_complete`](Completion::is_complete) returns
    /// true gives a meaningless value, so callers must poll first.
    pub fn result(&self) -> i64 {
        // SAFETY: the completion is live.
        unsafe { raw::rbd_aio_get_return_value(self.handle) as i64 }
    }

    /// Block until the operation finishes.
    pub fn wait(&self) -> Result<i64> {
        // SAFETY: the completion is live.
        let rc = unsafe { raw::rbd_aio_wait_for_complete(self.handle) };
        check("rbd_aio_wait_for_complete", "completion", rc)?;
        Ok(self.result())
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        // librbd requires a completion to have finished before it is
        // released; releasing one still in flight would free memory the
        // library is about to write to.
        if !self.is_complete() {
            // SAFETY: the completion is live and this blocks until librbd
            // is done with it.
            unsafe { raw::rbd_aio_wait_for_complete(self.handle) };
        }
        // SAFETY: the completion is finished and this is the only owner.
        unsafe { raw::rbd_aio_release(self.handle) };
    }
}
