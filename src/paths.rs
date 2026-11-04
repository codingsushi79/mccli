//! Where things live on disk.
//!
//! The host keeps everything under one data directory: server folders,
//! backups, users, the TLS identity and the activity log. The client keeps its
//! trusted fingerprints and recent connections in a separate config directory,
//! so a machine can be a host, a client, or both without the two mixing.
//!
//! `MCHOST_HOME` overrides both, which is handy for running several hosts on
//! one machine and for tests.

use std::path::PathBuf;

use anyhow::{Context, Result};

fn home_override() -> Option<PathBuf> {
    std::env::var_os("MCHOST_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Host data: servers, backups, users, identity.
pub fn data_dir() -> PathBuf {
    if let Some(home) = home_override() {
        return home.join("host");
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mchost")
}

/// Client state: known hosts and recent connections.
pub fn client_dir() -> PathBuf {
    if let Some(home) = home_override() {
        return home.join("client");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mchost")
}

pub fn servers_dir() -> PathBuf {
    data_dir().join("servers")
}

pub fn server_dir(name: &str) -> PathBuf {
    servers_dir().join(name)
}

pub fn backups_dir(name: &str) -> PathBuf {
    data_dir().join("backups").join(name)
}

pub fn ensure_host_dirs() -> Result<()> {
    for dir in [data_dir(), servers_dir(), data_dir().join("backups")] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    restrict_dir(&data_dir());
    Ok(())
}

pub fn ensure_client_dir() -> Result<()> {
    let dir = client_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    restrict_dir(&dir);
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_path: &std::path::Path) {}

/// Owner-only permissions for files holding secrets.
#[cfg(unix)]
pub fn restrict_file(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn restrict_file(_path: &std::path::Path) {}

/// Write a file atomically: a crash mid-write leaves the old copy intact.
pub fn write_atomic(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp~");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
