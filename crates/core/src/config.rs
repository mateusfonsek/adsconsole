use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::paths::EnvOverrides;
use crate::platform::Platform;
use crate::secrets::SecretBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    ReadOnly,
    Approval,
    Direct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    Read,
    Create,
    Edit,
    Media,
    History,
    Escape,
}

impl ToolGroup {
    pub fn default_enabled(self) -> bool {
        self != ToolGroup::Escape
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub enabled: bool,
    pub write_mode: WriteMode,
    pub approval_wait_secs: u64,
    pub groups: BTreeMap<ToolGroup, bool>,
    pub tools: BTreeMap<String, bool>,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            write_mode: WriteMode::Approval,
            approval_wait_secs: 120,
            groups: BTreeMap::new(),
            tools: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub autostart: bool,
    pub servers: BTreeMap<Platform, ServerSettings>,
    /// Escolhido na primeira execução; depois disso não muda sozinho.
    pub secret_backend: Option<SecretBackend>,
    /// `None`: segue o idioma do sistema.
    pub language: Option<String>,
    pub theme: Theme,
    /// Algum cliente de IA já chamou o MCP (primeiros passos do app).
    pub client_seen: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7458,
            autostart: false,
            servers: BTreeMap::new(),
            secret_backend: None,
            language: None,
            theme: Theme::System,
            client_seen: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub warning: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<LoadedConfig, CoreError> {
        let text = match std::fs::read(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedConfig {
                    config: Config::default(),
                    warning: None,
                });
            }
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice::<Config>(&text) {
            Ok(mut config) => {
                let mut warning = None;
                if config.port == 0 {
                    config.port = Config::default().port;
                    warning = Some(format!(
                        "config.json tinha a porta 0; usando a padrão {}.",
                        config.port
                    ));
                }
                Ok(LoadedConfig { config, warning })
            }
            Err(err) => {
                let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let backup_name = format!(
                    "{}.corrupt-{stamp}",
                    file.as_deref().unwrap_or("config.json")
                );
                std::fs::rename(path, path.with_file_name(&backup_name))?;
                Ok(LoadedConfig {
                    config: Config::default(),
                    warning: Some(format!(
                        "config.json estava inválido ({err}); usando o padrão. O arquivo antigo foi salvo como {backup_name}"
                    )),
                })
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), CoreError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| CoreError::Config(format!("Falha ao serializar a configuração: {e}")))?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn server(&self, p: Platform) -> ServerSettings {
        self.servers.get(&p).cloned().unwrap_or_default()
    }

    pub fn effective_port(&self, env: &EnvOverrides) -> u16 {
        env.port.unwrap_or(self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_gives_defaults() {
        let d = tempdir().unwrap();
        let l = Config::load(&d.path().join("config.json")).unwrap();
        assert_eq!(l.config.port, 7458);
        assert_eq!(
            l.config.server(Platform::MetaAds).write_mode,
            WriteMode::Approval
        );
        assert_eq!(l.config.server(Platform::GoogleAds).approval_wait_secs, 120);
        assert!(l.warning.is_none());
    }

    #[test]
    fn partial_file_fills_defaults() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, r#"{"servers":{"google_ads":{"write_mode":"direct"}}}"#).unwrap();
        let c = Config::load(&p).unwrap().config;
        let g = c.server(Platform::GoogleAds);
        assert_eq!(g.write_mode, WriteMode::Direct);
        assert!(g.enabled);
        assert_eq!(g.approval_wait_secs, 120);
        assert_eq!(c.port, 7458);
    }

    #[test]
    fn port_zero_falls_back_to_default() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, r#"{"port":0}"#).unwrap();
        let l = Config::load(&p).unwrap();
        assert_eq!(l.config.port, 7458);
        assert!(l.warning.is_some());
    }

    #[test]
    fn config_roundtrip_new_fields() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.json");
        let c = Config {
            secret_backend: Some(crate::secrets::SecretBackend::File),
            language: Some("en".into()),
            theme: Theme::Dark,
            client_seen: true,
            ..Config::default()
        };
        c.save(&p).unwrap();
        let l = Config::load(&p).unwrap().config;
        assert_eq!(l, c);
        assert_eq!(Config::default().theme, Theme::System);
        assert!(!Config::default().client_seen);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.json");
        let mut c = Config::default();
        let mut s = ServerSettings::default();
        s.tools.insert("mutate".into(), true);
        s.groups.insert(ToolGroup::Escape, true);
        c.servers.insert(Platform::GoogleAds, s);
        c.save(&p).unwrap();
        assert!(!d.path().join("config.json.tmp").exists());
        assert_eq!(Config::load(&p).unwrap().config, c);
    }

    #[test]
    fn corrupt_config_is_backed_up() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, "{ não é json").unwrap();
        let l = Config::load(&p).unwrap();
        assert_eq!(l.config, Config::default());
        assert!(l.warning.unwrap().contains("config.json.corrupt-"));
        let n = std::fs::read_dir(d.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.json.corrupt-")
            })
            .count();
        assert_eq!(n, 1);
        assert!(!p.exists());
    }

    #[test]
    fn escape_group_off_by_default() {
        assert!(!ToolGroup::Escape.default_enabled());
        assert!(ToolGroup::Read.default_enabled());
    }

    #[test]
    fn corrupt_non_utf8_config_is_backed_up() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
        let l = Config::load(&p).unwrap();
        assert_eq!(l.config, Config::default());
        assert!(l.warning.unwrap().contains("config.json.corrupt-"));
        assert!(!p.exists());
        let n = std::fs::read_dir(d.path()).unwrap().count();
        assert_eq!(n, 1);
    }
}
