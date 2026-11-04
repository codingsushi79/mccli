mod host;
mod net;
mod paths;
mod proto;
mod tls;
mod tui;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use proto::{DEFAULT_PORT, Role};

/// Host Minecraft servers, and control them from anywhere with a terminal panel.
#[derive(Parser)]
#[command(name = "mchost", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the panel and connect to a host (the default).
    Connect {
        /// Host address: ip, hostname, host:port or a url.
        address: Option<String>,
        /// Username to log in as.
        #[arg(short, long)]
        user: Option<String>,
    },
    /// Run the host: manage servers and accept panel logins.
    Serve {
        /// Address and port to listen on.
        #[arg(short, long, default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
        bind: String,
        /// Name shown to panels (defaults to the machine's hostname).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Manage panel accounts on this host.
    User {
        #[command(subcommand)]
        action: UserCmd,
    },
    /// Print this host's TLS fingerprint, to check on first connect.
    Fingerprint,
    /// Forget a host's remembered fingerprint (after it was reinstalled).
    Forget { address: String },
}

#[derive(Subcommand)]
enum UserCmd {
    /// List accounts.
    List,
    /// Add an account (prompts for the password).
    Add {
        username: String,
        /// viewer, operator or admin.
        #[arg(short, long, default_value = "operator")]
        role: String,
        /// Limit to these servers (comma separated). Default: all.
        #[arg(short, long)]
        servers: Option<String>,
    },
    /// Remove an account.
    Remove { username: String },
    /// Set an account's password (prompts).
    Passwd { username: String },
    /// Change an account's role.
    Role { username: String, role: String },
}

fn prompt_password() -> Result<String> {
    let first = rpassword::prompt_password("password: ")?;
    let second = rpassword::prompt_password("again:    ")?;
    if first != second {
        bail!("the passwords did not match");
    }
    host::users::validate_password(&first)?;
    Ok(first)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Connect {
        address: None,
        user: None,
    }) {
        Cmd::Connect { address, user } => tui::run(address, user),
        Cmd::Serve { bind, name } => host::serve(host::ServeOptions { bind, name }),
        Cmd::Fingerprint => {
            println!("{}", tls::local_fingerprint()?);
            Ok(())
        }
        Cmd::Forget { address } => {
            let address = net::normalize_address(&address)?;
            if tls::forget(&address)? {
                println!("forgot {address}; the next connect will ask again");
            } else {
                println!("{address} was not remembered");
            }
            Ok(())
        }
        Cmd::User { action } => match action {
            UserCmd::List => host::with_users(|store| {
                println!("{:<20} {:<9} {:<24} LAST LOGIN", "USER", "ROLE", "SERVERS");
                for u in &store.users {
                    println!(
                        "{:<20} {:<9} {:<24} {}",
                        u.username,
                        u.role.to_string(),
                        if u.servers.is_empty() { "all".into() } else { u.servers.join(",") },
                        u.last_login.map(proto::fmt_time).unwrap_or_else(|| "never".into())
                    );
                }
                Ok(())
            }),
            UserCmd::Add { username, role, servers } => {
                let role = Role::parse(&role).ok_or_else(|| anyhow::anyhow!("role is viewer, operator or admin"))?;
                let servers = servers
                    .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
                    .unwrap_or_default();
                let password = prompt_password()?;
                host::with_users(|store| store.add(&username, &password, role, servers))?;
                println!("added {username} ({role}). Restart is not needed.");
                Ok(())
            }
            UserCmd::Remove { username } => {
                host::with_users(|store| store.remove(&username))?;
                println!("removed {username}");
                Ok(())
            }
            UserCmd::Passwd { username } => {
                let password = prompt_password()?;
                host::with_users(|store| store.set_password(&username, &password))?;
                println!("password changed for {username}");
                Ok(())
            }
            UserCmd::Role { username, role } => {
                let role = Role::parse(&role).ok_or_else(|| anyhow::anyhow!("role is viewer, operator or admin"))?;
                host::with_users(|store| {
                    let servers = store.get(&username).map(|u| u.servers.clone()).unwrap_or_default();
                    store.set_role(&username, role, servers)
                })?;
                println!("{username} is now {role}");
                Ok(())
            }
        },
    }
}
