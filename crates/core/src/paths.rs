use std::path::PathBuf;

use crate::error::CoreError;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnvOverrides {
    pub port: Option<u16>,
    pub token: Option<String>,
    pub data_dir: Option<PathBuf>,
}

impl EnvOverrides {
    pub fn from_env() -> Result<Self, CoreError> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, CoreError> {
        let get = |k: &str| get(k).filter(|v| !v.is_empty());
        let port = match get("ADSCONSOLE_PORT") {
            None => None,
            Some(v) => match v.parse::<u16>() {
                Ok(p) if p >= 1 => Some(p),
                _ => {
                    return Err(CoreError::Config(format!(
                        "ADSCONSOLE_PORT inválida: '{v}' (use um número de 1 a 65535)"
                    )));
                }
            },
        };
        Ok(Self {
            port,
            token: get("ADSCONSOLE_TOKEN"),
            data_dir: get("ADSCONSOLE_DATA_DIR").map(PathBuf::from),
        })
    }
}

pub fn data_dir(env: &EnvOverrides) -> Result<PathBuf, CoreError> {
    let dir = match &env.data_dir {
        Some(d) => d.clone(),
        None => dirs::data_dir()
            .ok_or_else(|| {
                CoreError::Config("Não foi possível localizar o diretório de dados".into())
            })?
            .join("adsconsole"),
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides() {
        let env = EnvOverrides::from_vars(|k| match k {
            "ADSCONSOLE_PORT" => Some("9000".into()),
            "ADSCONSOLE_TOKEN" => Some("".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(env.port, Some(9000));
        assert_eq!(env.token, None);
        assert_eq!(crate::config::Config::default().effective_port(&env), 9000);
        assert!(
            EnvOverrides::from_vars(|k| (k == "ADSCONSOLE_PORT").then(|| "abc".into())).is_err()
        );
    }

    #[test]
    fn data_dir_uses_override_and_creates() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("a").join("b");
        let env = EnvOverrides {
            data_dir: Some(target.clone()),
            ..Default::default()
        };
        assert_eq!(data_dir(&env).unwrap(), target);
        assert!(target.is_dir());
    }
}
