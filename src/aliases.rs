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
            "subscriptions".to_string(),
            vec![
                "cline/cline-pass/".to_string(),
                "opencode-go/".to_string(),
            ],
        );
        kinds.insert("credits".to_string(), vec!["cline/".to_string()]);
        AliasConfig {
            kinds,
            default_kind: "subscriptions".to_string(),
        }
    }
    pub fn billing_kind(&self, backend: BackendId, model_id: &str, scope: &str) -> String {
        if backend == BackendId::Cline {
            match scope {
                "cline-pass" => {
                    if self.kinds.contains_key("subscriptions") {
                        return "subscriptions".to_string();
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

/// Whether `s` contains "free" as a delimited token (case-insensitive): the substring must be
/// bounded by a non-alphanumeric character or the string boundary on both sides, so "freedom"
/// and "freemium" do not match but "beam-free", "free-tier", "Beam Free" and ":free" do.
pub fn contains_free_token(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let needle = b"free";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if bytes[i..i + needle.len()] == *needle {
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let right_idx = i + needle.len();
            let right_ok = right_idx == bytes.len()
                || !bytes[right_idx].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
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
            // A model is also available as free/<base> when "free" appears as a delimited
            // token in its id or display name (case-insensitive), e.g. "beam-free", "free-tier",
            // "Beam Free", or ":free" - but not when "free" is merely a substring of a larger
            // word such as "freedom" or "freemium". When the base name is exactly "free" (e.g.
            // cline/openrouter/free), append the previous path segment so the alias stays
            // identifiable (free/openrouter/free) instead of collapsing to free/free.
            // The operator may override this by defining a kind literally named "free" in
            // [model_aliases], which then owns the namespace.
            let operator_owns_free = config.kinds.contains_key("free");
            let is_free = !operator_owns_free
                && (contains_free_token(&m.id) || contains_free_token(&m.name));
            if is_free {
                let base = base_name(&m.id);
                let free_base = if base.eq_ignore_ascii_case("free") {
                    // use last two segments when available, else prefix with "free" to avoid
                    // collapsing to free/free.
                    let parts: Vec<&str> = m.id.split('/').collect();
                    if parts.len() >= 2 {
                        format!("{}/{}", parts[parts.len()-2], parts[parts.len()-1])
                    } else {
                        // Single-segment "free" id: keep it as free/free (unavoidable).
                        base.to_string()
                    }
                } else {
                    base.to_string()
                };
                let free_alias = format!("free/{}", free_base);
                table
                    .entry(free_alias)
                    .or_default()
                    .push((*backend, m.id.clone()));
            }
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
        assert_eq!(base_name("opencode-go/deepseek-v4.1-flash"), "deepseek-v4.1-flash");
        assert_eq!(base_name("grok-4.7"), "grok-4.7");
    }

    #[test]
    fn cline_scope_authoritative() {
        let cfg = AliasConfig::default_config();
        assert_eq!(
            cfg.billing_kind(BackendId::Cline, "cline/cline-pass/deepseek-v4.1-flash", "cline-pass"),
"subscriptions"
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
            vec![model("opencode-go/deepseek-v4.1-flash", "")],
        );
        let cfg = AliasConfig::default_config();
        let table = build_alias_table(&catalogs, &cfg);
        assert!(table.contains_key("subscriptions/deepseek-v4.1-flash"));
        assert!(table.contains_key("credits/gpt-5.2"));
    }

    #[test]
    fn free_alias_for_id_or_name_containing_free() {
        let mut catalogs = std::collections::HashMap::new();
        catalogs.insert(
            BackendId::Cline,
            vec![
                // free only in id
                model("cline/openai/beam-free", "credits"),
                // free only in name, id has no "free"
                ModelInfo {
                    id: "cline/openai/solar-mini4".to_string(),
                    name: "Solar Mini Free".to_string(),
                    context: 0,
                    input: 0,
                    output: 0,
                    scope: "credits".to_string(),
                },
                // not free
                model("cline/openai/gpt-5.2", "credits"),
            ],
        );
        catalogs.insert(
            BackendId::OpencodeGo,
            vec![model("opencode-go/beam-free", "")],
        );
        let cfg = AliasConfig::default_config();
        let table = build_alias_table(&catalogs, &cfg);
        assert!(table.contains_key("free/beam-free"));
        assert!(table.contains_key("free/solar-mini4"));
        assert!(!table.contains_key("free/gpt-5.2"));
        // Delimited token only: "freedom"/"freemium" must not match.
        let mut catalogs2 = std::collections::HashMap::new();
        catalogs2.insert(
            BackendId::Cline,
            vec![
                model("cline/openai/freedom-4", "credits"),
                model("cline/openai/freemium-pro", "credits"),
            ],
        );
        let t2 = build_alias_table(&catalogs2, &AliasConfig::default_config());
        assert!(!t2.contains_key("free/freedom-4"));
        assert!(!t2.contains_key("free/freemium-pro"));
        // free alias groups across backends
        let concretes = table.get("free/beam-free").unwrap();
        assert_eq!(concretes.len(), 2);
    }
}
