//! Fetching server jars from upstream: Mojang, PaperMC, Purpur and Fabric.
//!
//! `latest` resolves to the newest stable release each project offers. The
//! download streams to a temporary file and is renamed into place only when it
//! is complete, so a half-downloaded jar never gets launched.

use std::io::{Read, Write};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::instance::Instance;
use crate::proto::{ServerKind, ServerState};

const USER_AGENT: &str = concat!("mchost/", env!("CARGO_PKG_VERSION"));
pub const JAR: &str = "server.jar";

fn get_json(url: &str) -> Result<Value> {
    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    response
        .body_mut()
        .read_json::<Value>()
        .with_context(|| format!("reading {url}"))
}

/// Numeric-aware version ordering: `1.21.10` > `1.21.9`, `26.1` > `1.21.11`.
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(['.', '-'])
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

fn is_stable(version: &str) -> bool {
    !version.contains(['-', 'w']) && version.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Resolve the version and the URL to download for it.
pub fn resolve(kind: ServerKind, version: &str) -> Result<(String, String)> {
    let wanted = version.trim();
    let latest = wanted.is_empty() || wanted.eq_ignore_ascii_case("latest");
    match kind {
        ServerKind::Vanilla => {
            let manifest = get_json("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")?;
            let id = if latest {
                manifest["latest"]["release"]
                    .as_str()
                    .context("no latest release in Mojang's manifest")?
                    .to_string()
            } else {
                wanted.to_string()
            };
            let entry = manifest["versions"]
                .as_array()
                .and_then(|list| list.iter().find(|v| v["id"].as_str() == Some(&id)))
                .with_context(|| format!("Mojang has no version `{id}`"))?;
            let detail = get_json(entry["url"].as_str().context("version without url")?)?;
            let url = detail["downloads"]["server"]["url"]
                .as_str()
                .with_context(|| format!("`{id}` has no server download"))?;
            Ok((id, url.to_string()))
        }
        ServerKind::Paper => {
            let project = get_json("https://fill.papermc.io/v3/projects/paper")?;
            let mut all: Vec<String> = project["versions"]
                .as_object()
                .context("unexpected PaperMC response")?
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            all.sort_by_key(|v| version_key(v));
            let id = if latest {
                all.iter()
                    .rev()
                    .find(|v| is_stable(v))
                    .cloned()
                    .context("PaperMC lists no stable version")?
            } else if all.iter().any(|v| v == wanted) {
                wanted.to_string()
            } else {
                bail!("Paper has no version `{wanted}`");
            };
            let build = get_json(&format!(
                "https://fill.papermc.io/v3/projects/paper/versions/{id}/builds/latest"
            ))?;
            let url = build["downloads"]["server:default"]["url"]
                .as_str()
                .with_context(|| format!("no Paper build for `{id}` yet"))?;
            Ok((id, url.to_string()))
        }
        ServerKind::Purpur => {
            let project = get_json("https://api.purpurmc.org/v2/purpur")?;
            let id = if latest {
                project["metadata"]["current"]
                    .as_str()
                    .context("Purpur reports no current version")?
                    .to_string()
            } else {
                let known = project["versions"]
                    .as_array()
                    .is_some_and(|l| l.iter().any(|v| v.as_str() == Some(wanted)));
                if !known {
                    bail!("Purpur has no version `{wanted}`");
                }
                wanted.to_string()
            };
            Ok((id.clone(), format!("https://api.purpurmc.org/v2/purpur/{id}/latest/download")))
        }
        ServerKind::Fabric => {
            let stable = |url: &str| -> Result<String> {
                get_json(url)?
                    .as_array()
                    .and_then(|list| {
                        list.iter()
                            .find(|v| v["stable"].as_bool() == Some(true))
                            .and_then(|v| v["version"].as_str())
                            .map(str::to_string)
                    })
                    .ok_or_else(|| anyhow!("no stable entry at {url}"))
            };
            let id = if latest {
                stable("https://meta.fabricmc.net/v2/versions/game")?
            } else {
                wanted.to_string()
            };
            let loader = stable("https://meta.fabricmc.net/v2/versions/loader")?;
            let installer = stable("https://meta.fabricmc.net/v2/versions/installer")?;
            Ok((
                id.clone(),
                format!(
                    "https://meta.fabricmc.net/v2/versions/loader/{id}/{loader}/{installer}/server/jar"
                ),
            ))
        }
        ServerKind::Custom => bail!("custom servers bring their own jar — upload it and set its name"),
    }
}

/// Download `url` to `dest`, reporting progress through `progress(done, total)`.
fn download(url: &str, dest: &std::path::Path, mut progress: impl FnMut(u64, Option<u64>)) -> Result<()> {
    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024 * 1024)
        .reader();
    let tmp = dest.with_extension("download");
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    file.sync_all()?;
    drop(file);
    if done < 1024 {
        let _ = std::fs::remove_file(&tmp);
        bail!("the download was suspiciously small ({done} bytes)");
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Install a jar in the background. The server sits in `installing` until it
/// finishes; `then_start` boots it straight after.
pub fn spawn(instance: Arc<Instance>, kind: ServerKind, version: String, then_start: bool) -> Result<()> {
    {
        let mut rt = instance.lock();
        if rt.state.is_live() {
            bail!("stop {} before changing its jar", instance.name);
        }
        if rt.state == ServerState::Installing {
            bail!("{} is already installing", instance.name);
        }
        rt.state = ServerState::Installing;
        rt.activity = Some("resolving version…".into());
    }
    std::thread::spawn(move || {
        instance.log(format!("installing {kind} {version}"));
        let result = (|| -> Result<String> {
            let (resolved, url) = resolve(kind, &version)?;
            instance.lock().activity = Some(format!("downloading {kind} {resolved}"));
            let dest = instance.dir.join(JAR);
            let mut last_pct = u64::MAX;
            download(&url, &dest, |done, total| {
                let text = match total {
                    Some(total) if total > 0 => {
                        let pct = done * 100 / total;
                        if pct == last_pct {
                            return;
                        }
                        last_pct = pct;
                        format!("downloading {kind} {resolved} — {pct}%")
                    }
                    _ => format!("downloading {kind} {resolved} — {}", crate::proto::fmt_bytes(done)),
                };
                instance.lock().activity = Some(text);
            })?;
            Ok(resolved)
        })();
        {
            let mut rt = instance.lock();
            rt.activity = None;
            rt.state = ServerState::Stopped;
            match &result {
                Ok(resolved) => {
                    rt.settings.kind = kind;
                    rt.settings.version = resolved.clone();
                    rt.settings.jar = JAR.into();
                    rt.push(format!("[mchost] installed {kind} {resolved}"));
                }
                Err(err) => rt.push(format!("[mchost] install failed: {err:#}")),
            }
        }
        instance.save();
        if result.is_ok() && then_start && let Err(err) = instance.start() {
            instance.log(format!("could not start: {err:#}"));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_numerically() {
        let mut v = vec!["1.21.9", "26.1", "1.21.10", "1.8.8", "1.21"];
        v.sort_by_key(|s| version_key(s));
        assert_eq!(v, vec!["1.8.8", "1.21", "1.21.9", "1.21.10", "26.1"]);
    }

    #[test]
    fn stability() {
        assert!(is_stable("1.21.4"));
        assert!(is_stable("26.3"));
        assert!(!is_stable("26.3-rc-3"));
        assert!(!is_stable("1.21.11-pre5"));
        assert!(!is_stable("24w14a"));
    }
}
