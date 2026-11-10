//! MCP server with Meta Ads only, over the real data directory.
//!
//! `cargo run -p adsconsole-mcp --example meta_server`
//!
//! - Data directory: `ADSCONSOLE_DATA_DIR` or the default (`adsconsole.db`, `config.json`,
//!   secrets). The connections come from `meta_connect`.
//! - Port: `ADSCONSOLE_PORT` (default 7458). Token: `ADSCONSOLE_TOKEN` or the stored one
//!   (generated the first time).
//! - Write mode: the one in `config.json`.
//! - `ADSCONSOLE_AUTO_APPROVE=1` approves each request automatically. Without it, type
//!   `pending`, `approve <id>` or `reject <id>` in the terminal.

use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::db::Db;
use adsconsole_core::paths::{EnvOverrides, data_dir};
use adsconsole_core::secrets::{SecretBackend, ensure_mcp_token, open_secret_store};
use adsconsole_mcp::engine::{Engine, EngineEvent, SharedConfig};
use adsconsole_mcp::http::{ServerOptions, start};
use adsconsole_mcp::meta::MetaAdapter;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = EnvOverrides::from_env()?;
    let dir = data_dir(&env)?;
    let loaded = Config::load(&dir.join("config.json"))?;
    if let Some(w) = &loaded.warning {
        println!("Warning: {w}");
    }
    let config = loaded.config;
    let port = config.effective_port(&env);
    let mode = config.server(Platform::MetaAds).write_mode;
    let config: SharedConfig = Arc::new(RwLock::new(config));

    let db = Arc::new(Db::open(&dir.join("adsconsole.db"))?);
    let (secrets, backend) = open_secret_store(&dir);
    if backend == SecretBackend::File {
        println!(
            "Warning: keychain unavailable; secrets in {}",
            dir.join("secrets.json").display()
        );
    }
    let token = ensure_mcp_token(secrets.as_ref(), env.token.as_deref())?;

    let adapter = MetaAdapter::new(db.clone(), secrets, MetaEndpoints::default());
    let engine = Engine::new(adapter, db, config.clone())?;
    match engine.refresh_accounts().await {
        Ok(n) => println!("Accounts in cache: {n}"),
        Err(e) => println!(
            "Warning: failed to refresh the accounts: {}: {}",
            e.code, e.message
        ),
    }

    let auto = std::env::var("ADSCONSOLE_AUTO_APPROVE").is_ok_and(|v| v == "1");
    if auto {
        tokio::spawn(auto_approve(engine.clone()));
    } else {
        tokio::spawn(print_events(engine.clone()));
    }

    let server = start(
        vec![engine.clone()],
        config,
        ServerOptions {
            port,
            token: token.clone(),
            allow_ephemeral_port: false,
        },
    )
    .await?;
    println!("TOKEN={token}");
    println!(
        "{}: http://{}{}",
        Platform::MetaAds.display_name(),
        server.addr,
        Platform::MetaAds.endpoint()
    );
    println!("Data directory: {}", dir.display());
    println!("Write mode: {mode:?}");
    if auto {
        println!("Auto-approval on (ADSCONSOLE_AUTO_APPROVE=1).");
    } else {
        println!("Commands: pending | approve <id> | reject <id>");
    }
    println!("Ctrl+C to exit.");

    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        () = command_loop(engine), if !auto => {}
    }
    server.shutdown().await;
    Ok(())
}

/// Reads commands from stdin until the end of the input.
async fn command_loop(engine: Engine) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (None, _) => {}
            (Some("pending"), _) => match engine.pending() {
                Ok(list) if list.is_empty() => println!("Nothing pending."),
                Ok(list) => {
                    for p in list {
                        println!(
                            "  {} | {} | expires {}",
                            p.change.change_id, p.change.summary, p.expires_at
                        );
                        if let Some(w) = p.warning {
                            println!("    warning: {w}");
                        }
                    }
                }
                Err(e) => println!("Failed to list: {}: {}", e.code, e.message),
            },
            (Some("approve"), Some(id)) => match engine.approve(id).await {
                Ok(rec) => print_record("approved", &rec),
                Err(e) => println!("Failed to approve: {}: {}", e.code, e.message),
            },
            (Some("reject"), Some(id)) => match engine.reject(id).await {
                Ok(rec) => println!("  rejected → {:?}", rec.status),
                Err(e) => println!("Failed to reject: {}: {}", e.code, e.message),
            },
            _ => println!("Commands: pending | approve <id> | reject <id>"),
        }
    }
}

fn print_record(what: &str, rec: &adsconsole_core::db::ChangeRecord) {
    println!(
        "  {what} → {:?}\n  before: {}\n  after:  {}",
        rec.status,
        rec.before.clone().unwrap_or_default(),
        rec.after.clone().unwrap_or_default()
    );
    if let Some(err) = &rec.error {
        println!("  error: {err}");
    }
}

/// Without auto-approval: reports each request and each result.
async fn print_events(engine: Engine) {
    let mut rx = engine.subscribe();
    while let Ok(ev) = rx.recv().await {
        match ev {
            EngineEvent::ApprovalRequested {
                change_id,
                summary,
                warning,
                ..
            } => {
                println!("request {change_id}: {summary}");
                if let Some(w) = warning {
                    println!("  warning: {w}");
                }
                println!("  approve {change_id}  |  reject {change_id}");
            }
            EngineEvent::ChangeApplied { change_id } => println!("applied: {change_id}"),
            EngineEvent::ChangeFailed { change_id } => println!("failed: {change_id}"),
            EngineEvent::ChangeRejected { change_id } => println!("rejected: {change_id}"),
            EngineEvent::ChangeExpired { change_id } => println!("expired: {change_id}"),
        }
    }
}

async fn auto_approve(engine: Engine) {
    let mut rx = engine.subscribe();
    while let Ok(ev) = rx.recv().await {
        let EngineEvent::ApprovalRequested {
            change_id,
            summary,
            warning,
            ..
        } = ev
        else {
            continue;
        };
        println!("request {change_id}: {summary}");
        if let Some(w) = warning {
            println!("  warning: {w}");
        }
        match engine.approve(&change_id).await {
            Ok(rec) => print_record("approved", &rec),
            Err(e) => println!("  failed to approve: {}: {}", e.code, e.message),
        }
    }
}
