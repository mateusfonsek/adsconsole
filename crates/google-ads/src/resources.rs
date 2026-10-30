//! Nomes de recurso, IDs temporários e chaves de `MutateOperation`.

use serde_json::{Value, json};

pub fn rn(customer: &str, collection: &str, id: &str) -> String {
    format!("customers/{customer}/{collection}/{id}")
}

/// Gera IDs temporários negativos (`-1`, `-2`, …) numa sequência única por mutate.
#[derive(Debug, Clone)]
pub struct TempIds {
    customer: String,
    n: i64,
}

impl TempIds {
    pub fn new(customer: &str) -> Self {
        Self {
            customer: customer.to_string(),
            n: 0,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self, collection: &str) -> String {
        self.n += 1;
        rn(&self.customer, collection, &format!("-{}", self.n))
    }
}

const OP_KEYS: &[(&str, &str)] = &[
    ("campaigns", "campaignOperation"),
    ("campaignBudgets", "campaignBudgetOperation"),
    ("adGroups", "adGroupOperation"),
    ("adGroupCriteria", "adGroupCriterionOperation"),
    ("adGroupAds", "adGroupAdOperation"),
    ("ads", "adOperation"),
    ("campaignCriteria", "campaignCriterionOperation"),
    ("assets", "assetOperation"),
    ("campaignAssets", "campaignAssetOperation"),
    ("assetGroups", "assetGroupOperation"),
    ("assetGroupAssets", "assetGroupAssetOperation"),
    ("assetGroupSignals", "assetGroupSignalOperation"),
    ("sharedSets", "sharedSetOperation"),
    ("sharedCriteria", "sharedCriterionOperation"),
    ("campaignSharedSets", "campaignSharedSetOperation"),
    ("adGroupAssets", "adGroupAssetOperation"),
];

/// Chave do `MutateOperation` a partir de `customers/{c}/{colecao}/{id}`.
pub fn op_key(resource_name: &str) -> Result<&'static str, String> {
    let mut parts = resource_name.split('/');
    let collection = match (parts.next(), parts.next(), parts.next()) {
        (Some("customers"), Some(_), Some(c)) => c,
        _ => return Err(format!("nome de recurso inválido: {resource_name}")),
    };
    OP_KEYS
        .iter()
        .find(|(c, _)| *c == collection)
        .map(|(_, k)| *k)
        .ok_or_else(|| format!("coleção sem operação de mutate: {collection}"))
}

pub fn remove_op(rn: &str) -> Result<Value, String> {
    let key = op_key(rn)?;
    Ok(json!({ key: { "remove": rn } }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn temp_ids_unique() {
        let mut t = TempIds::new("1");
        assert_eq!(t.next("campaigns"), "customers/1/campaigns/-1");
        assert_eq!(t.next("adGroups"), "customers/1/adGroups/-2");
        assert_eq!(rn("1", "campaigns", "5"), "customers/1/campaigns/5");
    }

    #[test]
    fn op_keys() {
        assert_eq!(
            op_key("customers/1/campaigns/5").unwrap(),
            "campaignOperation"
        );
        assert_eq!(
            op_key("customers/1/adGroupCriteria/2~3").unwrap(),
            "adGroupCriterionOperation"
        );
        assert!(op_key("customers/1/unknowns/5").is_err());
        assert!(op_key("lixo").is_err());
        assert_eq!(
            remove_op("customers/1/campaigns/5").unwrap(),
            json!({"campaignOperation":{"remove":"customers/1/campaigns/5"}})
        );
    }
}
