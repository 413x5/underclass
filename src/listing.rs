//! What `/v1/models` advertises: the catalogs of backends that actually have an account, plus
//! the aliases whose concrete models survive that filter.
//!
//! The stored catalog is deliberately *not* pruned when an account is removed: it is the record of
//! what the backend published, and re-adding an account restores its models unchanged. The listing
//! is therefore a read-time view over the store, not a copy of it.

use crate::aliases::AliasTable;
use crate::models::{
    Account, BackendId, Modalities, ModelCost, ModelInfo, ModelMetadata, ModelStatus,
    ReasoningCapability, WireApi,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// One entry of the `GET /v1/models` `data` array.
///
/// Limits are `Option<i64>` so an unpublished limit stays `None` and reaches the wire as `null`
/// rather than as `0`: a client must never compute a zero budget from "we do not know".
#[derive(Clone, Debug, PartialEq)]
pub struct AdvertisedModel {
    pub id: String,
    pub name: String,
    pub context: Option<i64>,
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub owned_by: String,
    /// Quota scope the model bills against (`credits`, `cline-pass`), or `None` when the whole
    /// account shares one quota.
    pub scope: Option<String>,
    pub metadata: ListingMetadata,
}

/// The `metadata` object of a listing entry: an explicit projection of the stored block, so the
/// wire contract cannot drift when an internal field is added or renamed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ListingMetadata {
    pub reasoning: Option<ReasoningCapability>,
    pub tools: Option<bool>,
    pub structured_output: Option<bool>,
    pub modalities: Option<Modalities>,
    pub cost: Option<ModelCost>,
    pub prompt_cache: Option<bool>,
    pub knowledge: Option<String>,
    pub wire: Option<WireApi>,
    pub reasoning_field: Option<String>,
    /// `available` or `deprecated`. A deprecated row is never advertised at all, so this reads
    /// `available` on the wire today; it is emitted so a client reads one field either way.
    pub status: ModelStatus,
}

impl ListingMetadata {
    /// Fill every field this projection does not know from `other`: two backends serving one id
    /// describe the same model, so a capability either of them published is true of the id, and the
    /// entry that wins on limits must not erase what the loser published.
    fn fill_from(&mut self, other: &ListingMetadata) {
        if self.reasoning.is_none() {
            self.reasoning = other.reasoning.clone();
        }
        if self.tools.is_none() {
            self.tools = other.tools;
        }
        if self.structured_output.is_none() {
            self.structured_output = other.structured_output;
        }
        if self.modalities.is_none() {
            self.modalities = other.modalities.clone();
        }
        if self.cost.is_none() {
            self.cost = other.cost.clone();
        }
        if self.prompt_cache.is_none() {
            self.prompt_cache = other.prompt_cache;
        }
        if self.knowledge.is_none() {
            self.knowledge = other.knowledge.clone();
        }
        if self.wire.is_none() {
            self.wire = other.wire;
        }
        if self.reasoning_field.is_none() {
            self.reasoning_field = other.reasoning_field.clone();
        }
    }

    fn from_model(metadata: &ModelMetadata) -> Self {
        Self {
            reasoning: metadata.reasoning.clone(),
            tools: metadata.tools,
            structured_output: metadata.structured_output,
            modalities: metadata.modalities.clone(),
            cost: metadata.cost.clone(),
            prompt_cache: metadata.prompt_cache,
            knowledge: metadata.knowledge.clone(),
            wire: metadata.wire,
            reasoning_field: metadata.reasoning_field.clone(),
            status: metadata.status,
        }
    }
}

/// What a client sees of an entry. Serialized field by field so the seven keys underclass has
/// always emitted keep their names and meanings, and every new key is additive. The three limit
/// keys are never omitted: an unpublished value is JSON `null`.
#[derive(Clone, Debug, Serialize)]
struct WireModel<'a> {
    id: &'a str,
    object: &'static str,
    name: &'a str,
    context_window: Option<i64>,
    max_input_tokens: Option<i64>,
    max_output_tokens: Option<i64>,
    owned_by: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    metadata: &'a ListingMetadata,
}

impl AdvertisedModel {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(WireModel {
            id: &self.id,
            object: "model",
            name: &self.name,
            context_window: self.context,
            max_input_tokens: self.input,
            max_output_tokens: self.output,
            owned_by: &self.owned_by,
            scope: self.scope.as_deref(),
            metadata: &self.metadata,
        })
        .expect("listing entry serializes")
    }
}

