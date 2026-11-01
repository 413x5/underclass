//! models.dev: the curated capability feed underclass reads for models its own backends publish
//! without specs.
//!
//! It is an **advisory spec source, never a set source**. The set of cline-pass models comes from
//! Cline's public recommended-models feed; models.dev only fills in `limit`, `cost`, `modalities`,
//! `reasoning_options`, `tool_call`, `structured_output` and `knowledge` for ids that set names.
//! It is also stale in three measured ways that must not be inherited - see
//! `docs/adr/fork/model-metadata-sources.md` - so its `status` is parsed and reported but never
//! applied as a retirement (Cline's own feed decides that), an id models.dev does not know is not
//! removed, and an id models.dev omits but its backend publishes is kept.
//!
//! Nothing here is on the request path: a fetch feeds the catalog refresh, and a failed fetch
//! leaves the stored catalog untouched.

use crate::models::{
    Modalities, ModelCost, ModelInfo, ModelMetadata, ModelStatus, ReasoningCapability,
};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

/// The whole curated database, as one document keyed by provider id.
pub const FEED_URL: &str = "https://models.dev/api.json";
/// The provider whose models describe the ClinePass subscription.
pub const CLINE_PASS_PROVIDER: &str = "cline-pass";

pub fn feed_url() -> String {
    std::env::var("UNDERCLASS_MODELS_DEV_URL").unwrap_or_else(|_| FEED_URL.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("the corpus has no usable `{0}` provider")]
    MissingProvider(String),
}

