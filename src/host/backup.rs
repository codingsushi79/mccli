//! Backups: a `.tar.gz` of the whole server folder.
//!
//! A backup of a running server first turns autosave off and flushes the
//! world (`save-off`, `save-all flush`), so the archive is consistent, then
//! turns it back on. Restores need the server stopped; the current folder is
//! moved aside first and only deleted once the archive has unpacked cleanly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

use super::instance::Instance;
use crate::paths;
use crate::proto::{BackupInfo, now};

#[derive(Serialize, Deserialize, Default)]
struct Meta {
    note: String,
    by: String,
    created: i64,
}

fn archive(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.tar.gz"))
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("`{id}` is not a backup id");
    }
    Ok(())
}

pub fn list(server: &str) -> Result<Vec<BackupInfo>> {
    let dir = paths::backups_dir(server);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".tar.gz") else { continue };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let meta: Meta = std::fs::read_to_string(meta_path(&dir, id))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        out.push(BackupInfo {
            id: id.to_string(),
            created: if meta.created > 0 { meta.created } else { 0 },
            size,
            note: meta.note,
            by: meta.by,
        });
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

/// Start a backup in the background.
pub fn spawn_create(instance: Arc<Instance>, note: String, by: String) -> Result<String> {
    {
        let mut rt = instance.lock();
        if rt.backup_running {
            bail!("a backup of {} is already running", instance.name);
        }
        rt.backup_running = true;
    }
    let id = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let reply = format!("backing up {} as {id}", instance.name);
    std::thread::spawn(move || {
        let running = instance.is_running();
        if running {
            let _ = instance.send_command("save-off");
            let _ = instance.send_command("save-all flush");
            // `save-all flush` blocks the server thread until it is done; give
            // it a moment to reach the disk.
            std::thread::sleep(Duration::from_secs(3));
        }
        instance.log(format!("backup {id} started"));
        let result = write_archive(&instance, &id, &note, &by);
        if running && instance.is_running() {
            let _ = instance.send_command("save-on");
        }
        match result {
            Ok(size) => instance.log(format!("backup {id} done ({})", crate::proto::fmt_bytes(size))),
            Err(err) => instance.log(format!("backup {id} failed: {err:#}")),
        }
        instance.lock().backup_running = false;
    });
    Ok(reply)
}

fn write_archive(instance: &Instance, id: &str, note: &str, by: &str) -> Result<u64> {
    let dir = paths::backups_dir(&instance.name);
    std::fs::create_dir_all(&dir)?;
    let target = archive(&dir, id);
    let tmp = target.with_extension("partial");
    let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let encoder = GzEncoder::new(file, Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    builder
        .append_dir_all(".", &instance.dir)
        .context("archiving the server folder")?;
    builder.into_inner()?.finish()?.sync_all()?;
    std::fs::rename(&tmp, &target)?;
    let meta = Meta {
        note: note.to_string(),
        by: by.to_string(),
        created: now(),
    };
    std::fs::write(meta_path(&dir, id), serde_json::to_string(&meta)?)?;
    Ok(std::fs::metadata(&target)?.len())
}

pub fn restore(instance: Arc<Instance>, id: &str) -> Result<String> {
    valid_id(id)?;
    let source = archive(&paths::backups_dir(&instance.name), id);
    if !source.exists() {
        bail!("no backup `{id}`");
    }
    {
        let mut rt = instance.lock();
        if rt.state.is_live() {
            bail!("stop {} before restoring", instance.name);
        }
        if rt.backup_running {
            bail!("another backup job is running");
        }
        rt.backup_running = true;
    }
    let id = id.to_string();
    let reply = format!("restoring {} from {id}", instance.name);
    std::thread::spawn(move || {
        instance.log(format!("restoring backup {id}"));
        let result = unpack(&instance.dir, &source);
        match result {
            Ok(()) => instance.log(format!("restored {id}")),
            Err(err) => instance.log(format!("restore failed, nothing changed: {err:#}")),
        }
        instance.lock().backup_running = false;
    });
    Ok(reply)
}

fn unpack(server_dir: &Path, archive: &Path) -> Result<()> {
    let parent = server_dir.parent().context("server folder has no parent")?;
    let name = server_dir.file_name().context("server folder has no name")?.to_string_lossy();
    let staging = parent.join(format!(".{name}.restore"));
    let aside = parent.join(format!(".{name}.old"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    // `unpack` refuses entries that would land outside `staging`.
    tar.unpack(&staging).context("unpacking the archive")?;

    let _ = std::fs::remove_dir_all(&aside);
    std::fs::rename(server_dir, &aside).context("moving the current folder aside")?;
    if let Err(err) = std::fs::rename(&staging, server_dir) {
        // Put things back the way they were.
        let _ = std::fs::rename(&aside, server_dir);
        return Err(err).context("swapping in the restored folder");
    }
    let _ = std::fs::remove_dir_all(&aside);
    Ok(())
}

pub fn delete(server: &str, id: &str) -> Result<()> {
    valid_id(id)?;
    let dir = paths::backups_dir(server);
    let path = archive(&dir, id);
    if !path.exists() {
        bail!("no backup `{id}`");
    }
    std::fs::remove_file(path)?;
    let _ = std::fs::remove_file(meta_path(&dir, id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape() {
        assert!(valid_id("20261104-120000").is_ok());
        assert!(valid_id("../x").is_err());
        assert!(valid_id("").is_err());
    }

    #[test]
    fn unpack_swaps_folders() {
        let base = std::env::temp_dir().join(format!("mchost-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let server = base.join("srv");
        std::fs::create_dir_all(server.join("world")).unwrap();
        std::fs::write(server.join("world/level.dat"), "v1").unwrap();

        let archive_path = base.join("a.tar.gz");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
        builder.append_dir_all(".", &server).unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        std::fs::write(server.join("world/level.dat"), "v2").unwrap();
        std::fs::write(server.join("extra.txt"), "new").unwrap();
        unpack(&server, &archive_path).unwrap();
        assert_eq!(std::fs::read_to_string(server.join("world/level.dat")).unwrap(), "v1");
        assert!(!server.join("extra.txt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
