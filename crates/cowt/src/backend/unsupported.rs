//! Stub backend for platforms whose VFS integration is not yet shipped.

use std::path::Path;

use anyhow::{bail, Result};

use super::{Backend, MountGuard};

pub struct Unsupported {
    name: &'static str,
    #[allow(dead_code)]
    reason: &'static str,
}

impl Unsupported {
    pub fn new(name: &'static str, reason: &'static str) -> Self {
        Self { name, reason }
    }

    fn err(&self) -> anyhow::Error {
        anyhow::anyhow!("backend '{}' unavailable: {}", self.name, self.reason)
    }
}

impl Backend for Unsupported {
    fn name(&self) -> &'static str {
        self.name
    }

    fn available(&self) -> Result<()> {
        bail!("{}", self.err())
    }

    fn mount(&self, _l: &Path, _u: &Path, _w: &Path, _m: &Path) -> Result<MountGuard> {
        Err(self.err())
    }

    fn unmount(&self, _m: &Path) -> Result<()> {
        Err(self.err())
    }

    fn is_mounted(&self, _m: &Path) -> bool {
        false
    }
}
