use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::error::CoreError;

/// Segredo em memória: `Debug` mascarado e sem `Display`, para não vazar em logs.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

pub const KEYRING_SERVICE: &str = "adsconsole";

const PROBE_KEY: &str = "__probe";
const MCP_TOKEN_KEY: &str = "mcp_token";

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, CoreError>;
    fn set(&self, key: &str, value: &str) -> Result<(), CoreError>;
    /// Apagar uma chave inexistente é `Ok(())`.
    fn delete(&self, key: &str) -> Result<(), CoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum SecretBackend {
    Keychain,
    File,
}

fn lock_error<T>(_: T) -> CoreError {
    CoreError::Secret("Armazenamento de segredos indisponível (lock envenenado)".into())
}

#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<BTreeMap<String, String>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, CoreError> {
        Ok(self.map.lock().map_err(lock_error)?.get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.map
            .lock()
            .map_err(lock_error)?
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), CoreError> {
        self.map.lock().map_err(lock_error)?.remove(key);
        Ok(())
    }
}

/// Segredos em JSON `{chave: valor}` com permissão 0600 (unix) a cada escrita.
pub struct FileStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<BTreeMap<String, String>, CoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                CoreError::Secret(format!(
                    "Arquivo de segredos {} ilegível: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, map: &BTreeMap<String, String>) -> Result<(), CoreError> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(map)
            .map_err(|e| CoreError::Secret(format!("Falha ao serializar segredos: {e}")))?;
        // Escrita atômica: arquivo temporário irmão (0600) e rename por cima do destino.
        let mut tmp_name = self.path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> std::io::Result<()> {
            let mut file = options.open(&tmp)?;
            // Garante 0600 também quando o temporário já existia com outra permissão.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
}

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> Result<Option<String>, CoreError> {
        let _guard = self.lock.lock().map_err(lock_error)?;
        Ok(self.read()?.get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), CoreError> {
        let _guard = self.lock.lock().map_err(lock_error)?;
        let mut map = self.read()?;
        map.insert(key.to_string(), value.to_string());
        self.write(&map)
    }
    fn delete(&self, key: &str) -> Result<(), CoreError> {
        let _guard = self.lock.lock().map_err(lock_error)?;
        let mut map = self.read()?;
        if map.remove(key).is_some() {
            self.write(&map)?;
        }
        Ok(())
    }
}

/// Segredos no keychain do sistema.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, CoreError> {
        keyring::Entry::new(&self.service, key).map_err(keyring_error)
    }
}

fn keyring_error(e: keyring::Error) -> CoreError {
    CoreError::Secret(format!("Keychain indisponível: {e}"))
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>, CoreError> {
        match self.entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error(e)),
        }
    }
    fn set(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.entry(key)?.set_password(value).map_err(keyring_error)
    }
    fn delete(&self, key: &str) -> Result<(), CoreError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_error(e)),
        }
    }
}

/// Escreve, lê e apaga uma chave de teste. A chave é apagada mesmo se a leitura falhar.
fn keychain_works(store: &dyn SecretStore) -> bool {
    if store.set(PROBE_KEY, "ok").is_err() {
        return false;
    }
    let read = matches!(store.get(PROBE_KEY), Ok(Some(v)) if v == "ok");
    store.delete(PROBE_KEY).is_ok() && read
}

