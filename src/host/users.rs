//! Panel accounts.
//!
//! Passwords are stored as Argon2id hashes, never in the clear. A failed login
//! costs the same whether or not the user exists, so the login prompt cannot
//! be used to discover account names.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::proto::{Role, UserInfo, now};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct User {
    pub username: String,
    pub hash: String,
    pub role: Role,
    /// Servers this user may see. Empty means all of them.
    #[serde(default)]
    pub servers: Vec<String>,
    pub created: i64,
    #[serde(default)]
    pub last_login: Option<i64>,
    #[serde(default)]
    pub last_ip: Option<String>,
}

impl User {
    pub fn can_see(&self, server: &str) -> bool {
        self.role == Role::Admin || self.servers.is_empty() || self.servers.iter().any(|s| s == server)
    }

    pub fn info(&self) -> UserInfo {
        UserInfo {
            username: self.username.clone(),
            role: self.role,
            servers: self.servers.clone(),
            created: self.created,
            last_login: self.last_login,
            last_ip: self.last_ip.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct UserStore {
    pub users: Vec<User>,
}

fn store_path() -> std::path::PathBuf {
    paths::data_dir().join("users.json")
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// A random password that is easy to read aloud and type.
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 18];
    OsRng.fill_bytes(&mut bytes);
    let chars: String = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}-{}", &chars[..6], &chars[6..12], &chars[12..])
}

pub fn validate_username(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        bail!("usernames are 1–32 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!("usernames may use letters, digits, `_`, `-` and `.`");
    }
    Ok(())
}

pub fn validate_password(password: &str) -> Result<()> {
    if password.chars().count() < 8 {
        bail!("passwords need at least 8 characters");
    }
    if password.len() > 256 {
        bail!("that password is too long");
    }
    Ok(())
}

impl UserStore {
    pub fn load() -> Result<UserStore> {
        let path = store_path();
        if !path.exists() {
            return Ok(UserStore::default());
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        paths::ensure_host_dirs()?;
        let path = store_path();
        paths::write_atomic(&path, serde_json::to_string_pretty(self)?.as_bytes())?;
        paths::restrict_file(&path);
        Ok(())
    }

    pub fn get(&self, username: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.username.eq_ignore_ascii_case(username))
    }

    fn get_mut(&mut self, username: &str) -> Option<&mut User> {
        self.users
            .iter_mut()
            .find(|u| u.username.eq_ignore_ascii_case(username))
    }

    pub fn add(&mut self, username: &str, password: &str, role: Role, servers: Vec<String>) -> Result<()> {
        validate_username(username)?;
        validate_password(password)?;
        if self.get(username).is_some() {
            bail!("user `{username}` already exists");
        }
        self.users.push(User {
            username: username.to_string(),
            hash: hash_password(password)?,
            role,
            servers,
            created: now(),
            last_login: None,
            last_ip: None,
        });
        Ok(())
    }

    pub fn remove(&mut self, username: &str) -> Result<()> {
        let Some(user) = self.get(username) else {
            bail!("no user `{username}`");
        };
        if user.role == Role::Admin && self.admins() == 1 {
            bail!("`{username}` is the last admin; add another admin first");
        }
        self.users.retain(|u| !u.username.eq_ignore_ascii_case(username));
        Ok(())
    }

    pub fn set_password(&mut self, username: &str, password: &str) -> Result<()> {
        validate_password(password)?;
        let hash = hash_password(password)?;
        let user = self
            .get_mut(username)
            .with_context(|| format!("no user `{username}`"))?;
        user.hash = hash;
        Ok(())
    }

    pub fn set_role(&mut self, username: &str, role: Role, servers: Vec<String>) -> Result<()> {
        let admins = self.admins();
        let user = self
            .get_mut(username)
            .with_context(|| format!("no user `{username}`"))?;
        if user.role == Role::Admin && role != Role::Admin && admins == 1 {
            bail!("`{username}` is the last admin");
        }
        user.role = role;
        user.servers = servers;
        Ok(())
    }

    fn admins(&self) -> usize {
        self.users.iter().filter(|u| u.role == Role::Admin).count()
    }

    /// Check a password. On success, records the login.
    pub fn authenticate(&mut self, username: &str, password: &str, ip: &str) -> Option<User> {
        // A fixed hash to verify against when the user does not exist, so a
        // missing account takes as long to reject as a wrong password.
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let dummy = DUMMY.get_or_init(|| hash_password("not-a-real-password").unwrap_or_default());

        match self.get_mut(username) {
            Some(user) => {
                if verify_password(password, &user.hash) {
                    user.last_login = Some(now());
                    user.last_ip = Some(ip.to_string());
                    Some(user.clone())
                } else {
                    None
                }
            }
            None => {
                let _ = verify_password(password, dummy);
                None
            }
        }
    }
}

/// Per-address login throttling: five misses in ten minutes locks that
/// address out for ten minutes.
#[derive(Default)]
pub struct Throttle {
    misses: HashMap<IpAddr, (u32, Instant)>,
}

const MAX_MISSES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(600);

impl Throttle {
    pub fn locked(&mut self, ip: IpAddr) -> Option<Duration> {
        let (count, since) = *self.misses.get(&ip)?;
        let elapsed = since.elapsed();
        if elapsed > WINDOW {
            self.misses.remove(&ip);
            return None;
        }
        (count >= MAX_MISSES).then(|| WINDOW - elapsed)
    }

    pub fn miss(&mut self, ip: IpAddr) {
        let entry = self.misses.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() > WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    pub fn clear(&mut self, ip: IpAddr) {
        self.misses.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_verify_and_are_not_stored_plain() {
        let mut store = UserStore::default();
        store.add("alex", "correct horse", Role::Operator, vec![]).unwrap();
        assert!(!store.users[0].hash.contains("correct horse"));
        assert!(store.authenticate("alex", "correct horse", "1.2.3.4").is_some());
        assert!(store.authenticate("ALEX", "correct horse", "1.2.3.4").is_some());
        assert!(store.authenticate("alex", "wrong password", "1.2.3.4").is_none());
        assert!(store.authenticate("nobody", "correct horse", "1.2.3.4").is_none());
        assert_eq!(store.users[0].last_ip.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn the_last_admin_is_protected() {
        let mut store = UserStore::default();
        store.add("root", "password1", Role::Admin, vec![]).unwrap();
        assert!(store.remove("root").is_err());
        assert!(store.set_role("root", Role::Viewer, vec![]).is_err());
        store.add("second", "password2", Role::Admin, vec![]).unwrap();
        assert!(store.remove("root").is_ok());
    }

    #[test]
    fn weak_input_is_refused() {
        let mut store = UserStore::default();
        assert!(store.add("a b", "password1", Role::Viewer, vec![]).is_err());
        assert!(store.add("ok", "short", Role::Viewer, vec![]).is_err());
    }

    #[test]
    fn throttle_locks_after_repeated_misses() {
        let mut throttle = Throttle::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..MAX_MISSES {
            assert!(throttle.locked(ip).is_none());
            throttle.miss(ip);
        }
        assert!(throttle.locked(ip).is_some());
        throttle.clear(ip);
        assert!(throttle.locked(ip).is_none());
    }

    #[test]
    fn server_scoping() {
        let user = User {
            username: "u".into(),
            hash: String::new(),
            role: Role::Operator,
            servers: vec!["survival".into()],
            created: 0,
            last_login: None,
            last_ip: None,
        };
        assert!(user.can_see("survival"));
        assert!(!user.can_see("creative"));
    }
}