/// @cc [owner:ghuntley,label:pool] models-dev-projection
/// `parse_provider` MUST return one namespaced `ModelInfo` per model in `provider` for which a
/// limit, a capability or a status is published, MUST use models.dev's `limit.context` and its
/// `limit.output`, MUST record `input = context` when models.dev carries no `limit.input` (it never
/// does for `cline-pass`), MUST translate `reasoning_options` into the effort ladder where the only
/// option type is `effort` and leave it empty for a `toggle`, and MUST NOT panic on a malformed
/// document. `name` and `namespace` are what turn a provider's bare id into a catalog id.
pub fn parse_provider(
    document: &Value,
    provider: &str,
    namespace: &str,
    prefix: &str,
    scope: &str,
) -> Vec<ModelInfo> {
    let Some(models) = document
        .get(provider)
        .and_then(|p| p.get("models"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, model) in models {
        let id = model
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or(key);
        // Only ids in the namespace this feed is authoritative for are projected; a provider that
        // later grows a bucket of ours is not silently adopted.
        if !namespace.is_empty() && !id.starts_with(namespace) {
            continue;
        }
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(id);
        let limit = model.get("limit");
        let published = |key: &str| {
            limit
                .and_then(|l| l.get(key))
                .and_then(Value::as_i64)
                .filter(|value| *value > 0)
        };
        let context = published("context").unwrap_or(0);
        let mut info = ModelInfo {
            id: format!("{prefix}{id}"),
            name: name.to_string(),
            context,
            input: published("input").unwrap_or(0),
            output: published("output").unwrap_or(0),
            scope: scope.to_string(),
            metadata: ModelMetadata::default(),
        };
        info.input_from_context();
        info.metadata = metadata_for(model);
        out.push(info);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn metadata_for(model: &Value) -> ModelMetadata {
    let mut metadata = ModelMetadata::default();
    if let Some(reasoning) = model.get("reasoning").and_then(Value::as_bool) {
        metadata.reasoning = Some(ReasoningCapability {
            supported: reasoning,
            effort: effort_ladder(model.get("reasoning_options")),
        });
    }
    metadata.tools = model.get("tool_call").and_then(Value::as_bool);
    metadata.structured_output = model.get("structured_output").and_then(Value::as_bool);
    metadata.knowledge = model
        .get("knowledge")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|knowledge| !knowledge.is_empty())
        .map(str::to_string);
    if let Some(modalities) = model.get("modalities") {
        let names = |key: &str| {
            modalities
                .get(key)
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        metadata.modalities = Some(Modalities {
            input: names("input"),
            output: names("output"),
        });
    }
    if let Some(cost) = model.get("cost") {
        let price = |key: &str| cost.get(key).and_then(Value::as_f64);
        let cost = ModelCost {
            input: price("input"),
            output: price("output"),
            cache_read: price("cache_read"),
            cache_write: price("cache_write"),
        };
        metadata.prompt_cache = Some(cost.cache_read.is_some() || cost.cache_write.is_some());
        if cost != ModelCost::default() {
            metadata.cost = Some(cost);
        }
    }
    if model.get("status").and_then(Value::as_str) == Some("deprecated") {
        metadata.status = ModelStatus::Deprecated;
    }
    metadata
}

/// The effort ladder on a models.dev model. `effort` supplies the levels and any other option type
/// (`toggle`) supplies none, because a toggle model has no levels to publish.
fn effort_ladder(options: Option<&Value>) -> Vec<String> {
    let Some(options) = options.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ladder: Vec<String> = Vec::new();
    for option in options {
        if option.get("type").and_then(Value::as_str) != Some("effort") {
            continue;
        }
        let Some(values) = option.get("values").and_then(Value::as_array) else {
            continue;
        };
        for level in values.iter().filter_map(Value::as_str) {
            if !ladder.iter().any(|known| known == level) {
                ladder.push(level.to_string());
            }
        }
    }
    ladder
}

/// @cc [owner:ghuntley,label:pool] models-dev-read-failure-is-an-error
/// `fetch` MUST fail - returning `Err` and no partial document - on a non-2xx answer, a body that is
/// not JSON, or a JSON value without the provider it reads, so a caller can never mistake a failed
/// read for an empty or retired corpus. It MUST NOT retry, log a body, or write anything: the
/// caller decides what a failure means for the stored catalog, which is to leave it alone. `url` is
/// a parameter so a test can point it at a guaranteed failure without touching the environment.
pub async fn fetch(
    url: &str,
    cline_pass: &str,
    user_agent: Option<&str>,
) -> Result<Value, FeedError> {
    let resp = Client::new()
        .get(url)
        .timeout(Duration::from_secs(30))
        .header(
            http::header::USER_AGENT,
            crate::user_agent::resolve(None, user_agent),
        )
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(FeedError::Status(resp.status().as_u16()));
    }
    let document: Value = resp.json().await?;
    // The corpus must have the provider *and* a usable `models` object. A missing provider, or a
    // `models` member that is null or a string, is a failed read, not an empty corpus: an empty
    // result would be indistinguishable from "this provider publishes nothing", and the caller
    // would skip enrichment without ever logging why.
    if document
        .get(cline_pass)
        .and_then(|p| p.get("models"))
        .and_then(Value::as_object)
        .is_none()
    {
        return Err(FeedError::MissingProvider(cline_pass.to_string()));
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/models-dev-cline-pass.json"))
            .expect("checked-in fixture parses")
    }

    fn cline_pass() -> Vec<ModelInfo> {
        parse_provider(
            &fixture(),
            CLINE_PASS_PROVIDER,
            "cline-pass/",
            "cline/",
            "cline-pass",
        )
    }

    fn row<'a>(models: &'a [ModelInfo], id: &str) -> &'a ModelInfo {
        models
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
    }

    #[test]
    fn a_full_cline_pass_row_is_projected() {
        let models = cline_pass();
        let flash = row(&models, "cline/cline-pass/deepseek-v4.1-flash");
        assert_eq!(flash.name, "DeepSeek V4.1 Flash");
        assert_eq!(flash.context, 1_000_000);
        assert_eq!(
            flash.input, 1_000_000,
            "models.dev publishes no limit.input, so input equals context"
        );
        assert_eq!(flash.output, 384_000);
        assert_eq!(flash.scope, "cline-pass");
        let metadata = &flash.metadata;
        assert_eq!(
            metadata.reasoning,
            Some(ReasoningCapability {
                supported: true,
                effort: vec!["none".into(), "low".into(), "high".into(), "max".into()],
            }),
            "only the documented graded ladder is advertised"
        );
        assert_eq!(metadata.tools, Some(true));
        let modalities = metadata.modalities.as_ref().expect("modalities");
        assert_eq!(modalities.input, vec!["text", "image"]);
        let cost = metadata.cost.as_ref().expect("cost");
        assert_eq!(cost.input, Some(0.15));
        assert_eq!(cost.output, Some(0.6));
        assert_eq!(cost.cache_read, Some(0.003));
        assert_eq!(metadata.prompt_cache, Some(true));
        assert_eq!(metadata.status, ModelStatus::Available);
    }

    #[test]
    fn a_toggle_model_records_no_effort_ladder() {
        let models = cline_pass();
        let minimax = row(&models, "cline/cline-pass/minimax-m3");
        let reasoning = minimax.metadata.reasoning.as_ref().expect("reasoning");
        assert!(reasoning.supported);
        assert!(
            reasoning.effort.is_empty(),
            "a toggle-only model publishes no levels"
        );
    }

    #[test]
    fn models_dev_deprecations_are_recorded() {
        let models = cline_pass();
        for id in [
            "cline/cline-pass/glm-5.2",
            "cline/cline-pass/deepseek-v4-flash",
            "cline/cline-pass/kimi-k2.6",
            "cline/cline-pass/kimi-k2.7-code",
        ] {
            assert_eq!(
                row(&models, id).metadata.status,
                ModelStatus::Deprecated,
                "{id} is marked deprecated upstream"
            );
        }
        assert_eq!(
            row(&models, "cline/cline-pass/mimo-v2.5").metadata.status,
            ModelStatus::Available,
            "models.dev does not mark it, so this read records nothing either"
        );
    }

    #[test]
    fn ids_outside_the_namespace_are_ignored() {
        let document = serde_json::json!({"cline-pass": {"models": {
            "cline-pass/kimi-k3": {"id": "cline-pass/kimi-k3", "limit": {"context": 10}},
            "anthropic/claude-sonnet-4-6": {"id": "anthropic/claude-sonnet-4-6"}
        }}});
        let models = parse_provider(
            &document,
            CLINE_PASS_PROVIDER,
            "cline-pass/",
            "cline/",
            "cline-pass",
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "cline/cline-pass/kimi-k3");
    }

    /// A non-2xx answer, a connection failure, or a body without the provider is an error, never
    /// an empty corpus - a caller that confused the two would retire every cline-pass model.
    #[tokio::test]
    async fn a_failed_read_is_an_error_not_an_empty_corpus() {
        for url in ["http://127.0.0.1:9/api.json", "not a url at all"] {
            assert!(
                fetch(url, CLINE_PASS_PROVIDER, None).await.is_err(),
                "{url} must not read as an empty corpus"
            );
        }
    }

    /// A well-formed corpus that lacks the provider underclass reads is a *missing provider*, not a
    /// status failure: the HTTP read succeeded, and the message must say so.
    #[test]
    fn a_document_without_the_provider_reports_the_missing_provider() {
        let error = FeedError::MissingProvider(CLINE_PASS_PROVIDER.to_string());
        assert_eq!(
            error.to_string(),
            "the corpus has no usable `cline-pass` provider",
            "the message must name the cause rather than a phantom status"
        );
        // A provider whose `models` member is not an object is the same failed read: an empty
        // vector there would skip enrichment without a warning.
        for document in [
            serde_json::json!({"opencode-go": {"models": {}}}),
            serde_json::json!({"cline-pass": {"models": null}}),
            serde_json::json!({"cline-pass": {"models": "nope"}}),
        ] {
            assert!(
                document
                    .get(CLINE_PASS_PROVIDER)
                    .and_then(|p| p.get("models"))
                    .and_then(Value::as_object)
                    .is_none(),
                "the same check `fetch` applies: {document}"
            );
        }
    }

    #[test]
    fn a_missing_provider_or_malformed_document_yields_nothing() {
        for document in [
            serde_json::json!({}),
            serde_json::json!({"cline-pass": null}),
            serde_json::json!({"cline-pass": {"models": "nope"}}),
            serde_json::json!({"kimi-code-plan-global": {"models": {}}}),
        ] {
            assert!(
                parse_provider(
                    &document,
                    CLINE_PASS_PROVIDER,
                    "cline-pass/",
                    "cline/",
                    "cline-pass"
                )
                .is_empty()
            );
        }
    }
}
