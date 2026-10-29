use crate::models::{BackendId, ModelInfo};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct AliasConfig {
    pub kinds: BTreeMap<String, Vec<String>>,
    pub default_kind: String,
}

impl AliasConfig {
    pub fn from_config(kinds: std::collections::BTreeMap<String, Vec<String>>, default_kind: String) -> Self {
        AliasConfig { kinds, default_kind }
    }
    pub fn default_config() -> Self {
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "subscription".to_string(),
            vec![
                "cline/cline-pass/".to_string(),
                "opencode_go/".to_string(),
            ],
        );
        kinds.insert("credits".to_string(), vec!["cline/".to_string()]);
        AliasConfig {
            kinds,
            default_kind: "subscription".to_string(),
        }
    }
    pub fn billing_kind(&self, backend: BackendId, model_id: &str, scope: &str) -> String {
        if backend == BackendId::Cline {
            match scope {
                "cline-pass" => {
                    if self.kinds.contains_key("subscription") {
                        return "subscription".to_string();
                    }
                }
                "credits" => {
                    if self.kinds.contains_key("credits") {
                        return "credits".to_string();
                    }
                }
                _ => {}
            }
        }
        let mut best: Option<(usize, &str, &str)> = None; // (len, kind, prefix)
        for (kind, prefixes) in &self.kinds {
            for prefix in prefixes {
                if model_id.starts_with(prefix) {
                    let len = prefix.len();
                    // longest prefix wins; on tie, lexicographically smallest kind wins.
                    let better = match best {
                        None => true,
                        Some((bl, bk, _)) => {
                            len > bl || (len == bl && kind.as_str() < bk)
                        }
                    };
                    if better {
                        best = Some((len, kind.as_str(), prefix.as_str()));
                    }
                }
            }
        }
        if let Some((_, kind, _)) = best {
            return kind.to_string();
        }
        self.default_kind.clone()
    }
}

pub type AliasTable = std::collections::HashMap<String, Vec<(BackendId, String)>>;

pub fn base_name(model_id: &str) -> &str {
    model_id.rsplit('/').next().unwrap_or(model_id)
}

pub fn build_alias_table(
    catalogs: &std::collections::HashMap<BackendId, Vec<ModelInfo>>,
    config: &AliasConfig,
) -> AliasTable {
    let mut table: AliasTable = std::collections::HashMap::new();
    for (backend, models) in catalogs {
        for m in models {
            let kind = config.billing_kind(*backend, &m.id, &m.scope);
            let alias = format!("{}/{}", kind, base_name(&m.id));
            table
                .entry(alias)
                .or_default()
                .push((*backend, m.id.clone()));
        }
    }
    for v in table.values_mut() {
        v.sort_by(|a, b| {
            use std::cmp::Ordering;
            match a.0.as_str().cmp(b.0.as_str()) { Ordering::Equal => a.1.cmp(&b.1), o => o }
        });
        v.dedup();
    }
    table
}

pub fn is_alias(table: &AliasTable, requested: &str) -> bool {
    table.contains_key(requested)
}

pub fn concrete_for(table: &AliasTable, backend: BackendId, requested: &str) -> Option<String> {
    table
        .get(requested)?
        .iter()
        .find(|(b, _)| *b == backend)
        .map(|(_, id)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelInfo;

    fn model(id: &str, scope: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            context: 0,
            input: 0,
            output: 0,
            scope: scope.to_string(),
        }
    }

    #[test]
    fn base_name_last_segment() {
        assert_eq!(base_name("cline/openai/gpt-5.2"), "gpt-5.2");
        assert_eq!(base_name("opencode_go/deepseek-v4.1-flash"), "deepseek-v4.1-flash");
        assert_eq!(base_name("grok-4.7"), "grok-4.7");
    }

    #[test]
    fn cline_scope_authoritative() {
        let cfg = AliasConfig::default_config();
        assert_eq!(
            cfg.billing_kind(BackendId::Cline, "cline/cline-pass/deepseek-v4.1-flash", "cline-pass"),
"subscription"
        );
        assert_eq!(
            cfg.billing_kind(BackendId::Cline, "cline/openai/gpt-5.2", "credits"),
"credits"
        );
    }

    #[test]
    fn build_table_groups_by_kind_and_base() {
        let mut catalogs = std::collections::HashMap::new();
        catalogs.insert(
            BackendId::Cline,
            vec![
                model("cline/cline-pass/deepseek-v4.1-flash", "cline-pass"),
                model("cline/openai/gpt-5.2", "credits"),
            ],
        );
        catalogs.insert(
            BackendId::OpencodeGo,
            vec![model("opencode_go/deepseek-v4.1-flash", "")],
        );
        let cfg = AliasConfig::default_config();
        let table = build_alias_table(&catalogs, &cfg);
        assert!(table.contains_key("subscription/deepseek-v4.1-flash"));
        assert!(table.contains_key("credits/gpt-5.2"));
    }
}
