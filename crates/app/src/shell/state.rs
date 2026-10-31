//! Estado gerenciado pelo Tauri: os serviços (depois do boot) e o resultado do boot.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use adsconsole_core::paths::EnvOverrides;

use crate::core::AppError;
use crate::core::services::{BootStateView, Services};

pub struct AppState {
    pub env: EnvOverrides,
    pub data_dir: PathBuf,
    pub demo: bool,
    services: RwLock<Option<Arc<Services>>>,
    boot: RwLock<BootStateView>,
}

impl AppState {
    pub fn new(env: EnvOverrides, data_dir: PathBuf, demo: bool) -> Self {
        AppState {
            env,
            data_dir,
            demo,
            services: RwLock::new(None),
            boot: RwLock::new(BootStateView {
                issue: None,
                notices: Vec::new(),
            }),
        }
    }

    pub fn services(&self) -> Result<Arc<Services>, AppError> {
        self.services
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| AppError::new("NOT_READY", "O AdsConsole ainda não terminou de abrir."))
    }

    pub fn set_services(&self, s: Option<Arc<Services>>) {
        *self.services.write().unwrap_or_else(|e| e.into_inner()) = s;
    }

    pub fn boot_state(&self) -> BootStateView {
        self.boot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_boot_state(&self, b: BootStateView) {
        *self.boot.write().unwrap_or_else(|e| e.into_inner()) = b;
    }
}
