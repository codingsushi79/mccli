//! The client side of the wire: address parsing, the TLS session, and a
//! background worker so a slow network never freezes the panel.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde_json::Value;

use crate::proto::{self, DEFAULT_PORT, MAX_LINE, PROTOCOL, Reply, Request, Welcome};
use crate::tls;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// Turn whatever the user typed into `host:port`.
///
/// Accepts a bare host or IP, `host:port`, `[v6]:port`, and urls such as
/// `mchost://play.example.com` or `https://panel.example.com/` — the scheme
/// and path are dropped, since all that matters is where to dial.
pub fn normalize_address(input: &str) -> Result<String> {
    let mut raw = input.trim();
    if raw.is_empty() {
        bail!("enter the host's address, e.g. 192.168.1.20 or play.example.com");
    }
    if let Some((_, rest)) = raw.split_once("://") {
        raw = rest;
    }
    // Drop any path or query from a pasted url.
    if let Some(i) = raw.find(['/', '?', '#']) {
        raw = &raw[..i];
    }
    // And any user@ prefix.
    if let Some((_, rest)) = raw.rsplit_once('@') {
        raw = rest;
    }
    if raw.is_empty() {
        bail!("`{}` has no host in it", input.trim());
    }
    if raw.contains(char::is_whitespace) {
        bail!("`{raw}` contains a space");
    }
    if raw.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(format!("[{raw}]:{DEFAULT_PORT}"));
    }
    if raw.starts_with('[') {
        return Ok(if raw.contains("]:") {
            raw.to_string()
        } else {
            format!("{raw}:{DEFAULT_PORT}")
        });
    }
    match raw.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port
                .parse()
                .map_err(|_| anyhow!("`{port}` is not a port number"))?;
            if host.is_empty() {
                bail!("`{raw}` has no host in it");
            }
            Ok(format!("{host}:{port}"))
        }
        None => Ok(format!("{raw}:{DEFAULT_PORT}")),
    }
}

fn dial(address: &str) -> Result<TcpStream> {
    let candidates: Vec<SocketAddr> = address
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve `{address}`"))?
        .collect();
    if candidates.is_empty() {
        bail!("`{address}` did not resolve to any address");
    }
    let mut last = None;
    for candidate in candidates {
        match TcpStream::connect_timeout(&candidate, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(CALL_TIMEOUT))?;
                stream.set_write_timeout(Some(CALL_TIMEOUT))?;
                stream.set_nodelay(true).ok();
                return Ok(stream);
            }
            Err(err) => last = Some(err),
        }
    }
    let err = last.expect("at least one candidate");
    let hint = match err.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            " — nothing is listening there. Is `mchost serve` running, and is the port right?"
        }
        std::io::ErrorKind::TimedOut => " — no answer. Check the address and any firewall.",
        _ => "",
    };
    bail!("cannot reach {address}: {err}{hint}")
}

pub struct Session {
    stream: BufReader<StreamOwned<ClientConnection, TcpStream>>,
    pub welcome: Welcome,
}

pub enum Outcome {
    Connected(Session),
    /// First contact: the user has to approve this fingerprint first.
    Untrusted { fingerprint: String },
}

/// Connect and log in.
///
/// `accept` is a fingerprint the user has just approved for a host we had not
/// seen before; it is pinned for this connection and remembered only once the
/// login succeeds.
pub fn connect(
    address: &str,
    username: &str,
    password: &str,
    accept: Option<&str>,
) -> Result<Outcome> {
    let address = normalize_address(address)?;
    let known = tls::known_fingerprint(&address);
    let pinned = known.clone().or_else(|| accept.map(str::to_string));
    let probing = pinned.is_none();

    let (config, seen) = tls::client_config(pinned)?;
    let mut socket = dial(&address)?;
    let name = ServerName::try_from(tls::SERVER_NAME).expect("static name");
    let mut connection = ClientConnection::new(config, name).context("starting TLS")?;
    while connection.is_handshaking() {
        connection
            .complete_io(&mut socket)
            .map_err(|e| anyhow!("TLS handshake with {address} failed: {e}"))?;
    }
    let fingerprint = seen
        .0
        .lock()
        .unwrap()
        .clone()
        .context("the host did not present a certificate")?;

    if probing {
        // Nothing has been sent yet: the password stays put until the user
        // has looked at the fingerprint.
        return Ok(Outcome::Untrusted { fingerprint });
    }

    let mut session = Session {
        stream: BufReader::new(StreamOwned::new(connection, socket)),
        welcome: Welcome {
            username: String::new(),
            role: proto::Role::Viewer,
            host_name: String::new(),
            version: String::new(),
        },
    };
    let data = session
        .call(&Request::Login {
            username: username.to_string(),
            password: password.to_string(),
            protocol: PROTOCOL,
        })?
        .map_err(|e| anyhow!("{e}"))?;
    session.welcome = serde_json::from_value(data).context("unexpected login reply")?;
    if known.is_none() {
        tls::trust(&address, &fingerprint)?;
    }
    Ok(Outcome::Connected(session))
}

