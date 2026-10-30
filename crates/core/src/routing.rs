use crate::db::{ConnectionStatus, Db};
use crate::error::CoreError;
use crate::platform::Platform;

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("Conexão desconhecida: {0}")]
    UnknownConnection(String),
    #[error("A conta {account_id} não está na conexão {connection}")]
    AccountNotInConnection {
        account_id: String,
        connection: String,
    },
    #[error("Conta {0} não encontrada em nenhuma conexão")]
    AccountNotFound(String),
    #[error(
        "A conta {account_id} existe em mais de uma conexão ({}); informe qual usar",
        connections
            .iter()
            .map(|(id, name)| format!("{name} [{id}]"))
            .collect::<Vec<_>>()
            .join(", ")
    )]
    Ambiguous {
        account_id: String,
        /// Pares (connection_id, nome).
        connections: Vec<(String, String)>,
    },
    #[error("A conexão {0} precisa ser reconectada")]
    NeedsReconnect(String),
    #[error(transparent)]
    Db(#[from] CoreError),
}

/// Google: tira hífens e espaços. Meta: garante o prefixo `act_` (sem diferenciar
/// maiúsculas); vazio continua vazio.
pub fn normalize_account_id(p: Platform, raw: &str) -> String {
    let raw = raw.trim();
    match p {
        Platform::GoogleAds => raw
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .collect(),
        Platform::MetaAds if raw.is_empty() => String::new(),
        Platform::MetaAds => match raw.get(..4) {
            Some(prefix) if prefix.eq_ignore_ascii_case("act_") => format!("act_{}", &raw[4..]),
            _ => format!("act_{raw}"),
        },
    }
}

