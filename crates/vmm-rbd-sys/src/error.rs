//! Errors from librados/librbd, with the errno rendered.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RbdError {
    /// The C entry point that failed.
    pub call: &'static str,
    /// What was being operated on, for a message an operator can act on.
    pub subject: String,
    /// The negative errno librados returned.
    pub code: i32,
}

impl RbdError {
    pub fn new(call: &'static str, subject: impl Into<String>, code: i32) -> Self {
        RbdError {
            call,
            subject: subject.into(),
            code,
        }
    }

    /// The positive errno, for callers mapping onto their own error space.
    pub fn errno(&self) -> i32 {
        self.code.abs()
    }

    /// Does this mean the object simply is not there?
    pub fn is_not_found(&self) -> bool {
        self.errno() == libc::ENOENT
    }

    /// Does this mean the caller lacks the capability or the key?
    pub fn is_permission_denied(&self) -> bool {
        matches!(self.errno(), e if e == libc::EPERM || e == libc::EACCES)
    }
}

impl fmt::Display for RbdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let description = std::io::Error::from_raw_os_error(self.errno());
        write!(
            f,
            "{}({}) failed: {description} (errno {})",
            self.call,
            self.subject,
            self.errno()
        )
    }
}

impl std::error::Error for RbdError {}

pub type Result<T> = std::result::Result<T, RbdError>;

/// Turn a librados return code into a `Result`.
pub(crate) fn check(call: &'static str, subject: &str, code: i32) -> Result<()> {
    if code < 0 {
        return Err(RbdError::new(call, subject, code));
    }
    Ok(())
}
