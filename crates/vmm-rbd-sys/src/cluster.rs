//! RADOS cluster handle and pool context.
//!
//! Ownership is strictly nested — image inside io context inside cluster —
//! and librados requires teardown in that order. Rust's drop order for
//! struct fields is declaration order, which would tear down the cluster
//! first, so each layer holds an `Arc` on the one below and lets reference
//! counting do it correctly instead.

use crate::error::{check, RbdError, Result};
use crate::raw;
use std::ffi::CString;
use std::path::Path;
use std::sync::Arc;

/// A connected RADOS cluster.
pub struct Cluster {
    handle: raw::rados_t,
    name: String,
}

// SAFETY: librados' cluster handle is documented as thread-safe; the
// library takes its own locks around every operation on it.
unsafe impl Send for Cluster {}
unsafe impl Sync for Cluster {}

impl Cluster {
    /// Connect to a cluster.
    ///
    /// `config` is a ceph.conf path, `cluster_name` the cluster's name
    /// (conventionally `ceph`) and `user` the client entity — `admin`
    /// becomes `client.admin`.
    pub fn connect(
        config: Option<&Path>,
        cluster_name: &str,
        user: &str,
        keyring: Option<&Path>,
    ) -> Result<Arc<Self>> {
        let subject = format!("{cluster_name}/client.{user}");
        let c_cluster = cstring(cluster_name, "cluster name")?;
        let c_user = cstring(&format!("client.{user}"), "user name")?;

        let mut handle: raw::rados_t = core::ptr::null_mut();
        // SAFETY: both strings are valid C strings that outlive the call,
        // and handle is a live out-parameter.
        let rc = unsafe { raw::rados_create2(&mut handle, c_cluster.as_ptr(), c_user.as_ptr(), 0) };
        check("rados_create2", &subject, rc)?;

        // From here on the handle must be shut down on every failure path,
        // so wrap it before anything else can fail.
        let cluster = Cluster {
            handle,
            name: subject.clone(),
        };

        match config {
            Some(path) => {
                let c_path = cstring(&path.display().to_string(), "config path")?;
                // SAFETY: the handle is live and c_path is a valid C string.
                let rc = unsafe { raw::rados_conf_read_file(cluster.handle, c_path.as_ptr()) };
                check("rados_conf_read_file", &path.display().to_string(), rc)?;
            }
            None => {
                // Null means librados' own search path: /etc/ceph/<name>.conf
                // and the CEPH_CONF environment variable.
                // SAFETY: the handle is live; a null path is documented.
                let rc = unsafe { raw::rados_conf_read_file(cluster.handle, core::ptr::null()) };
                check("rados_conf_read_file", "default search path", rc)?;
            }
        }

        if let Some(keyring) = keyring {
            cluster.set_option("keyring", &keyring.display().to_string())?;
        }

        // SAFETY: the handle is configured and not yet connected.
        let rc = unsafe { raw::rados_connect(cluster.handle) };
        check("rados_connect", &subject, rc)?;

        Ok(Arc::new(cluster))
    }

    /// Override one ceph.conf option before connecting.
    pub fn set_option(&self, key: &str, value: &str) -> Result<()> {
        let c_key = cstring(key, "option name")?;
        let c_value = cstring(value, "option value")?;
        // SAFETY: the handle is live and both strings are valid.
        let rc = unsafe { raw::rados_conf_set(self.handle, c_key.as_ptr(), c_value.as_ptr()) };
        check("rados_conf_set", key, rc)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Open an I/O context on a pool.
    pub fn pool(self: &Arc<Self>, pool: &str) -> Result<Arc<IoCtx>> {
        let c_pool = cstring(pool, "pool name")?;
        let mut handle: raw::rados_ioctx_t = core::ptr::null_mut();
        // SAFETY: the cluster is connected, c_pool is valid and handle is a
        // live out-parameter.
        let rc = unsafe { raw::rados_ioctx_create(self.handle, c_pool.as_ptr(), &mut handle) };
        check("rados_ioctx_create", pool, rc)?;

        Ok(Arc::new(IoCtx {
            handle,
            pool: pool.to_string(),
            // Keeping the cluster alive is the point: librados forbids
            // shutting it down while an io context is open.
            _cluster: Arc::clone(self),
        }))
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        // SAFETY: the handle came from rados_create2 and every io context
        // holds an Arc on this cluster, so none is still open.
        unsafe { raw::rados_shutdown(self.handle) };
    }
}

/// An I/O context bound to one pool.
pub struct IoCtx {
    handle: raw::rados_ioctx_t,
    pool: String,
    _cluster: Arc<Cluster>,
}

// SAFETY: librados' io context is thread-safe, as with the cluster handle.
unsafe impl Send for IoCtx {}
unsafe impl Sync for IoCtx {}

impl IoCtx {
    pub fn pool(&self) -> &str {
        &self.pool
    }

    pub(crate) fn as_ptr(&self) -> raw::rados_ioctx_t {
        self.handle
    }
}

impl Drop for IoCtx {
    fn drop(&mut self) {
        // SAFETY: the handle came from rados_ioctx_create and every image
        // holds an Arc on this context, so none is still open.
        unsafe { raw::rados_ioctx_destroy(self.handle) };
    }
}

/// Reject a string librados cannot accept, rather than truncating it.
pub(crate) fn cstring(value: &str, what: &str) -> Result<CString> {
    CString::new(value).map_err(|_| {
        // EINVAL: an interior NUL is a malformed argument, not an I/O fault.
        RbdError::new(
            "cstring",
            format!("{what} contains an interior NUL"),
            -libc::EINVAL,
        )
    })
}
