//! Narrow Node-API whole-file lock host for the independent TypeScript core.
//!
//! Timeout loops and every database operation remain in TypeScript. This adapter owns one safely
//! opened coordination file and exposes only nonblocking SH/EX attempts, unlock, and close.

use std::fs::{File, TryLockError};

use napi::{Error, Result, Status};
use napi_derive::napi;

const ABI_VERSION: u32 = 1;

fn io_error(operation: &str, error: std::io::Error) -> Error {
    let class = if error.kind() == std::io::ErrorKind::Unsupported {
        "UNSUPPORTED"
    } else {
        "IO"
    };
    Error::new(
        Status::GenericFailure,
        format!("{class}:{operation}: {error}"),
    )
}

#[napi]
pub fn abi_version() -> u32 {
    ABI_VERSION
}

#[napi]
pub struct NativeLockFile {
    file: Option<File>,
}

#[napi]
impl NativeLockFile {
    #[napi(factory)]
    pub fn open(path: String) -> Result<Self> {
        let file = File::options()
            .read(true)
            .open(&path)
            .map_err(|e| io_error("open", e))?;
        Ok(Self { file: Some(file) })
    }

    #[napi]
    pub fn try_lock_shared(&self) -> Result<bool> {
        self.try_lock(false)
    }

    #[napi]
    pub fn try_lock_exclusive(&self) -> Result<bool> {
        self.try_lock(true)
    }

    #[napi]
    pub fn unlock(&self) -> Result<()> {
        self.file()?.unlock().map_err(|e| io_error("unlock", e))
    }

    #[napi]
    pub fn close(&mut self) {
        self.file.take();
    }
}

impl NativeLockFile {
    fn file(&self) -> Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| Error::new(Status::InvalidArg, "IO:lock file is closed"))
    }

    fn try_lock(&self, exclusive: bool) -> Result<bool> {
        let result = if exclusive {
            self.file()?.try_lock()
        } else {
            self.file()?.try_lock_shared()
        };
        match result {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(e)) => Err(io_error("lock", e)),
        }
    }
}