impl Session {
    /// Outer error: the connection broke. Inner error: the host said no.
    pub fn call(&mut self, request: &Request) -> Result<Result<Value, String>> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        let writer = self.stream.get_mut();
        writer.write_all(&line).context("sending")?;
        writer.flush().context("sending")?;

        let mut buf = Vec::new();
        let read = (&mut self.stream)
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut buf)
            .context("waiting for the host")?;
        if read == 0 {
            bail!("the host closed the connection");
        }
        let reply: Reply = serde_json::from_slice(&buf).context("garbled reply from the host")?;
        Ok(if reply.ok {
            Ok(reply.data)
        } else {
            Err(reply.error.unwrap_or_else(|| "request failed".into()))
        })
    }
}

// -------------------------------------------------------------- worker ---

pub struct Job {
    pub id: u64,
    pub request: Request,
}

pub struct Done {
    pub id: u64,
    pub result: Result<Value, String>,
}

/// A session on its own thread. The UI posts jobs and picks up results each
/// frame, so it keeps drawing however long the network takes.
pub struct Link {
    tx: Sender<Job>,
    rx: Receiver<Done>,
    pub online: Arc<AtomicBool>,
    next_id: u64,
}

impl Link {
    pub fn spawn(session: Session, address: String, username: String, password: String) -> Link {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let online = Arc::new(AtomicBool::new(true));
        let flag = online.clone();
        std::thread::Builder::new()
            .name("mchost-link".into())
            .spawn(move || {
                let mut session = Some(session);
                while let Ok(job) = job_rx.recv() {
                    let result = run_job(&mut session, &job.request, &address, &username, &password);
                    flag.store(session.is_some(), Ordering::Relaxed);
                    if done_tx.send(Done { id: job.id, result }).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning the network thread");
        Link {
            tx: job_tx,
            rx: done_rx,
            online,
            next_id: 1,
        }
    }

    pub fn send(&mut self, request: Request) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.tx.send(Job { id, request });
        id
    }

    pub fn poll(&self) -> Vec<Done> {
        self.rx.try_iter().collect()
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }
}

fn run_job(
    session: &mut Option<Session>,
    request: &Request,
    address: &str,
    username: &str,
    password: &str,
) -> Result<Value, String> {
    for attempt in 0..2 {
        if session.is_none() {
            match connect(address, username, password, None) {
                Ok(Outcome::Connected(fresh)) => *session = Some(fresh),
                Ok(Outcome::Untrusted { .. }) => return Err("host identity is no longer trusted".into()),
                Err(err) => return Err(format!("reconnect failed: {err:#}")),
            }
        }
        let Some(live) = session.as_mut() else {
            continue;
        };
        match live.call(request) {
            Ok(result) => return result,
            Err(err) => {
                *session = None;
                if attempt == 1 {
                    return Err(format!("connection lost: {err:#}"));
                }
            }
        }
    }
    Err("connection lost".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_normalize() {
        let p = DEFAULT_PORT;
        assert_eq!(normalize_address("10.0.0.5").unwrap(), format!("10.0.0.5:{p}"));
        assert_eq!(normalize_address(" host:1234 ").unwrap(), "host:1234");
        assert_eq!(
            normalize_address("mchost://play.example.com").unwrap(),
            format!("play.example.com:{p}")
        );
        assert_eq!(
            normalize_address("https://panel.example.com:9000/servers?x=1").unwrap(),
            "panel.example.com:9000"
        );
        assert_eq!(normalize_address("::1").unwrap(), format!("[::1]:{p}"));
        assert_eq!(normalize_address("[::1]:77").unwrap(), "[::1]:77");
        assert!(normalize_address("").is_err());
        assert!(normalize_address("host:notaport").is_err());
    }
}