/// Decide qual conexão atende a conta. `account_id` já vem normalizado.
pub fn resolve_connection(
    db: &Db,
    p: Platform,
    account_id: &str,
    explicit: Option<&str>,
) -> Result<String, RoutingError> {
    if let Some(id) = explicit {
        let conn = db
            .get_connection(id)?
            .filter(|c| c.platform == p)
            .ok_or_else(|| RoutingError::UnknownConnection(id.to_string()))?;
        let in_conn = db
            .accounts_for(p, account_id)?
            .iter()
            .any(|(cid, _)| cid == id);
        if !in_conn {
            return Err(RoutingError::AccountNotInConnection {
                account_id: account_id.to_string(),
                connection: id.to_string(),
            });
        }
        if conn.status == ConnectionStatus::NeedsReconnect {
            return Err(RoutingError::NeedsReconnect(conn.connection_id));
        }
        return Ok(conn.connection_id);
    }

    let candidate_ids: Vec<String> = db
        .accounts_for(p, account_id)?
        .into_iter()
        .map(|(cid, _)| cid)
        .collect();
    if candidate_ids.is_empty() {
        return Err(RoutingError::AccountNotFound(account_id.to_string()));
    }
    // list_connections vem ordenada por connection_id, como accounts_for.
    let candidates: Vec<_> = db
        .list_connections(p)?
        .into_iter()
        .filter(|c| candidate_ids.contains(&c.connection_id))
        .collect();
    let usable: Vec<_> = candidates
        .iter()
        .filter(|c| c.status == ConnectionStatus::Ok)
        .collect();
    match usable.as_slice() {
        [] => Err(RoutingError::NeedsReconnect(
            candidate_ids.into_iter().next().unwrap_or_default(),
        )),
        [only] => Ok(only.connection_id.clone()),
        many => match many.iter().find(|c| c.is_default) {
            Some(d) => Ok(d.connection_id.clone()),
            None => Err(RoutingError::Ambiguous {
                account_id: account_id.to_string(),
                connections: many
                    .iter()
                    .map(|c| (c.connection_id.clone(), c.name.clone()))
                    .collect(),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::Value;

    use super::*;

    #[test]
    fn meta_ids_normalize() {
        assert_eq!(
            normalize_account_id(Platform::MetaAds, "ACT_123"),
            "act_123"
        );
        assert_eq!(
            normalize_account_id(Platform::MetaAds, "Act_123"),
            "act_123"
        );
        assert_eq!(normalize_account_id(Platform::MetaAds, " 123 "), "act_123");
        assert_eq!(normalize_account_id(Platform::MetaAds, " act_1 "), "act_1");
        assert_eq!(normalize_account_id(Platform::MetaAds, ""), "");
    }
    use crate::db::{AccountInfo, ConnectionMeta, ConnectionStatus, Db};

    fn conn(id: &str, p: Platform, default: bool) -> ConnectionMeta {
        ConnectionMeta {
            connection_id: id.into(),
            platform: p,
            name: format!("Conexão {id}"),
            is_default: default,
            status: ConnectionStatus::Ok,
            expires_at: None,
            extra: Value::Null,
            created_at: Utc::now(),
        }
    }

    fn acct(id: &str) -> AccountInfo {
        AccountInfo {
            account_id: id.into(),
            name: format!("Conta {id}"),
            currency: "BRL".into(),
            timezone: None,
            status: None,
            manager_id: None,
        }
    }

    /// `a` (default) e `b`; act_1 só em `a`, act_2 em ambas, act_3 só em `b`.
    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.upsert_connection(&conn("a", Platform::MetaAds, true))
            .unwrap();
        db.upsert_connection(&conn("b", Platform::MetaAds, false))
            .unwrap();
        db.replace_accounts(Platform::MetaAds, "a", &[acct("act_1"), acct("act_2")])
            .unwrap();
        db.replace_accounts(Platform::MetaAds, "b", &[acct("act_2"), acct("act_3")])
            .unwrap();
        db
    }

    fn resolve(db: &Db, id: &str, explicit: Option<&str>) -> Result<String, RoutingError> {
        resolve_connection(db, Platform::MetaAds, id, explicit)
    }

    #[test]
    fn normalizes_ids() {
        assert_eq!(
            normalize_account_id(Platform::GoogleAds, "123-456-7890"),
            "1234567890"
        );
        assert_eq!(
            normalize_account_id(Platform::GoogleAds, " 123 456 "),
            "123456"
        );
        assert_eq!(normalize_account_id(Platform::MetaAds, "123"), "act_123");
        assert_eq!(
            normalize_account_id(Platform::MetaAds, "act_123"),
            "act_123"
        );
    }

    #[test]
    fn single_candidate() {
        assert_eq!(resolve(&setup(), "act_3", None).unwrap(), "b");
    }

    #[test]
    fn shared_account_uses_default() {
        assert_eq!(resolve(&setup(), "act_2", None).unwrap(), "a");
    }

    #[test]
    fn shared_without_default_is_ambiguous() {
        let db = setup();
        // Padrão de outra plataforma não conta.
        db.upsert_connection(&conn("g", Platform::GoogleAds, true))
            .unwrap();
        db.upsert_connection(&conn("a", Platform::MetaAds, false))
            .unwrap();
        match resolve(&db, "act_2", None).unwrap_err() {
            RoutingError::Ambiguous {
                account_id,
                connections,
            } => {
                assert_eq!(account_id, "act_2");
                assert_eq!(
                    connections,
                    vec![
                        ("a".to_string(), "Conexão a".to_string()),
                        ("b".to_string(), "Conexão b".to_string())
                    ]
                );
            }
            e => panic!("erro inesperado: {e:?}"),
        }
    }

    #[test]
    fn explicit_connection_wins() {
        assert_eq!(resolve(&setup(), "act_2", Some("b")).unwrap(), "b");
    }

    #[test]
    fn explicit_without_account() {
        assert!(matches!(
            resolve(&setup(), "act_1", Some("b")),
            Err(RoutingError::AccountNotInConnection { .. })
        ));
    }

    #[test]
    fn explicit_needs_reconnect() {
        let db = setup();
        db.set_connection_status("b", ConnectionStatus::NeedsReconnect)
            .unwrap();
        assert!(matches!(
            resolve(&db, "act_3", Some("b")),
            Err(RoutingError::NeedsReconnect(c)) if c == "b"
        ));
    }

    #[test]
    fn explicit_other_platform_is_unknown() {
        let db = setup();
        db.upsert_connection(&conn("g", Platform::GoogleAds, true))
            .unwrap();
        assert!(matches!(
            resolve(&db, "act_1", Some("g")),
            Err(RoutingError::UnknownConnection(_))
        ));
    }

    #[test]
    fn unknown_and_missing() {
        let db = setup();
        assert!(matches!(
            resolve(&db, "act_1", Some("zzz")),
            Err(RoutingError::UnknownConnection(c)) if c == "zzz"
        ));
        assert!(matches!(
            resolve(&db, "act_9", None),
            Err(RoutingError::AccountNotFound(_))
        ));
    }

    #[test]
    fn needs_reconnect_is_skipped() {
        let db = setup();
        db.set_connection_status("a", ConnectionStatus::NeedsReconnect)
            .unwrap();
        assert_eq!(resolve(&db, "act_2", None).unwrap(), "b");
        assert!(matches!(
            resolve(&db, "act_1", None),
            Err(RoutingError::NeedsReconnect(c)) if c == "a"
        ));
    }

    #[test]
    fn replace_accounts_replaces() {
        let db = setup();
        db.replace_accounts(Platform::MetaAds, "b", &[acct("act_3")])
            .unwrap();
        db.replace_accounts(Platform::MetaAds, "b", &[acct("act_4")])
            .unwrap();
        assert!(
            db.accounts_for(Platform::MetaAds, "act_3")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.accounts_for(Platform::MetaAds, "act_4").unwrap().len(),
            1
        );
    }

    #[test]
    fn delete_connection_clears_cache() {
        let db = setup();
        db.delete_connection("b").unwrap();
        assert!(
            db.accounts_for(Platform::MetaAds, "act_3")
                .unwrap()
                .is_empty()
        );
        assert!(db.get_connection("b").unwrap().is_none());
    }

    #[test]
    fn upsert_keeps_cache_and_default_is_exclusive() {
        let db = setup();
        let mut b = conn("b", Platform::MetaAds, false);
        b.name = "Novo nome".into();
        db.upsert_connection(&b).unwrap();
        assert_eq!(
            db.accounts_for(Platform::MetaAds, "act_3").unwrap().len(),
            1
        );
        db.set_default_connection(Platform::MetaAds, "b").unwrap();
        let list = db.list_connections(Platform::MetaAds).unwrap();
        assert!(!list[0].is_default && list[1].is_default);
        assert_eq!(list[1].name, "Novo nome");
        assert_eq!(
            db.list_accounts(Platform::MetaAds, Some("a"))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(db.list_accounts(Platform::MetaAds, None).unwrap().len(), 4);
    }
}
