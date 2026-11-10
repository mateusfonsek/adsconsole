use std::collections::HashMap;

use adsconsole_core::config::ServerSettings;

use crate::adapter::ToolSpec;

/// The tools of a platform, in registration order, with the toggles applied.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    specs: Vec<ToolSpec>,
    index: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new(specs: Vec<ToolSpec>) -> Result<Self, String> {
        let mut index = HashMap::with_capacity(specs.len());
        for (i, s) in specs.iter().enumerate() {
            if index.insert(s.name.clone(), i).is_some() {
                return Err(format!("Duplicate tool: {}", s.name));
            }
        }
        Ok(Self { specs, index })
    }

    /// All of them, in registration order (includes the hidden ones).
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Includes the hidden ones.
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.index.get(name).map(|&i| &self.specs[i])
    }

    /// Priority: `s.tools[name]` → `s.groups[group]` → `group.default_enabled()`.
    /// A hidden or unknown tool is never enabled.
    pub fn is_enabled(&self, name: &str, s: &ServerSettings) -> bool {
        self.get(name)
            .is_some_and(|spec| Self::spec_enabled(spec, s))
    }

    pub fn enabled(&self, s: &ServerSettings) -> Vec<&ToolSpec> {
        self.specs
            .iter()
            .filter(|spec| Self::spec_enabled(spec, s))
            .collect()
    }

    fn spec_enabled(spec: &ToolSpec, s: &ServerSettings) -> bool {
        spec.exposed
            && s.tools.get(&spec.name).copied().unwrap_or_else(|| {
                s.groups
                    .get(&spec.group)
                    .copied()
                    .unwrap_or_else(|| spec.group.default_enabled())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{PlatformAdapter, ToolSpec};
    use crate::fake::FakePlatform;
    use adsconsole_core::Platform;
    use adsconsole_core::config::{ServerSettings, ToolGroup};
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Deserialize, JsonSchema)]
    struct Empty {}

    #[test]
    fn toggles_priority() {
        let r = ToolRegistry::new(FakePlatform::new(Platform::GoogleAds).tools()).unwrap();
        let mut s = ServerSettings::default();
        assert!(r.is_enabled("get_campaigns", &s));
        assert!(!r.is_enabled("raw_mutate", &s)); // Escape is off by default
        s.groups.insert(ToolGroup::Escape, true);
        assert!(r.is_enabled("raw_mutate", &s));
        s.tools.insert("raw_mutate".into(), false); // the tool's toggle wins over the group's
        assert!(!r.is_enabled("raw_mutate", &s));
        s.groups.insert(ToolGroup::Edit, false);
        assert!(!r.enabled(&s).iter().any(|t| t.name == "update_campaign"));
    }

    #[test]
    fn hidden_tool_never_enabled() {
        let specs = vec![
            ToolSpec::read::<Empty>("a", "A", ToolGroup::Read),
            ToolSpec::read::<Empty>("b", "B", ToolGroup::Read).hidden(),
        ];
        let r = ToolRegistry::new(specs).unwrap();
        let mut s = ServerSettings::default();
        s.tools.insert("b".into(), true);
        assert!(r.get("b").is_some());
        assert!(!r.is_enabled("b", &s));
        assert!(!r.is_enabled("does_not_exist", &s));
        let names: Vec<_> = r.enabled(&s).iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn duplicate_names_rejected() {
        let specs = vec![
            ToolSpec::read::<Empty>("x", "X", ToolGroup::Read),
            ToolSpec::write::<Empty>("x", "X2", ToolGroup::Edit, true),
        ];
        assert!(ToolRegistry::new(specs).is_err());
    }

    #[test]
    fn mcp_tool_annotations() {
        let r = ToolRegistry::new(FakePlatform::new(Platform::GoogleAds).tools()).unwrap();
        let get = r.get("get_campaigns").unwrap().to_mcp_tool();
        let ann = get.annotations.clone().unwrap();
        assert_eq!(ann.read_only_hint, Some(true));
        let out = get
            .output_schema
            .clone()
            .expect("get_campaigns declares an output schema");
        assert_eq!(out["type"], "object");
        assert!(out["properties"]["campaigns"].is_object());

        let rm = r.get("remove_campaign").unwrap().to_mcp_tool();
        assert_eq!(rm.annotations.clone().unwrap().destructive_hint, Some(true));
        assert_eq!(rm.annotations.unwrap().read_only_hint, Some(false));
        assert!(
            rm.description
                .unwrap()
                .ends_with(" Warning: this change cannot be undone.")
        );

        let up = r.get("update_campaign").unwrap().to_mcp_tool();
        assert_eq!(up.annotations.clone().unwrap().idempotent_hint, Some(true));
        assert!(!up.description.clone().unwrap().contains("cannot be undone"));
        let required = up.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "account_id"));
        assert!(required.iter().any(|v| v == "campaign_id"));
        assert!(up.input_schema["properties"]["account_id"].is_object());
        assert!(up.input_schema["properties"]["connection"].is_object());
        assert!(up.input_schema.get("allOf").is_none());
        assert!(up.input_schema.get("description").is_none());
    }
}