/// O keychain do sistema responde a um teste de escrita/leitura.
pub fn keychain_available() -> bool {
    keychain_works(&KeyringStore::new(KEYRING_SERVICE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SecretStoreError {
    #[error(
        "O keychain do sistema não respondeu. Os segredos do AdsConsole estão nele; tente de novo ou passe a usar um arquivo."
    )]
    KeychainUnavailable,
}

/// Usa o keychain se ele responder a um teste de escrita/leitura; senão cai
/// para `data_dir/secrets.json` (o app avisa o usuário quando vier `File`).
pub fn open_secret_store(data_dir: &Path) -> (Arc<dyn SecretStore>, SecretBackend) {
    match open_secret_store_with(data_dir, None, &keychain_available) {
        Ok(r) => r,
        // Sem backend lembrado, a escolha nunca falha.
        Err(SecretStoreError::KeychainUnavailable) => file_store(data_dir),
    }
}

/// Como `open_secret_store`, respeitando o backend já escolhido antes: um keychain
/// lembrado que não responde é erro (cair para o arquivo trocaria o token MCP).
pub fn open_secret_store_with(
    data_dir: &Path,
    remembered: Option<SecretBackend>,
    keychain_ok: &dyn Fn() -> bool,
) -> Result<(Arc<dyn SecretStore>, SecretBackend), SecretStoreError> {
    choose_store(data_dir, remembered, keychain_ok, || {
        Arc::new(KeyringStore::new(KEYRING_SERVICE))
    })
}

fn file_store(data_dir: &Path) -> (Arc<dyn SecretStore>, SecretBackend) {
    (
        Arc::new(FileStore::new(data_dir.join("secrets.json"))),
        SecretBackend::File,
    )
}

fn choose_store(
    data_dir: &Path,
    remembered: Option<SecretBackend>,
    keychain_ok: &dyn Fn() -> bool,
    keychain: impl FnOnce() -> Arc<dyn SecretStore>,
) -> Result<(Arc<dyn SecretStore>, SecretBackend), SecretStoreError> {
    match remembered {
        Some(SecretBackend::File) => Ok(file_store(data_dir)),
        Some(SecretBackend::Keychain) if keychain_ok() => Ok((keychain(), SecretBackend::Keychain)),
        Some(SecretBackend::Keychain) => Err(SecretStoreError::KeychainUnavailable),
        None if keychain_ok() => Ok((keychain(), SecretBackend::Keychain)),
        None => Ok(file_store(data_dir)),
    }
}

/// O token do ambiente vence e não é gravado; senão vale o token guardado;
/// senão gera um novo (32 bytes, base64url sem padding) e grava. Token vazio ou só
/// com espaços (no ambiente ou guardado) conta como ausente.
pub fn ensure_mcp_token(
    store: &dyn SecretStore,
    env_token: Option<&str>,
) -> Result<String, CoreError> {
    if let Some(token) = env_token.filter(|t| !t.trim().is_empty()) {
        return Ok(token.to_string());
    }
    if let Some(token) = store.get(MCP_TOKEN_KEY)?.filter(|t| !t.trim().is_empty()) {
        return Ok(token);
    }
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    store.set(MCP_TOKEN_KEY, &token)?;
    Ok(token)
}

pub fn connection_secret_key(connection_id: &str, name: &str) -> String {
    format!("conn/{connection_id}/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(s: &dyn SecretStore) {
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", "1").unwrap();
        assert_eq!(s.get("a").unwrap(), Some("1".to_string()));
        s.set("a", "2").unwrap();
        assert_eq!(s.get("a").unwrap(), Some("2".to_string()));
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
        s.delete("a").unwrap();
    }

    #[test]
    fn memory_and_file_store_contract() {
        contract(&MemoryStore::default());
        let dir = tempfile::tempdir().unwrap();
        contract(&FileStore::new(dir.path().join("secrets.json")));
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let s = FileStore::new(path.clone());
        s.set("a", "1").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        s.set("b", "2").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn file_store_write_is_atomic_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let s = FileStore::new(path.clone());
        s.set("a", "1").unwrap();
        s.set("b", "2").unwrap();
        s.delete("a").unwrap();
        assert!(!dir.path().join("secrets.json.tmp").exists());
        let json: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json["b"], "2");
    }

    #[test]
    fn file_store_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        FileStore::new(path.clone()).set("a", "1").unwrap();
        assert_eq!(
            FileStore::new(path).get("a").unwrap(),
            Some("1".to_string())
        );
    }

    #[test]
    fn token_generated_once_and_reused() {
        let s = MemoryStore::default();
        let t1 = ensure_mcp_token(&s, None).unwrap();
        assert_eq!(t1.len(), 43);
        assert_eq!(ensure_mcp_token(&s, None).unwrap(), t1);
    }

    #[test]
    fn env_token_wins_and_is_not_stored() {
        let s = MemoryStore::default();
        assert_eq!(ensure_mcp_token(&s, Some("abc")).unwrap(), "abc");
        assert_eq!(s.get("mcp_token").unwrap(), None);
    }

    #[test]
    fn empty_tokens_count_as_absent() {
        let s = MemoryStore::default();
        for env in [Some(""), Some("  \t")] {
            let t = ensure_mcp_token(&s, env).unwrap();
            assert!(!t.trim().is_empty());
        }
        s.set("mcp_token", " ").unwrap();
        let t = ensure_mcp_token(&s, None).unwrap();
        assert!(!t.trim().is_empty());
        assert_eq!(s.get("mcp_token").unwrap(), Some(t));
    }

    #[test]
    fn secret_debug_is_masked() {
        let s = Secret::new("ya29.abc");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{:?}", Some(s.clone())), "Some(Secret(***))");
        assert_eq!(s.expose(), "ya29.abc");
        assert!(!s.is_empty());
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"ya29.abc\"");
    }

    #[test]
    fn connection_key_format() {
        assert_eq!(
            connection_secret_key("c1", "refresh_token"),
            "conn/c1/refresh_token"
        );
    }

    fn mem() -> Arc<dyn SecretStore> {
        Arc::new(MemoryStore::default())
    }

    #[test]
    fn first_run_picks_keychain_when_ok() {
        let dir = tempfile::tempdir().unwrap();
        let (_, b) = choose_store(dir.path(), None, &|| true, mem).unwrap();
        assert_eq!(b, SecretBackend::Keychain);
    }

    #[test]
    fn first_run_picks_file_when_keychain_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (_, b) = choose_store(dir.path(), None, &|| false, mem).unwrap();
        assert_eq!(b, SecretBackend::File);
    }

    #[test]
    fn remembered_keychain_never_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let r = choose_store(dir.path(), Some(SecretBackend::Keychain), &|| false, mem);
        assert!(matches!(r, Err(SecretStoreError::KeychainUnavailable)));
    }

    #[test]
    fn remembered_file_skips_probe() {
        let dir = tempfile::tempdir().unwrap();
        let probe = || -> bool { panic!("não deveria testar o keychain") };
        let (_, b) = choose_store(dir.path(), Some(SecretBackend::File), &probe, mem).unwrap();
        assert_eq!(b, SecretBackend::File);
    }

    /// Grava e apaga normalmente, mas a leitura sempre falha.
    #[derive(Default)]
    struct BrokenGet(MemoryStore);
    impl SecretStore for BrokenGet {
        fn get(&self, _: &str) -> Result<Option<String>, CoreError> {
            Err(CoreError::Secret("falha".into()))
        }
        fn set(&self, k: &str, v: &str) -> Result<(), CoreError> {
            self.0.set(k, v)
        }
        fn delete(&self, k: &str) -> Result<(), CoreError> {
            self.0.delete(k)
        }
    }

    #[test]
    fn probe_key_removed_even_if_get_fails() {
        let s = BrokenGet::default();
        assert!(!keychain_works(&s));
        assert_eq!(s.0.get(PROBE_KEY).unwrap(), None);
    }

    #[test]
    fn backend_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SecretBackend::Keychain).unwrap(),
            "\"keychain\""
        );
    }

    #[test]
    #[ignore = "toca no keychain real"]
    fn keyring_store_contract() {
        contract(&KeyringStore::new("adsconsole-test"));
    }
}
