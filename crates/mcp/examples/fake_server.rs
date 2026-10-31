//! Servidor MCP com as duas plataformas fake, para testar clientes reais.
//!
//! `cargo run -p adsconsole-mcp --example fake_server`
//! Porta: `ADSCONSOLE_PORT` (padrão 7458). Token: `ADSCONSOLE_TOKEN` ou um novo a cada execução.
//! Modo aprovação; cada pedido é aprovado automaticamente e logado no terminal.

use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::Db;
use adsconsole_core::paths::EnvOverrides;
use adsconsole_core::secrets::{MemoryStore, ensure_mcp_token};
use adsconsole_mcp::engine::{Engine, EngineEvent, SharedConfig};
use adsconsole_mcp::fake::FakePlatform;
use adsconsole_mcp::http::{ServerOptions, start};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = EnvOverrides::from_env()?;
    let mut config = Config::default();
    for p in Platform::ALL {
        config.servers.insert(
            p,
            ServerSettings {
                write_mode: WriteMode::Approval,
                ..ServerSettings::default()
            },
        );
    }
    let port = config.effective_port(&env);
    let config: SharedConfig = Arc::new(RwLock::new(config));
    let token = ensure_mcp_token(&MemoryStore::default(), env.token.as_deref())?;

    let mut engines = Vec::new();
    for p in Platform::ALL {
        // Um banco por plataforma: as duas fakes usam os mesmos ids de conexão.
        let db = Arc::new(Db::open_in_memory()?);
        let fake = FakePlatform::new(p);
        fake.seed_connections(&db)?;
        let engine = Engine::new(fake, db, config.clone())?;
        engine.refresh_accounts().await?;
        tokio::spawn(auto_approve(engine.clone()));
        engines.push(engine);
    }

    let server = start(
        engines,
        config,
        ServerOptions {
            port,
            token: token.clone(),
            allow_ephemeral_port: false,
        },
    )
    .await?;
    println!("TOKEN={token}");
    for p in Platform::ALL {
        println!(
            "{}: http://{}{}",
            p.display_name(),
            server.addr,
            p.endpoint()
        );
    }
    println!("Ctrl+C para sair.");

    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(())
}

async fn auto_approve(engine: Engine) {
    let mut rx = engine.subscribe();
    while let Ok(ev) = rx.recv().await {
        let EngineEvent::ApprovalRequested {
            change_id,
            platform,
            summary,
            warning,
        } = ev
        else {
            continue;
        };
        println!(
            "[{}] pedido {change_id}: {summary}",
            platform.display_name()
        );
        if let Some(w) = warning {
            println!("  aviso: {w}");
        }
        match engine.approve(&change_id).await {
            Ok(rec) => println!(
                "  aprovado → {:?}\n  antes:  {}\n  depois: {}",
                rec.status,
                rec.before.unwrap_or_default(),
                rec.after.unwrap_or_default()
            ),
            Err(e) => println!("  falha ao aprovar: {e}"),
        }
    }
}
