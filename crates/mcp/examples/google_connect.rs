//! Connects a Google account (OAuth in the browser) to the AdsConsole data directory.
//!
//! `cargo run -p adsconsole-mcp --example google_connect -- <client.json> <name>
//!     [--login-customer-id N] [--access-level test|explorer|basic|standard]`
//!
//! Data directory: `ADSCONSOLE_DATA_DIR` or the default. Opens `adsconsole.db` and the
//! secret store, logs in through the browser and lists the accounts. Never prints
//! secrets (client secret, tokens).

use std::path::PathBuf;
use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::db::Db;
use adsconsole_core::paths::{EnvOverrides, data_dir};
use adsconsole_core::secrets::{SecretBackend, open_secret_store};
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_mcp::google::GoogleAdapter;
use adsconsole_mcp::google::connections::{
    AccessLevel, AddGoogleConnection, add_google_connection,
};

const USAGE: &str = "usage: google_connect <client.json> <name> [--login-customer-id N] \
                     [--access-level test|explorer|basic|standard]";

struct Args {
    client_json: PathBuf,
    name: String,
    login_customer_id: Option<String>,
    access_level: AccessLevel,
}

fn access_level(s: &str) -> Result<AccessLevel, String> {
    match s.to_ascii_lowercase().as_str() {
        "test" => Ok(AccessLevel::Test),
        "explorer" => Ok(AccessLevel::Explorer),
        "basic" => Ok(AccessLevel::Basic),
        "standard" => Ok(AccessLevel::Standard),
        other => Err(format!("unknown access level: {other}\n{USAGE}")),
    }
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut login_customer_id = None;
    let mut level = AccessLevel::Explorer;
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--login-customer-id" => {
                login_customer_id = Some(it.next().ok_or(USAGE)?);
            }
            "--access-level" => level = access_level(&it.next().ok_or(USAGE)?)?,
            s if s.starts_with("--") => return Err(format!("unknown option: {s}\n{USAGE}")),
            _ => positional.push(a),
        }
    }
    let [client_json, name]: [String; 2] = positional.try_into().map_err(|_| USAGE.to_string())?;
    Ok(Args {
        client_json: PathBuf::from(client_json),
        name,
        login_customer_id,
        access_level: level,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let client_json = std::fs::read(&args.client_json)
        .map_err(|e| format!("could not read {}: {e}", args.client_json.display()))?;

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

    let adapter = GoogleAdapter::new(db.clone(), secrets, GoogleEndpoints::default());
    let req = AddGoogleConnection {
        name: args.name,
        client_json,
        login_customer_id: args.login_customer_id,
        access_level: args.access_level,
    };
    println!("Opening the browser for the Google login (waits up to 5 min)...");
    let open = |url: &str| open::that(url);
    let meta = add_google_connection(&adapter, req, &open)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;

    println!(
        "Connection created: {} ({}), level {}",
        meta.connection_id,
        meta.name,
        args.access_level.label()
    );
    let accounts = db.list_accounts(Platform::GoogleAds, Some(&meta.connection_id))?;
    println!("Accounts ({}):", accounts.len());
    println!("  id | name | currency | status | manager_id");
    for (_, a) in accounts {
        println!(
            "  {} | {} | {} | {} | {}",
            a.account_id,
            a.name,
            a.currency,
            a.status.as_deref().unwrap_or("-"),
            a.manager_id.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}
