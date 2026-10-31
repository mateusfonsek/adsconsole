//! Servidor MCP só com o Google Ads, sobre o diretório de dados real.
//!
//! `cargo run -p adsconsole-mcp --example google_server`
//!
//! - Diretório de dados: `ADSCONSOLE_DATA_DIR` ou o padrão (`adsconsole.db`, `config.json`,
//!   segredos). As conexões vêm de `google_connect`.
//! - Porta: `ADSCONSOLE_PORT` (padrão 7458). Token: `ADSCONSOLE_TOKEN` ou o guardado
//!   (gerado na primeira vez).
//! - Modo de escrita: o do `config.json`.
//! - `ADSCONSOLE_AUTO_APPROVE=1` aprova cada pedido sozinho. Sem ela, digite no terminal
//!   `pendentes`, `aprovar <id>` ou `recusar <id>`.

use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::db::Db;
use adsconsole_core::paths::{EnvOverrides, data_dir};
use adsconsole_core::secrets::{SecretBackend, ensure_mcp_token, open_secret_store};
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_mcp::engine::{Engine, EngineEvent, SharedConfig};
use adsconsole_mcp::google::GoogleAdapter;
use adsconsole_mcp::http::{ServerOptions, start};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = EnvOverrides::from_env()?;
    let dir = data_dir(&env)?;
    let loaded = Config::load(&dir.join("config.json"))?;
    if let Some(w) = &loaded.warning {
        println!("Aviso: {w}");
    }
    let config = loaded.config;
    let port = config.effective_port(&env);
    let mode = config.server(Platform::GoogleAds).write_mode;
    let config: SharedConfig = Arc::new(RwLock::new(config));

    let db = Arc::new(Db::open(&dir.join("adsconsole.db"))?);
    let (secrets, backend) = open_secret_store(&dir);
    if backend == SecretBackend::File {
        println!(
            "Aviso: keychain indisponível; segredos em {}",
            dir.join("secrets.json").display()
        );
    }
    let token = ensure_mcp_token(secrets.as_ref(), env.token.as_deref())?;

    let adapter = GoogleAdapter::new(db.clone(), secrets, GoogleEndpoints::default());
    let engine = Engine::new(adapter, db, config.clone())?;
    match engine.refresh_accounts().await {
        Ok(n) => println!("Contas no cache: {n}"),
        Err(e) => println!(
            "Aviso: falha ao atualizar as contas: {}: {}",
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
        Platform::GoogleAds.display_name(),
        server.addr,
        Platform::GoogleAds.endpoint()
    );
    println!("Diretório de dados: {}", dir.display());
    println!("Modo de escrita: {mode:?}");
    if auto {
        println!("Aprovação automática ligada (ADSCONSOLE_AUTO_APPROVE=1).");
    } else {
        println!("Comandos: pendentes | aprovar <id> | recusar <id>");
    }
    println!("Ctrl+C para sair.");

    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        () = command_loop(engine), if !auto => {}
    }
    server.shutdown().await;
    Ok(())
}

/// Lê comandos do stdin até o fim da entrada.
async fn command_loop(engine: Engine) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (None, _) => {}
            (Some("pendentes"), _) => match engine.pending() {
                Ok(list) if list.is_empty() => println!("Nenhuma pendência."),
                Ok(list) => {
                    for p in list {
                        println!(
                            "  {} | {} | expira {}",
                            p.change.change_id, p.change.summary, p.expires_at
                        );
                        if let Some(w) = p.warning {
                            println!("    aviso: {w}");
                        }
                    }
                }
                Err(e) => println!("Falha ao listar: {}: {}", e.code, e.message),
            },
            (Some("aprovar"), Some(id)) => match engine.approve(id).await {
                Ok(rec) => print_record("aprovado", &rec),
                Err(e) => println!("Falha ao aprovar: {}: {}", e.code, e.message),
            },
            (Some("recusar"), Some(id)) => match engine.reject(id).await {
                Ok(rec) => println!("  recusado → {:?}", rec.status),
                Err(e) => println!("Falha ao recusar: {}: {}", e.code, e.message),
            },
            _ => println!("Comandos: pendentes | aprovar <id> | recusar <id>"),
        }
    }
}

fn print_record(what: &str, rec: &adsconsole_core::db::ChangeRecord) {
    println!(
        "  {what} → {:?}\n  antes:  {}\n  depois: {}",
        rec.status,
        rec.before.clone().unwrap_or_default(),
        rec.after.clone().unwrap_or_default()
    );
    if let Some(err) = &rec.error {
        println!("  erro: {err}");
    }
}

/// Sem aprovação automática: avisa cada pedido e cada resultado.
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
                println!("pedido {change_id}: {summary}");
                if let Some(w) = warning {
                    println!("  aviso: {w}");
                }
                println!("  aprovar {change_id}  |  recusar {change_id}");
            }
            EngineEvent::ChangeApplied { change_id } => println!("aplicada: {change_id}"),
            EngineEvent::ChangeFailed { change_id } => println!("falhou: {change_id}"),
            EngineEvent::ChangeRejected { change_id } => println!("recusada: {change_id}"),
            EngineEvent::ChangeExpired { change_id } => println!("expirada: {change_id}"),
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
        println!("pedido {change_id}: {summary}");
        if let Some(w) = warning {
            println!("  aviso: {w}");
        }
        match engine.approve(&change_id).await {
            Ok(rec) => print_record("aprovado", &rec),
            Err(e) => println!("  falha ao aprovar: {}: {}", e.code, e.message),
        }
    }
}
