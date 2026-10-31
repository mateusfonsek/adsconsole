//! Montagem de um `Runtime` de teste: pasta temporária, segredos em memória e
//! adaptadores falsos. Nunca toca no keychain real.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use adsconsole_app::core::runtime::{Adapters, BootOptions, Runtime};
use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::paths::EnvOverrides;
use adsconsole_core::secrets::{MemoryStore, SecretBackend, SecretStore, SecretStoreError};
use adsconsole_mcp::fake::FakePlatform;

pub struct Fakes {
    pub google: Arc<FakePlatform>,
    pub meta: Arc<FakePlatform>,
}

/// Porta livre agora (o listener é solto antes de devolver).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Grava um `config.json` com a porta dada (e o resto padrão).
pub fn write_config(dir: &Path, f: impl FnOnce(&mut Config)) {
    let mut c = Config::default();
    f(&mut c);
    c.save(&dir.join("config.json")).unwrap();
}

pub fn options(dir: &Path, keychain_ok: bool) -> (BootOptions, Fakes) {
    let fakes = Fakes {
        google: FakePlatform::new(Platform::GoogleAds),
        meta: FakePlatform::new(Platform::MetaAds),
    };
    let (g, m) = (fakes.google.clone(), fakes.meta.clone());
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let opts = BootOptions {
        data_dir: dir.to_path_buf(),
        env: EnvOverrides::default(),
        open_secrets: Box::new(move |remembered| match (remembered, keychain_ok) {
            (Some(SecretBackend::Keychain), false) => Err(SecretStoreError::KeychainUnavailable),
            (Some(SecretBackend::File), _) | (None, false) => {
                Ok((store.clone(), SecretBackend::File))
            }
            _ => Ok((store.clone(), SecretBackend::Keychain)),
        }),
        make_adapters: Box::new(move |db, _| {
            // As duas fakes usam os ids conn_a/conn_b; no banco compartilhado, só o
            // Google é semeado.
            g.seed_connections(&db).unwrap();
            Adapters {
                google: g.clone(),
                meta: m.clone(),
            }
        }),
    };
    (opts, fakes)
}

/// Boot com o keychain respondendo e a porta dada.
pub async fn boot_on(dir: &Path, port: u16) -> (Runtime, Fakes) {
    write_config(dir, |c| c.port = port);
    let (opts, fakes) = options(dir, true);
    (Runtime::boot(opts).await.unwrap(), fakes)
}
