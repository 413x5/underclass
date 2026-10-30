//! What `/v1/models` advertises: the catalogs of backends that actually have an account, plus
//! the aliases whose concrete models survive that filter.
//!
//! The stored catalog is deliberately *not* pruned when an account is removed: it is the record of
//! what the backend published, and re-adding an account restores its models unchanged. The listing
//! is therefore a read-time view over the store, not a copy of it.

use crate::aliases::AliasTable;
use crate::models::{Account, BackendId, ModelInfo};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One entry of the `GET /v1/models` `data` array.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvertisedModel {
    pub id: String,
    pub name: String,
    pub context: i64,
    pub input: i64,
    pub output: i64,
    pub owned_by: String,
}

impl AdvertisedModel {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "object": "model",
            "name": self.name,
            "context_window": self.context,
            "max_input_tokens": self.input,
            "max_output_tokens": self.output,
            "owned_by": self.owned_by,
        })
    }
}

/// @cc [owner:ghuntley,label:proxy] configured-backend-set
/// `backends_with_accounts` MUST treat an account as configured regardless of its `status`, so a
/// cooling, disabled, or auth-error account still makes its backend's models advertised.
pub fn backends_with_accounts(accounts: &[Account]) -> HashSet<BackendId> {
    accounts.iter().map(|a| a.backend).collect()
}

