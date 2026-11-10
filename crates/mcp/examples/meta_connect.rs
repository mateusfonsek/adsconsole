//! Connects a Meta account to the AdsConsole data directory.
//!
//! `cargo run -p adsconsole-mcp --example meta_connect -- <name> --login <app_id> <config_id> [--port N] [--relay <url>]`
//! `cargo run -p adsconsole-mcp --example meta_connect -- <name> --token [--app-id <id>]`
//!
//! The App Secret and the token never come from the arguments: they are read from
//! `META_APP_SECRET` and `META_ACCESS_TOKEN`, or typed in the terminal without echo.
//! Data directory: `ADSCONSOLE_DATA_DIR` or the default. Never prints secrets.
//!
//! `--relay`: https page that receives the login redirect and forwards it to localhost
//! (required with the app published; register it under the app's "Valid OAuth Redirect
//! URIs").

use std::io::Write;
use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::db::Db;
use adsconsole_core::paths::{EnvOverrides, data_dir};
use adsconsole_core::secrets::{Secret, SecretBackend, open_secret_store};
use adsconsole_mcp::meta::MetaAdapter;
use adsconsole_mcp::meta::connections::{
    AddMetaLogin, AddMetaToken, add_meta_login_connection, add_meta_token_connection,
};
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::oauth::AppCreds;

const USAGE: &str = "usage: meta_connect <name> --login <app_id> <config_id> [--port N] [--relay <url>]\n\
                     \x20     meta_connect <name> --token [--app-id <id>]";

#[derive(Debug, PartialEq)]
enum Mode {
    Login {
        app_id: String,
        config_id: String,
        port: u16,
        relay: Option<String>,
    },
    Token {
        app_id: Option<String>,
    },
}

#[derive(Debug, PartialEq)]
struct Args {
    name: String,
    mode: Mode,
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut it = raw.into_iter();
    let name = it.next().filter(|n| !n.starts_with("--")).ok_or(USAGE)?;
    let mut mode = None;
    let mut port = 0u16;
    let mut relay = None;
    let mut app_id = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--login" => {
                let id = it.next().ok_or(USAGE)?;
                let cfg = it.next().ok_or(USAGE)?;
                mode = Some((id, cfg));
            }
            "--token" => app_id = app_id.or(Some(None)),
            "--app-id" => app_id = Some(Some(it.next().ok_or(USAGE)?)),
            "--relay" => relay = Some(it.next().ok_or(USAGE)?),
            "--port" => {
                port = it
                    .next()
                    .ok_or(USAGE)?
                    .parse()
                    .map_err(|_| format!("invalid port\n{USAGE}"))?;
            }
            other => return Err(format!("unknown option: {other}\n{USAGE}")),
        }
    }
    let mode = match (mode, app_id) {
        (Some((app_id, config_id)), None) => Mode::Login {
            app_id,
            config_id,
            port,
            relay,
        },
        (None, Some(app_id)) => Mode::Token { app_id },
        _ => return Err(USAGE.to_string()),
    };
    Ok(Args { name, mode })
}

/// Reads from `var` or from the terminal, without echo.
fn secret(var: &str, prompt: &str) -> Result<Secret, Box<dyn std::error::Error>> {
    if let Ok(v) = std::env::var(var)
        && !v.trim().is_empty()
    {
        return Ok(Secret::new(v.trim()));
    }
    print!("{prompt}: ");
    std::io::stdout().flush()?;
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!();
    read?;
    Ok(Secret::new(line.trim()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let env = EnvOverrides::from_env()?;
    let dir = data_dir(&env)?;
    let db = Arc::new(Db::open(&dir.join("adsconsole.db"))?);
    let (secrets, backend) = open_secret_store(&dir);
    println!("Data directory: {}", dir.display());
    if backend == SecretBackend::File {
        println!(
            "Warning: keychain unavailable; secrets in {}",
            dir.join("secrets.json").display()
        );
    }
    let adapter = MetaAdapter::new(db.clone(), secrets, MetaEndpoints::default());
    let meta = match args.mode {
        Mode::Login {
            app_id,
            config_id,
            port,
            relay,
        } => {
            let app = AppCreds {
                app_id,
                app_secret: secret("META_APP_SECRET", "App Secret")?,
                config_id,
                relay_url: relay,
            };
            println!("Opening the browser for the Meta login (waits up to 5 min)...");
            let open = |url: &str| open::that(url);
            add_meta_login_connection(
                &adapter,
                AddMetaLogin {
                    name: args.name,
                    app,
                    port,
                },
                &open,
            )
            .await
        }
        Mode::Token { app_id } => {
            let access_token = secret("META_ACCESS_TOKEN", "Token")?;
            let app_secret = match &app_id {
                Some(_) => Some(secret("META_APP_SECRET", "App Secret")?),
                None => None,
            };
            add_meta_token_connection(
                &adapter,
                AddMetaToken {
                    name: args.name,
                    access_token,
                    app_id,
                    app_secret,
                },
            )
            .await
        }
    }
    .map_err(|e| format!("{}: {}", e.code, e.message))?;

    let expires = meta.expires_at.map_or_else(
        || "does not expire".to_string(),
        |t| t.format("%Y-%m-%d").to_string(),
    );
    println!(
        "Connection created: {} ({}), access: {expires}",
        meta.connection_id, meta.name
    );
    let accounts = db.list_accounts(Platform::MetaAds, Some(&meta.connection_id))?;
    println!("Accounts ({}):", accounts.len());
    println!("  id | name | currency | status");
    for (_, a) in accounts {
        println!(
            "  {} | {} | {} | {}",
            a.account_id,
            a.name,
            a.currency,
            a.status.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args, String> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_modes() {
        assert_eq!(
            args(&["Mine", "--login", "123", "cfg", "--port", "53682"]).unwrap(),
            Args {
                name: "Mine".into(),
                mode: Mode::Login {
                    app_id: "123".into(),
                    config_id: "cfg".into(),
                    port: 53682,
                    relay: None,
                }
            }
        );
        assert_eq!(
            args(&["System", "--token", "--app-id", "9"]).unwrap().mode,
            Mode::Token {
                app_id: Some("9".into())
            }
        );
        assert!(matches!(
            args(&["M", "--login", "1", "c", "--relay", "https://r.test/"]).unwrap().mode,
            Mode::Login { relay: Some(r), .. } if r == "https://r.test/"
        ));
        assert!(args(&["x"]).is_err());
        assert!(args(&["x", "--login", "1"]).is_err());
        assert!(args(&["x", "--login", "1", "c", "--token"]).is_err());
        assert!(args(&["--token"]).is_err());
    }
}