/// @cc [owner:ghuntley,label:proxy] configured-backend-set
/// `backends_with_accounts` MUST treat an account as configured regardless of its `status`, so a
/// cooling, disabled, or auth-error account still makes its backend's models advertised.
pub fn backends_with_accounts(accounts: &[Account]) -> HashSet<BackendId> {
    accounts.iter().map(|a| a.backend).collect()
}

/// @cc [owner:ghuntley,label:proxy] unknown-limits-not-zero
/// A published limit MUST be `Some` only when a source reported it; a stored `0` - the value every
/// backend uses for "unpublished" - MUST become `None`, and `context_window`,
/// `max_input_tokens`, `max_output_tokens` MUST stay present on every entry as JSON `null` in that
/// case, so a client tells "unknown" from "zero capacity" without probing for the key.
///
/// An alias MUST compute each limit as the widest published value among its servable concretes,
/// and MUST be `None` only when none of them published one: an unknown concrete neither wins nor
/// drags a known sibling down to `None`.
fn published_limit(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

/// The widest published limit across several sources: `None` when none of them reported one.
fn widest_published(values: impl Iterator<Item = Option<i64>>) -> Option<i64> {
    values.flatten().max()
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
/// A row whose recorded `status` is `deprecated` MUST NOT be advertised, and MUST NOT keep an alias
/// alive or contribute to its limits: a retired model is not something an account can serve.
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
        for m in models.iter().filter(|m| !m.is_deprecated()) {
            let mut entry = AdvertisedModel {
                id: m.id.clone(),
                name: m.name.clone(),
                context: published_limit(m.context),
                input: published_limit(m.input),
                output: published_limit(m.output),
                owned_by: format!("underclass-{}", backend.as_str()),
                scope: (!m.scope.is_empty()).then(|| m.scope.clone()),
                metadata: ListingMetadata::from_model(&m.metadata),
            };
            match merged.get_mut(&m.id) {
                Some(existing) => {
                    // Same id on several backends: the widest context window wins, so the caller
                    // sees the most capable account's limits (and its owner). What either backend
                    // published about the model is not discarded by that choice - a capability
                    // reported by any backend serving the id is true of the id - so the union of
                    // the two blocks goes onto whichever entry survives, in either order.
                    if entry.context.unwrap_or(0) > existing.context.unwrap_or(0) {
                        entry.metadata.fill_from(&existing.metadata);
                        *existing = entry;
                    } else {
                        existing.metadata.fill_from(&entry.metadata);
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
        let mut contexts = Vec::new();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut sources: Vec<&ModelInfo> = Vec::new();
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
            if m.is_deprecated() {
                continue;
            }
            contexts.push(published_limit(m.context));
            inputs.push(published_limit(m.input));
            outputs.push(published_limit(m.output));
            sources.push(m);
        }
        if sources.is_empty() {
            continue;
        }
        merged.entry(alias.clone()).or_insert(AdvertisedModel {
            id: alias.clone(),
            name: alias.clone(),
            context: widest_published(contexts.into_iter()),
            input: widest_published(inputs.into_iter()),
            output: widest_published(outputs.into_iter()),
            owned_by: "underclass-alias".to_string(),
            scope: shared_scope(&sources),
            metadata: alias_metadata(&sources),
        });
    }

    merged.into_values().collect()
}

/// The billing surface an alias advertises: the scope every servable concrete agrees on, or `None`
/// when they disagree. An absent `scope` means the whole account shares one quota, so picking one
/// concrete's surface would be wrong for the others; a narrow alias whose concretes all bill the
/// same subscription should still say so.
fn shared_scope(sources: &[&ModelInfo]) -> Option<String> {
    let first = sources.first()?.scope.clone();
    sources
        .iter()
        .all(|m| m.scope == first)
        .then_some(first)
        .filter(|scope| !scope.is_empty())
}

/// The metadata an alias advertises: reasoning and tools are the union across its concretes (a
/// client may send an effort level any concrete accepts), while everything that describes one
/// model - cost, modalities, knowledge, wire - is omitted rather than picked from a sibling,
/// because an alias spans several models and no single value would be true of all of them.
fn alias_metadata(sources: &[&ModelInfo]) -> ListingMetadata {
    let mut reasoning_effort: Vec<String> = Vec::new();
    let mut reasoning_supported = false;
    let mut tools: Option<bool> = None;
    for m in sources {
        if let Some(reasoning) = &m.metadata.reasoning {
            reasoning_supported |= reasoning.supported;
            for level in &reasoning.effort {
                if !reasoning_effort.contains(level) {
                    reasoning_effort.push(level.clone());
                }
            }
        }
        if let Some(supported) = m.metadata.tools {
            tools = Some(tools.unwrap_or(false) || supported);
        }
    }
    ListingMetadata {
        reasoning: reasoning_supported.then_some(ReasoningCapability {
            supported: true,
            effort: reasoning_effort,
        }),
        tools,
        status: ModelStatus::Available,
        ..Default::default()
    }
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
            metadata: Default::default(),
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

    fn kimi_alias() -> AliasTable {
        let mut aliases = AliasTable::new();
        aliases.insert(
            "subscriptions/kimi-k3".into(),
            vec![
                (BackendId::OpencodeGo, "opencode-go/kimi-k3".to_string()),
                (BackendId::Cline, "cline/cline-pass/kimi-k3".to_string()),
            ],
        );
        aliases
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
        assert_eq!(listed[0].context, Some(500_000));
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
        assert_eq!(entry.context, Some(256_000));

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
        assert_eq!(listed[0].context, Some(400));
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

    #[test]
    fn unpublished_limits_serialize_as_null_and_keep_the_key() {
        let cats = catalogs(&[(BackendId::Cline, vec![model("cline/cline-pass/kimi-k3", 0)])]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline]),
            &AliasTable::new(),
        );
        let json = listed[0].to_json();
        for key in ["context_window", "max_input_tokens", "max_output_tokens"] {
            assert!(
                json.get(key).is_some(),
                "{key} must stay present so a client can destructure it"
            );
            assert_eq!(
                json[key],
                serde_json::Value::Null,
                "{key} must be null, not 0"
            );
        }
    }

    #[test]
    fn an_alias_ignores_unknown_limits_and_keeps_the_known_ones() {
        let unknown = model("opencode-go/kimi-k3", 0);
        let mut known = model("cline/cline-pass/kimi-k3", 200_000);
        known.output = 32_000;
        let cats = catalogs(&[
            (BackendId::OpencodeGo, vec![unknown]),
            (BackendId::Cline, vec![known]),
        ]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline, BackendId::OpencodeGo]),
            &kimi_alias(),
        );
        let alias = listed
            .iter()
            .find(|m| m.id == "subscriptions/kimi-k3")
            .expect("alias advertised");
        assert_eq!(
            alias.context,
            Some(200_000),
            "an unknown concrete must not drag the alias to null"
        );
        let json = alias.to_json();
        assert_eq!(json["max_input_tokens"], 200_000);
        assert_eq!(json["max_output_tokens"], 32_000);
    }

    #[test]
    fn an_alias_with_no_published_limit_anywhere_is_null() {
        let cats = catalogs(&[
            (BackendId::OpencodeGo, vec![model("opencode-go/kimi-k3", 0)]),
            (BackendId::Cline, vec![model("cline/cline-pass/kimi-k3", 0)]),
        ]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline, BackendId::OpencodeGo]),
            &kimi_alias(),
        );
        let alias = listed
            .iter()
            .find(|m| m.id == "subscriptions/kimi-k3")
            .expect("alias advertised");
        assert_eq!(alias.context, None);
        assert_eq!(alias.input, None);
        assert_eq!(alias.output, None);
    }

    /// Two backends serving one id describe the same model: the entry that wins on context width
    /// must not erase what the other one published about it.
    #[test]
    fn the_widest_context_winner_keeps_the_losers_metadata() {
        let mut narrow = model("shared", 100);
        narrow.metadata.reasoning = Some(ReasoningCapability {
            supported: true,
            effort: vec!["high".into()],
        });
        narrow.metadata.tools = Some(true);
        let wide = model("shared", 400);
        assert_eq!(
            wide.metadata,
            Default::default(),
            "the wider row published nothing"
        );
        let cats = catalogs(&[
            (BackendId::Codex, vec![narrow]),
            (BackendId::Copilot, vec![wide]),
        ]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Codex, BackendId::Copilot]),
            &AliasTable::new(),
        );
        assert_eq!(listed.len(), 1);
        let entry = &listed[0];
        assert_eq!(
            entry.owned_by, "underclass-copilot",
            "the wider context wins"
        );
        assert_eq!(entry.context, Some(400));
        assert_eq!(
            entry.metadata.reasoning.as_ref().map(|r| r.effort.clone()),
            Some(vec!["high".to_string()]),
            "a capability the losing backend published must survive the winner"
        );
        assert_eq!(entry.metadata.tools, Some(true));
    }

    /// The same union in the other order: the wide row is merged first and carries no metadata, so
    /// the narrower row that publishes one must still contribute it.
    #[test]
    fn the_widest_context_winner_keeps_metadata_from_a_later_narrower_row() {
        let wide = model("shared", 400);
        assert_eq!(wide.metadata, Default::default());
        let mut narrow = model("shared", 100);
        narrow.metadata.reasoning = Some(ReasoningCapability {
            supported: true,
            effort: vec!["max".into()],
        });
        narrow.metadata.knowledge = Some("2026-01".into());
        let cats = catalogs(&[
            (BackendId::Codex, vec![wide]),
            (BackendId::Copilot, vec![narrow]),
        ]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Codex, BackendId::Copilot]),
            &AliasTable::new(),
        );
        assert_eq!(listed.len(), 1);
        let entry = &listed[0];
        assert_eq!(entry.context, Some(400), "the wider context still wins");
        assert_eq!(entry.owned_by, "underclass-codex");
        assert_eq!(
            entry.metadata.reasoning.as_ref().map(|r| r.effort.clone()),
            Some(vec!["max".to_string()]),
            "a capability from the later, narrower row must survive"
        );
        assert_eq!(entry.metadata.knowledge.as_deref(), Some("2026-01"));
    }

    #[test]
    fn a_deprecated_model_is_never_advertised() {
        let mut retired = model("cline/cline-pass/mimo-v2.5", 1_048_576);
        retired.metadata.status = ModelStatus::Deprecated;
        let cats = catalogs(&[(
            BackendId::Cline,
            vec![retired, model("cline/cline-pass/kimi-k3", 1_048_576)],
        )]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline]),
            &AliasTable::new(),
        );
        assert_eq!(ids(&listed), vec!["cline/cline-pass/kimi-k3".to_string()]);
    }

    #[test]
    fn an_alias_whose_only_concrete_is_deprecated_is_not_advertised() {
        let mut retired = model("cline/cline-pass/mimo-v2.5", 1_048_576);
        retired.metadata.status = ModelStatus::Deprecated;
        let cats = catalogs(&[(
            BackendId::Cline,
            vec![retired, model("cline/cline-pass/kimi-k3", 1_048_576)],
        )]);
        let mut aliases = AliasTable::new();
        aliases.insert(
            "subscriptions/mimo-v2.5".into(),
            vec![(BackendId::Cline, "cline/cline-pass/mimo-v2.5".to_string())],
        );
        aliases.insert(
            "subscriptions/kimi-k3".into(),
            vec![(BackendId::Cline, "cline/cline-pass/kimi-k3".to_string())],
        );
        let listed = advertised_models(&cats, &HashSet::from([BackendId::Cline]), &aliases);
        assert!(
            ids(&listed)
                .iter()
                .all(|id| id != "subscriptions/mimo-v2.5"),
            "an alias whose every concrete is retired must not be advertised"
        );
        assert!(ids(&listed).iter().any(|id| id == "subscriptions/kimi-k3"));
        // Nor does the retired row keep an otherwise servable alias alive on its own.
        let retired_only = catalogs(&[(
            BackendId::Cline,
            vec![{
                let mut m = model("cline/cline-pass/mimo-v2.5", 1_048_576);
                m.metadata.status = ModelStatus::Deprecated;
                m
            }],
        )]);
        let listed = advertised_models(&retired_only, &HashSet::from([BackendId::Cline]), &aliases);
        assert!(
            ids(&listed)
                .iter()
                .all(|id| id != "subscriptions/mimo-v2.5")
        );
    }

    #[test]
    fn the_original_seven_keys_keep_their_meaning() {
        let cats = catalogs(&[(
            BackendId::Cline,
            vec![{
                let mut m = model("cline/cline-pass/kimi-k3", 262_144);
                m.name = "Kimi K3".into();
                m.input = 200_000;
                m.output = 32_000;
                m
            }],
        )]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline]),
            &AliasTable::new(),
        );
        let json = listed[0].to_json();
        assert_eq!(json["id"], "cline/cline-pass/kimi-k3");
        assert_eq!(json["object"], "model");
        assert_eq!(json["name"], "Kimi K3");
        assert_eq!(json["context_window"], 262_144);
        assert_eq!(json["max_input_tokens"], 200_000);
        assert_eq!(json["max_output_tokens"], 32_000);
        assert_eq!(json["owned_by"], "underclass-cline");
    }

    #[test]
    fn scope_reaches_the_listing_and_an_empty_scope_is_omitted() {
        let mut pass = model("cline/cline-pass/kimi-k3", 262_144);
        pass.scope = "cline-pass".into();
        let mut credits = model("cline/openai/gpt-5.4", 128_000);
        credits.scope = "credits".into();
        let cats = catalogs(&[
            (BackendId::Cline, vec![pass, credits]),
            (BackendId::Codex, vec![model("gpt-5.5", 400_000)]),
        ]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline, BackendId::Codex]),
            &AliasTable::new(),
        );
        let json = |id: &str| {
            listed
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .to_json()
        };
        assert_eq!(json("cline/cline-pass/kimi-k3")["scope"], "cline-pass");
        assert_eq!(json("cline/openai/gpt-5.4")["scope"], "credits");
        assert!(
            json("gpt-5.5").get("scope").is_none(),
            "an account-wide row names no billing surface"
        );
    }

    #[test]
    fn an_alias_advertises_the_scope_its_concretes_agree_on() {
        let mut pass = model("cline/cline-pass/kimi-k3", 1_048_576);
        pass.scope = "cline-pass".into();
        let cats = catalogs(&[(BackendId::Cline, vec![pass])]);
        let mut aliases = AliasTable::new();
        aliases.insert(
            "subscriptions/kimi-k3".into(),
            vec![(BackendId::Cline, "cline/cline-pass/kimi-k3".to_string())],
        );
        let listed = advertised_models(&cats, &HashSet::from([BackendId::Cline]), &aliases);
        let alias = listed
            .iter()
            .find(|m| m.id == "subscriptions/kimi-k3")
            .expect("alias advertised");
        assert_eq!(
            alias.to_json()["scope"],
            "cline-pass",
            "a narrow alias whose only concrete is a subscription model bills that surface"
        );

        // Concretes that bill differently cannot be described by one scope, so none is claimed.
        let mut wide = AliasTable::new();
        wide.insert(
            "subscriptions/kimi-k3".into(),
            vec![
                (BackendId::Cline, "cline/cline-pass/kimi-k3".to_string()),
                (
                    BackendId::Cline,
                    "cline/anthropic/claude-sonnet-4-6".to_string(),
                ),
            ],
        );
        let mixed = catalogs(&[(
            BackendId::Cline,
            vec![
                {
                    let mut m = model("cline/cline-pass/kimi-k3", 1_048_576);
                    m.scope = "cline-pass".into();
                    m
                },
                {
                    let mut m = model("cline/anthropic/claude-sonnet-4-6", 200_000);
                    m.scope = "credits".into();
                    m
                },
            ],
        )]);
        let listed = advertised_models(&mixed, &HashSet::from([BackendId::Cline]), &wide);
        let alias = listed
            .iter()
            .find(|m| m.id == "subscriptions/kimi-k3")
            .expect("alias advertised");
        assert!(
            alias.to_json().get("scope").is_none(),
            "a mixed alias names no single billing surface"
        );
    }

    #[test]
    fn a_full_metadata_row_reaches_the_wire() {
        let mut row = model("opencode-go/deepseek-v4.1-flash", 1_000_000);
        row.input = 1_000_000;
        row.output = 384_000;
        row.metadata = ModelMetadata {
            reasoning: Some(ReasoningCapability {
                supported: true,
                effort: vec!["low".into(), "high".into(), "max".into()],
            }),
            tools: Some(true),
            structured_output: Some(true),
            modalities: Some(Modalities {
                input: vec!["text".into(), "image/png".into()],
                output: vec!["text".into()],
            }),
            cost: Some(ModelCost {
                input: Some(0.15),
                output: Some(0.6),
                cache_read: Some(0.003),
                cache_write: None,
            }),
            prompt_cache: Some(true),
            knowledge: Some("2025-05".into()),
            wire: Some(WireApi::ChatCompletions),
            reasoning_field: Some("reasoning_content".into()),
            status: ModelStatus::Available,
        };
        let cats = catalogs(&[(BackendId::OpencodeGo, vec![row])]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::OpencodeGo]),
            &AliasTable::new(),
        );
        let json = listed[0].to_json();
        let metadata = &json["metadata"];
        assert_eq!(metadata["reasoning"]["supported"], true);
        assert_eq!(metadata["reasoning"]["effort"][2], "max");
        assert_eq!(metadata["tools"], true);
        assert_eq!(metadata["structured_output"], true);
        assert_eq!(metadata["modalities"]["input"][1], "image/png");
        assert_eq!(metadata["cost"]["input"], 0.15);
        assert_eq!(metadata["prompt_cache"], true);
        assert_eq!(metadata["knowledge"], "2025-05");
        assert_eq!(metadata["wire"], "chat-completions");
        assert_eq!(metadata["reasoning_field"], "reasoning_content");
        assert_eq!(metadata["status"], "available");
    }

    #[test]
    fn a_client_that_only_knows_the_original_seven_keys_still_parses_the_listing() {
        // The strongest available check that the new keys are additive: a struct with exactly the
        // original seven fields must deserialize an entry that carries all the new ones.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OldModel {
            id: String,
            object: String,
            name: String,
            context_window: Option<i64>,
            max_input_tokens: Option<i64>,
            max_output_tokens: Option<i64>,
            owned_by: String,
        }
        let mut row = model("opencode-go/kimi-k3", 1_048_576);
        row.metadata.wire = Some(WireApi::ChatCompletions);
        row.metadata.reasoning = Some(ReasoningCapability {
            supported: true,
            effort: vec!["max".into()],
        });
        let cats = catalogs(&[(BackendId::OpencodeGo, vec![row])]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::OpencodeGo]),
            &AliasTable::new(),
        );
        let body = serde_json::json!({
            "object": "list",
            "data": listed.iter().map(AdvertisedModel::to_json).collect::<Vec<_>>(),
        });
        let parsed: Vec<OldModel> = serde_json::from_value(body["data"].clone())
            .expect("the original seven keys must stay parseable");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "opencode-go/kimi-k3");
        assert_eq!(parsed[0].context_window, Some(1_048_576));
    }

    #[test]
    fn alias_metadata_is_the_union_of_what_its_concretes_published() {
        let mut one = model("opencode-go/kimi-k3", 1_048_576);
        one.metadata.reasoning = Some(ReasoningCapability {
            supported: true,
            effort: vec!["low".into(), "max".into()],
        });
        one.metadata.tools = Some(true);
        one.metadata.cost = Some(ModelCost {
            input: Some(3.0),
            ..Default::default()
        });
        let mut two = model("cline/cline-pass/kimi-k3", 1_048_576);
        two.metadata.reasoning = Some(ReasoningCapability {
            supported: true,
            effort: vec!["high".into(), "max".into()],
        });
        two.metadata.tools = Some(false);
        let cats = catalogs(&[
            (BackendId::OpencodeGo, vec![one]),
            (BackendId::Cline, vec![two]),
        ]);
        let listed = advertised_models(
            &cats,
            &HashSet::from([BackendId::Cline, BackendId::OpencodeGo]),
            &kimi_alias(),
        );
        let alias = listed
            .iter()
            .find(|m| m.id == "subscriptions/kimi-k3")
            .expect("alias advertised");
        let reasoning = alias.metadata.reasoning.as_ref().expect("reasoning union");
        assert_eq!(reasoning.effort, vec!["low", "max", "high"]);
        assert_eq!(
            alias.metadata.tools,
            Some(true),
            "any concrete accepting tools"
        );
        assert_eq!(
            alias.metadata.cost, None,
            "a cost that describes one concrete must not describe the alias"
        );
        assert_eq!(alias.metadata.wire, None);
    }
}
