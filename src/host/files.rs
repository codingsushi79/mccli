//! The file manager, `server.properties`, and the player lists the server
//! keeps on disk.
//!
//! Every path a panel sends is relative to one server's folder and is
//! resolved through `jail`, which refuses `..`, absolute paths, and symlinks
//! that lead outside the folder. Nothing here can touch another server, the
//! host's own data, or the rest of the machine.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::proto::{BanEntry, FileEntry, Listing, Property};

/// Biggest file the editor will open.
pub const MAX_EDIT: u64 = 2 * 1024 * 1024;

/// Resolve `relative` inside `root`, or refuse.
pub fn jail(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = relative.trim().trim_start_matches(['/', '\\']);
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir => bail!("`..` is not allowed in paths"),
            Component::RootDir | Component::Prefix(_) => bail!("paths are relative to the server folder"),
        }
    }
    // Follow symlinks for whatever already exists and make sure we are
    // still inside. For a path that does not exist yet, check its parent.
    let root_real = root.canonicalize().with_context(|| format!("{} is missing", root.display()))?;
    // A new nested path is only as safe as the closest part that already
    // exists, so check that — creating `a/b/c` must not follow a symlink `a`.
    let mut probe = path.as_path();
    while std::fs::symlink_metadata(probe).is_err() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    let real = probe.canonicalize()?;
    if !real.starts_with(&root_real) {
        bail!("that path leads outside the server folder");
    }
    Ok(path)
}

/// `a/b/c` from a jailed absolute path, for display.
fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

fn modified(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn list(root: &Path, relative: &str) -> Result<Listing> {
    let dir = jail(root, relative)?;
    if !dir.is_dir() {
        bail!("`{relative}` is not a folder");
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {relative}"))? {
        let entry = entry?;
        let Ok(meta) = entry.metadata() else { continue };
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: modified(&meta),
        });
    }
    // Folders first, then by name, the way every file manager does it.
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(Listing {
        path: display_relative(root, &dir),
        entries,
    })
}

pub fn read(root: &Path, relative: &str) -> Result<String> {
    let path = jail(root, relative)?;
    let meta = std::fs::metadata(&path).with_context(|| format!("`{relative}` not found"))?;
    if meta.is_dir() {
        bail!("`{relative}` is a folder");
    }
    if meta.len() > MAX_EDIT {
        bail!(
            "`{relative}` is {} — too big to edit here (limit {})",
            crate::proto::fmt_bytes(meta.len()),
            crate::proto::fmt_bytes(MAX_EDIT)
        );
    }
    let bytes = std::fs::read(&path)?;
    if bytes.contains(&0) {
        bail!("`{relative}` is a binary file");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("`{relative}` is not UTF-8 text"))
}

pub fn write(root: &Path, relative: &str, content: &str) -> Result<()> {
    if content.len() as u64 > MAX_EDIT {
        bail!("that file is too big to save from the editor");
    }
    let path = jail(root, relative)?;
    if path.is_dir() {
        bail!("`{relative}` is a folder");
    }
    if path == root {
        bail!("pick a file name");
    }
    crate::paths::write_atomic(&path, content.as_bytes())
}

