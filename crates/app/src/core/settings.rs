//! Tool settings (write mode, groups, tools, approval wait) and app settings (language,
//! theme, open at login). Everything takes effect immediately: the `SharedConfig` is the
//! same one the MCP server uses.

use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::config::{Theme, ToolGroup, WriteMode};
use adsconsole_core::secrets::SecretBackend;
use serde::Serialize;

use super::error::AppError;
use super::runtime::Runtime;

/// The project repository (`owner/name`), set at build time.
pub const REPO: &str = env!("ADSCONSOLE_REPO");
pub const LANGUAGES: [&str; 2] = ["pt", "en"];
const WAIT_RANGE: std::ops::RangeInclusive<u64> = 30..=900;

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ToolView {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub destructive: bool,
    pub reversible: bool,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct GroupView {
    pub group: ToolGroup,
    pub enabled: bool,
    pub tools: Vec<ToolView>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ToolSettingsView {
    pub platform: Platform,
    pub enabled: bool,
    pub write_mode: WriteMode,
    pub approval_wait_secs: u32,
    pub groups: Vec<GroupView>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct AppSettingsView {
    pub language: String,
    pub theme: Theme,
    pub autostart: bool,
    pub port: u16,
    pub secret_backend: Option<SecretBackend>,
    pub data_dir: String,
    pub version: String,
    pub repo_url: String,
    /// Development demo mode (`--demo`).
    pub demo: bool,
}

pub struct SettingsService {
    rt: Arc<Runtime>,
}

impl SettingsService {
    pub fn new(rt: Arc<Runtime>) -> Self {
        SettingsService { rt }
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.rt
    }

    pub fn tools(&self, p: Platform) -> ToolSettingsView {
        let settings = self.rt.config().server(p);
        let registry = self.rt.engine(p).registry();
        let mut groups: Vec<GroupView> = Vec::new();
        for spec in registry.specs().iter().filter(|s| s.exposed) {
            let tool = ToolView {
                name: spec.name.clone(),
                description: spec.description.clone(),
                enabled: registry.is_enabled(&spec.name, &settings),
                destructive: spec.destructive,
                reversible: spec.reversible,
            };
            match groups.iter_mut().find(|g| g.group == spec.group) {
                Some(g) => g.tools.push(tool),
                None => groups.push(GroupView {
                    group: spec.group,
                    enabled: settings
                        .groups
                        .get(&spec.group)
                        .copied()
                        .unwrap_or_else(|| spec.group.default_enabled()),
                    tools: vec![tool],
                }),
            }
        }
        groups.sort_by_key(|g| g.group);
        ToolSettingsView {
            platform: p,
            enabled: settings.enabled,
            write_mode: settings.write_mode,
            approval_wait_secs: settings.approval_wait_secs.min(u32::MAX as u64) as u32,
            groups,
        }
    }

    pub fn set_write_mode(&self, p: Platform, mode: WriteMode) -> Result<(), AppError> {
        self.rt
            .update_config(|c| c.servers.entry(p).or_default().write_mode = mode)?;
        Ok(())
    }

    /// Turns the whole group on or off: the group's per-tool choices are cleared.
    pub fn set_group(&self, p: Platform, group: ToolGroup, enabled: bool) -> Result<(), AppError> {
        let in_group: Vec<String> = self
            .rt
            .engine(p)
            .registry()
            .specs()
            .iter()
            .filter(|s| s.group == group)
            .map(|s| s.name.clone())
            .collect();
        self.rt.update_config(|c| {
            let s = c.servers.entry(p).or_default();
            s.groups.insert(group, enabled);
            for name in &in_group {
                s.tools.remove(name);
            }
        })?;
        Ok(())
    }

    pub fn set_tool(&self, p: Platform, name: &str, enabled: bool) -> Result<(), AppError> {
        if self
            .rt
            .engine(p)
            .registry()
            .get(name)
            .is_none_or(|s| !s.exposed)
        {
            return Err(
                AppError::new("INVALID_INPUT", format!("Unknown tool: {name}")).with_field("name"),
            );
        }
        self.rt.update_config(|c| {
            c.servers
                .entry(p)
                .or_default()
                .tools
                .insert(name.to_string(), enabled);
        })?;
        Ok(())
    }

    pub fn set_approval_wait(&self, p: Platform, secs: u64) -> Result<(), AppError> {
        if !WAIT_RANGE.contains(&secs) {
            return Err(AppError::new(
                "INVALID_INPUT",
                format!(
                    "Use {} to {} seconds.",
                    WAIT_RANGE.start(),
                    WAIT_RANGE.end()
                ),
            )
            .with_field("approval_wait_secs"));
        }
        self.rt
            .update_config(|c| c.servers.entry(p).or_default().approval_wait_secs = secs)?;
        Ok(())
    }

    pub fn app(&self) -> AppSettingsView {
        let c = self.rt.config();
        AppSettingsView {
            language: c.language.clone().unwrap_or_else(system_language),
            theme: c.theme,
            autostart: c.autostart,
            port: c.port,
            secret_backend: c.secret_backend,
            data_dir: self.rt.data_dir().display().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            repo_url: format!("https://github.com/{REPO}"),
            demo: false,
        }
    }

    pub fn set_language(&self, lang: &str) -> Result<(), AppError> {
        if !LANGUAGES.contains(&lang) {
            return Err(
                AppError::new("INVALID_INPUT", format!("Unsupported language: {lang}"))
                    .with_field("language"),
            );
        }
        self.rt
            .update_config(|c| c.language = Some(lang.to_string()))?;
        Ok(())
    }

    pub fn set_theme(&self, theme: Theme) -> Result<(), AppError> {
        self.rt.update_config(|c| c.theme = theme)?;
        Ok(())
    }

    /// Only saves the choice; the `shell` is what turns on start at login.
    pub fn set_autostart(&self, on: bool) -> Result<(), AppError> {
        self.rt.update_config(|c| c.autostart = on)?;
        Ok(())
    }
}

/// `pt` for any Portuguese variant of the system locale; `en` for everything else.
pub fn system_language() -> String {
    match sys_locale::get_locale() {
        Some(l) if l.to_lowercase().starts_with("pt") => "pt".into(),
        _ => "en".into(),
    }
}