/// @cc [owner:ghuntley,label:proxy] account-backed-model-advertisement
/// `advertised_models` MUST emit a backend's catalog rows only when that backend is in
/// `servable`, and MUST emit an alias only when at least one of its concrete `(backend, model)`
/// pairs belongs to a servable backend whose catalog lists that model. A model or alias with no
/// servable concrete MUST NOT appear, and the stored catalogs MUST NOT be mutated.
///
/// The filter is applied per concrete model, so an alias that groups models across backends keeps
/// every concrete that is still servable.
///
/// Surface plays no part here: `/v1/models` is a catalog of what the pool serves, so Claude's
/// Anthropic-surface rows are listed exactly as every other backend's are. The OpenAI-shaped
/// provider blocks in `underclass connect` and the UI snippet drop Claude separately.
pub fn advertised_models(
    catalogs: &HashMap<BackendId, Vec<ModelInfo>>,
    servable: &HashSet<BackendId>,
    aliases: &AliasTable,
) -> Vec<AdvertisedModel> {
    let mut merged: BTreeMap<String, AdvertisedModel> = BTreeMap::new();

    for backend in BackendId::all() {
        if !servable.contains(&backend) {
            continue;
        }
        let Some(models) = catalogs.get(&backend) else {
            continue;
        };
        for m in models {
            let entry = AdvertisedModel {
                id: m.id.clone(),
                name: m.name.clone(),
                context: m.context,
                input: m.input,
                output: m.output,
                owned_by: format!("underclass-{}", backend.as_str()),
            };
            match merged.get_mut(&m.id) {
                Some(existing) => {
                    // Same id on several backends: the widest context window wins, so the caller
                    // sees the most capable account's limits (and its owner).
                    if m.context > existing.context {
                        *existing = entry;
                    }
                }
                None => {
                    merged.insert(m.id.clone(), entry);
                }
            }
        }
    }

    // Deterministic order: `aliases` is a HashMap.
    let mut names: Vec<&String> = aliases.keys().collect();
    names.sort();
    for alias in names {
        let mut context = 0i64;
        let mut input = 0i64;
        let mut output = 0i64;
        let mut servable_concretes = 0usize;
        for (backend, concrete) in &aliases[alias] {
            if !servable.contains(backend) {
                continue;
            }
            let Some(models) = catalogs.get(backend) else {
                continue;
            };
            let Some(m) = models.iter().find(|m| m.id == *concrete) else {
                continue;
            };
            servable_concretes += 1;
            context = context.max(m.context);
            input = input.max(m.input);
            output = output.max(m.output);
        }
        if servable_concretes == 0 {
            continue;
        }
        merged.entry(alias.clone()).or_insert(AdvertisedModel {
            id: alias.clone(),
            name: alias.clone(),
            context,
            input,
            output,
            owned_by: "underclass-alias".to_string(),
        });
    }

    merged.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AccountStatus;

    fn model(id: &str, context: i64) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            context,
            input: context,
            output: context,
            scope: String::new(),
        }
    }

    fn account(backend: BackendId, status: AccountStatus) -> Account {
        Account {
            id: format!("{}-1", backend.as_str()),
            backend,
            label: "operator@example.test".into(),
            refresh_token: None,
            access_token: Some("tok".into()),
            expires_at: 0,
            token_refreshed_at: 0,
            account_id: None,
            residency: None,
            enterprise_url: None,
            plan: None,
            status,
            disabled_reason: None,
            reset_at: 0,
            scope_cooling: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn catalogs(entries: &[(BackendId, Vec<ModelInfo>)]) -> HashMap<BackendId, Vec<ModelInfo>> {
        entries.iter().cloned().collect()
    }

    fn ids(models: &[AdvertisedModel]) -> Vec<String> {
        models.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn row_on_backend_without_account_is_not_advertised() {
        let cats = catalogs(&[
            (BackendId::Copilot, vec![model("grok-4.7", 200_000)]),
            (
                BackendId::OpencodeGo,
                vec![model("opencode-go/grok-4.7", 200_000)],
            ),
        ]);
        let servable = HashSet::from([BackendId::OpencodeGo]);
        let listed = advertised_models(&cats, &servable, &AliasTable::new());
        assert_eq!(ids(&listed), vec!["opencode-go/grok-4.7".to_string()]);
    }

    #[test]
    fn namespaced_model_on_backend_with_account_is_advertised() {
        let cats = catalogs(&[(
            BackendId::Cline,
            vec![model("cline/cline-pass/kimi-k3", 500_000)],
        )]);
        let servable = HashSet::from([BackendId::Cline]);
        let listed = advertised_models(&cats, &servable, &AliasTable::new());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "cline/cline-pass/kimi-k3");
        assert_eq!(listed[0].owned_by, "underclass-cline");
        assert_eq!(listed[0].context, 500_000);
    }

    #[test]
    fn alias_is_advertised_only_while_a_concrete_is_servable() {
        let cats = catalogs(&[(
            BackendId::OpencodeGo,
            vec![model("opencode-go/grok-4.7", 256_000)],
        )]);
        let mut aliases = AliasTable::new();
        aliases.insert(
            "subscriptions/grok-4.7".into(),
            vec![
                (BackendId::Copilot, "grok-4.7".to_string()),
                (BackendId::OpencodeGo, "opencode-go/grok-4.7".to_string()),
            ],
        );

        let with_go = advertised_models(&cats, &HashSet::from([BackendId::OpencodeGo]), &aliases);
        let entry = with_go
            .iter()
            .find(|m| m.id == "subscriptions/grok-4.7")
            .expect("alias with a servable concrete must be advertised");
        assert_eq!(entry.owned_by, "underclass-alias");
        assert_eq!(entry.context, 256_000);

        let without_go = advertised_models(&cats, &Default::default(), &aliases);
        assert!(
            !ids(&without_go)
                .iter()
                .any(|id| id == "subscriptions/grok-4.7"),
            "alias with no servable concrete must be omitted"
        );
    }

    #[test]
    fn seeded_catalogs_without_accounts_produce_no_entries() {
        let cats = catalogs(&[
            (BackendId::Codex, crate::codex::default_catalog()),
            (BackendId::Claude, crate::claude::default_catalog()),
            (
                BackendId::Copilot,
                vec![model("grok-4.6", 0), model("grok-4.7", 0)],
            ),
        ]);
        let listed = advertised_models(&cats, &Default::default(), &AliasTable::new());
        assert!(listed.is_empty(), "no accounts means nothing is advertised");
    }

    #[test]
    fn account_status_does_not_affect_configured_backends() {
        let accounts = [
            account(BackendId::Cline, AccountStatus::Cooling),
            account(BackendId::OpencodeGo, AccountStatus::AuthError),
            account(BackendId::Codex, AccountStatus::Disabled),
        ];
        assert_eq!(
            backends_with_accounts(&accounts),
            HashSet::from([BackendId::Codex, BackendId::Cline, BackendId::OpencodeGo])
        );
    }

    #[test]
    fn widest_context_wins_and_alias_yields_to_concrete_id() {
        let cats = catalogs(&[
            (BackendId::Codex, vec![model("shared", 100)]),
            (BackendId::Copilot, vec![model("shared", 400)]),
        ]);
        let servable = HashSet::from([BackendId::Codex, BackendId::Copilot]);
        let mut aliases = AliasTable::new();
        aliases.insert(
            "shared".into(),
            vec![(BackendId::Codex, "shared".to_string())],
        );
        let listed = advertised_models(&cats, &servable, &aliases);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].context, 400);
        assert_eq!(listed[0].owned_by, "underclass-copilot");
    }

    #[test]
    fn every_backend_with_an_account_is_considered() {
        let cats: HashMap<BackendId, Vec<ModelInfo>> = BackendId::all()
            .iter()
            .map(|b| (*b, vec![model(&format!("{}/m", b.as_str()), 1)]))
            .collect();
        let listed = advertised_models(
            &cats,
            &BackendId::all().into_iter().collect(),
            &AliasTable::new(),
        );
        assert_eq!(listed.len(), BackendId::all().len());
    }
}