pub fn delete(root: &Path, relative: &str) -> Result<()> {
    let path = jail(root, relative)?;
    if path == root || relative.trim().trim_matches('/').is_empty() {
        bail!("refusing to delete the whole server folder");
    }
    let meta = std::fs::symlink_metadata(&path).with_context(|| format!("`{relative}` not found"))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(&path)?;
    } else {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn rename(root: &Path, from: &str, to: &str) -> Result<()> {
    let source = jail(root, from)?;
    let target = jail(root, to)?;
    if source == root || target == root {
        bail!("cannot rename the server folder itself");
    }
    if !source.exists() {
        bail!("`{from}` not found");
    }
    if target.exists() {
        bail!("`{to}` already exists");
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&source, &target)?;
    Ok(())
}

pub fn mkdir(root: &Path, relative: &str) -> Result<()> {
    let path = jail(root, relative)?;
    if path.exists() {
        bail!("`{relative}` already exists");
    }
    std::fs::create_dir_all(&path)?;
    Ok(())
}

/// Total size of a folder, for the dashboard.
pub fn du(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => du(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

// ---------------------------------------------------- server.properties ---

pub fn read_properties(dir: &Path) -> Result<Vec<Property>> {
    let text = match std::fs::read_to_string(dir.join("server.properties")) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    Ok(text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| Property {
            key: k.trim().to_string(),
            value: unescape(v.trim()),
        })
        .collect())
}

/// Java properties escape `:` and `=`; Minecraft writes `\:` in some values.
fn unescape(value: &str) -> String {
    value.replace("\\:", ":").replace("\\=", "=").replace("\\\\", "\\")
}

/// Update one key, keeping every other line (and comment) as it was.
pub fn set_property(dir: &Path, key: &str, value: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() || key.contains(['=', '\n', ' ']) {
        bail!("`{key}` is not a property name");
    }
    if value.contains('\n') {
        bail!("property values are one line");
    }
    let path = dir.join("server.properties");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if let Some((k, _)) = line.split_once('=')
                && !line.trim_start().starts_with('#')
                && k.trim() == key
            {
                found = true;
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("{key}={value}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    crate::paths::write_atomic(&path, out.as_bytes())
}

// --------------------------------------------------------- player lists ---

fn json_array(dir: &Path, file: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join(file))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok())
        .unwrap_or_default()
}

fn names(dir: &Path, file: &str) -> Vec<String> {
    json_array(dir, file)
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect()
}

pub fn ops(dir: &Path) -> Vec<String> {
    names(dir, "ops.json")
}

pub fn whitelist(dir: &Path) -> Vec<String> {
    names(dir, "whitelist.json")
}

pub fn banned(dir: &Path) -> Vec<BanEntry> {
    json_array(dir, "banned-players.json")
        .iter()
        .filter_map(|v| {
            Some(BanEntry {
                name: v.get("name")?.as_str()?.to_string(),
                reason: v.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string(),
                source: v.get("source").and_then(|r| r.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mchost-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_jail_holds() {
        let root = temp_root("jail");
        assert!(jail(&root, "../etc/passwd").is_err());
        assert!(jail(&root, "world/../../x").is_err());
        assert!(jail(&root, "/etc/passwd").is_ok_and(|p| p.starts_with(&root)));
        assert!(jail(&root, "plugins/a.yml").is_ok());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
            assert!(jail(&root, "escape/etc").is_err());
            assert!(jail(&root, "escape").is_err());
            assert!(jail(&root, "escape/new/deeper/file").is_err(), "new paths under a symlink");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn files_round_trip() {
        let root = temp_root("rt");
        mkdir(&root, "config").unwrap();
        write(&root, "config/test.yml", "a: 1\n").unwrap();
        assert_eq!(read(&root, "config/test.yml").unwrap(), "a: 1\n");
        rename(&root, "config/test.yml", "config/renamed.yml").unwrap();
        let listing = list(&root, "config").unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "renamed.yml");
        assert!(delete(&root, "").is_err(), "never the root");
        delete(&root, "config").unwrap();
        assert!(list(&root, "").unwrap().entries.is_empty());
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2]).unwrap();
        assert!(read(&root, "bin.dat").unwrap_err().to_string().contains("binary"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn properties_keep_comments_and_order() {
        let root = temp_root("props");
        std::fs::write(
            root.join("server.properties"),
            "#Minecraft server properties\nmotd=Hello\nserver-port=25565\n",
        )
        .unwrap();
        set_property(&root, "server-port", "25570").unwrap();
        set_property(&root, "pvp", "false").unwrap();
        let text = std::fs::read_to_string(root.join("server.properties")).unwrap();
        assert_eq!(
            text,
            "#Minecraft server properties\nmotd=Hello\nserver-port=25570\npvp=false\n"
        );
        let props = read_properties(&root).unwrap();
        assert_eq!(props.len(), 3);
        assert!(set_property(&root, "bad key", "x").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
