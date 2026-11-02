use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendId {
    Codex,
    Copilot,
    Claude,
    Cline,
    OpencodeGo,
    Vercel,
}

impl BackendId {
    pub const fn as_str(self) -> &'static str {
        match self {
            BackendId::Codex => "codex",
            BackendId::Copilot => "copilot",
            BackendId::Claude => "claude",
            BackendId::Cline => "cline",
            BackendId::OpencodeGo => "opencode-go",
            BackendId::Vercel => "vercel",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "codex" => Some(BackendId::Codex),
            "copilot" => Some(BackendId::Copilot),
            "claude" => Some(BackendId::Claude),
            "cline" => Some(BackendId::Cline),
            "opencode_go" | "opencode-go" => Some(BackendId::OpencodeGo),
            "vercel" => Some(BackendId::Vercel),
            _ => None,
        }
    }

    /// Every backend, in the order routing falls back through. `Vercel` is last: an unknown
    /// model keeps landing on the first backend that serves the requested surface, so adding a
    /// backend cannot move an existing model's fallback.
    pub const fn all() -> [BackendId; 6] {
        [
            BackendId::Codex,
            BackendId::Copilot,
            BackendId::Claude,
            BackendId::Cline,
            BackendId::OpencodeGo,
            BackendId::Vercel,
        ]
    }

    /// Terminal cells the longest id occupies, so a UI column can be sized from the ids instead of
    /// a hardcoded guess: adding a backend or renaming one moves this with it. Counted in bytes,
    /// which equal cells because every id is ASCII (`BackendId::all().map(as_str)` is asserted so).
    ///
    /// @cc [owner:ghuntley,label:cli] backend-id-width-derived
    /// The value MUST be computed from `BackendId::all()` rather than written as a literal, so that
    /// adding a backend or renaming one changes the width instead of leaving a column cap that clips
    /// the new id.
    pub const fn max_str_width() -> usize {
        let ids = Self::all();
        let mut widest = 0;
        let mut i = 0;
        while i < ids.len() {
            let width = ids[i].as_str().len();
            if width > widest {
                widest = width;
            }
            i += 1;
        }
        widest
    }
}

/// The model name without a trailing parenthesised suffix or surrounding whitespace.
///
/// Claude Code appends a thinking suffix such as `(high)` that is not part of the model's
/// identity, so routing and accounting use the base name. The id is otherwise preserved: it is
/// matched against operator-editable catalogs, and changing its case would stop an entry with an
/// uppercase character from matching - the request would then fall through to the unknown-model
/// fallback instead of the intended backend.
pub fn normalize_model_name(model: &str) -> String {
    let trimmed = model.trim();
    match trimmed.rfind('(') {
        Some(open) if trimmed.ends_with(')') => trimmed[..open].trim().to_string(),
        _ => trimmed.to_string(),
    }
}

/// The wire protocol a client speaks. Backends declare which surfaces they can serve, so the
/// pool never routes an Anthropic-shaped request to a backend that only speaks OpenAI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// OpenAI-shaped: `/v1/responses`, `/v1/chat/completions`.
    OpenAI,
    /// Anthropic Messages API: `/v1/messages`, `/v1/messages/count_tokens`.
    Anthropic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Healthy,
    Cooling,
    AuthError,
    Disabled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub backend: BackendId,
    pub label: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub expires_at: i64,
    /// Wall-clock time of the last successful token refresh, used to schedule proactive
    /// refresh-token rotation. `0` means the token was never rotated by underclass.
    #[serde(default)]
    pub token_refreshed_at: i64,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub residency: Option<String>,
    #[serde(default)]
    pub enterprise_url: Option<String>,
    /// Subscription plan reported by the upstream, when the backend exposes one
    /// (`pro`/`max`/`team`/`enterprise` for Claude Code accounts).
    #[serde(default)]
    pub plan: Option<String>,
    pub status: AccountStatus,
    /// Why the account is `Disabled` when underclass disabled it itself, so it can lift the
    /// disable once the cause clears. `None` for an operator disable, which is never auto-reverted.
    #[serde(default)]
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub reset_at: i64,
    /// Deadline per quota `scope` (for example a Cline billing surface). An entry blocks only the
    /// models carrying that scope; `status`/`reset_at` still block the whole account.
    #[serde(default)]
    pub scope_cooling: BTreeMap<String, i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `disabled_reason` recorded when an account is disabled for running on a free plan.
pub const FREE_PLAN_REASON: &str = "free_plan";

/// A status change the account's subscription plan calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanGate {
    /// The account is on a free plan: take it out of the pool.
    Disable,
    /// The account was disabled for a free plan and has since been upgraded: return it.
    Enable,
}

impl Account {
    /// @cc [owner:ghuntley,label:monitor] account-plan-metadata-only
    /// `chatgpt_plan` MUST return only a nonempty string from the stored Codex access
    /// token's `chatgpt_plan_type` claim (namespaced auth claim preferred), or None.
    /// Other backends and malformed tokens MUST return None; no I/O is permitted.
    pub fn chatgpt_plan(&self) -> Option<String> {
        if self.backend != BackendId::Codex {
            return None;
        }
        let claims = crate::jwt::parse_jwt_claims(self.access_token.as_deref()?)?;
        claims
            .get("https://api.openai.com/auth")
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                claims
                    .get("chatgpt_plan_type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
            .map(str::to_owned)
    }

    /// @cc [owner:ghuntley,label:pool] free-plan-gate
    /// `plan_gate` MUST return `Disable` when the account's `plan_label` is `free`
    /// (case-insensitive), including an account already `Disabled` without reason (an operator
    /// disable), so it is adopted and returns automatically after an upgrade; an account already
    /// `Disabled` with reason `FREE_PLAN_REASON` is not re-disabled. It MUST return `Enable`
    /// only when the account is `Disabled` with reason `FREE_PLAN_REASON` and a known, non-free
    /// plan. An unknown plan MUST yield `None`; no I/O is permitted.
    pub fn plan_gate(&self) -> Option<PlanGate> {
        let plan = self.plan_label()?;
        let gated = self.status == AccountStatus::Disabled
            && self.disabled_reason.as_deref() == Some(FREE_PLAN_REASON);
        let free = plan.trim().eq_ignore_ascii_case("free");
        match (free, gated) {
            (true, false) => Some(PlanGate::Disable),
            (false, true) => Some(PlanGate::Enable),
            _ => None,
        }
    }

    pub fn healthy(&self) -> bool {
        self.status == AccountStatus::Healthy
    }

    /// Whether this account may serve `scope` at `now`: the account itself must be healthy, and a
    /// scope with a future deadline must not be entered. An empty/absent scope inherits the
    /// account-wide state only.
    pub fn healthy_for(&self, scope: Option<&str>, now: i64) -> bool {
        self.healthy()
            && self
                .scope_cooling_until(scope)
                .is_none_or(|until| until <= now)
    }

    /// The deadline stored for `scope`, or `None` when the scope is absent or names no surface.
    pub fn scope_cooling_until(&self, scope: Option<&str>) -> Option<i64> {
        let scope = scope.filter(|scope| !scope.is_empty())?;
        self.scope_cooling.get(scope).copied()
    }

    /// The plan this account runs on, for display. Codex derives it from the stored access
    /// token's claims; other backends report it at onboarding and store it on the account.
    pub fn plan_label(&self) -> Option<String> {
        match self.backend {
            BackendId::Codex => self.chatgpt_plan(),
            _ => self.plan.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Context window in tokens. `0` means no source has ever published it.
    #[serde(default)]
    pub context: i64,
    /// Maximum input in tokens. `0` means no source has ever published it; a source that omits
    /// `limit.input` while publishing a context window means "input equals context", so ingest
    /// records `input = context` (see [`ModelInfo::input_from_context`]).
    #[serde(default)]
    pub input: i64,
    /// Maximum output in tokens. `0` means no source has ever published it.
    #[serde(default)]
    pub output: i64,
    /// Quota scope this model bills against. Empty means the whole account shares one quota.
    #[serde(default)]
    pub scope: String,
    /// What the backend published about the model's capabilities. Empty when the source said
    /// nothing: a capability is never synthesized here (see `metadata-never-invented`).
    #[serde(default)]
    pub metadata: ModelMetadata,
}

impl ModelInfo {
    /// @cc [owner:ghuntley,label:pool] input-falls-back-to-context
    /// `input_from_context` MUST set `input` to `context` when a source published a context window
    /// but no input limit, because absence of `limit.input` means "input equals context" on every
    /// backend underclass reads. It MUST leave `input` alone when the source published one, and
    /// MUST leave both at `0` when no source has published either, so "unpublished" stays
    /// distinguishable from a real limit.
    pub fn input_from_context(&mut self) {
        if self.input == 0 && self.context != 0 {
            self.input = self.context;
        }
    }

    /// Whether a source has recorded this model as retired upstream, which keeps it out of the
    /// advertised listing while leaving the stored row in place.
    pub fn is_deprecated(&self) -> bool {
        self.metadata.status == ModelStatus::Deprecated
    }
}

/// The metadata block of a catalog row: what a backend published about a model beyond its limits.
///
/// Every field is optional and defaults to "not published", so a catalog row written by an older
/// build (or by an operator) deserializes unchanged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Modalities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
    /// Whether the source publishes a prompt-cache price for this model. `None` means the source
    /// did not say, not "unsupported".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<WireApi>,
    /// The JSON field the backend streams reasoning in for this model (`reasoning_content`,
    /// `reasoning`). `None` means the source did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<String>,
    #[serde(default)]
    pub status: ModelStatus,
}

impl ModelMetadata {
    /// Record `field` as the model's reasoning field, replacing whatever a previous refresh stored
    /// so a corrected value propagates.
    pub fn set_reasoning_field(&mut self, field: impl Into<String>) {
        self.reasoning_field = Some(field.into());
    }

    /// Take every field `fresh` publishes, keeping this block's value only where `fresh` is silent.
    ///
    /// This is the opposite direction to [`ModelMetadata::fill_from`]: a fresh read from the source
    /// that publishes a field replaces the stored one, while a field the source omitted - because
    /// the read failed, or because the source dropped it - keeps what an earlier read recorded.
    pub fn overlay_from(&mut self, fresh: &ModelMetadata) {
        if fresh.reasoning.is_some() {
            self.reasoning = fresh.reasoning.clone();
        }
        if fresh.tools.is_some() {
            self.tools = fresh.tools;
        }
        if fresh.structured_output.is_some() {
            self.structured_output = fresh.structured_output;
        }
        if fresh.modalities.is_some() {
            self.modalities = fresh.modalities.clone();
        }
        if fresh.cost.is_some() {
            self.cost = fresh.cost.clone();
        }
        if fresh.prompt_cache.is_some() {
            self.prompt_cache = fresh.prompt_cache;
        }
        if fresh.knowledge.is_some() {
            self.knowledge = fresh.knowledge.clone();
        }
        if fresh.wire.is_some() {
            self.wire = fresh.wire;
        }
        if fresh.reasoning_field.is_some() {
            self.reasoning_field = fresh.reasoning_field.clone();
        }
    }

    /// Fill every field this row does not know from `previous`: a refresh that read a source with
    /// fewer fields MUST NOT erase a capability some earlier read already established.
    ///
    /// `status` is deliberately excluded: retirement is decided by the caller that has the
    /// current set in hand, so a stale stored mark cannot keep a model retired after its backend
    /// started serving it again.
    pub fn fill_from(&mut self, previous: &ModelMetadata) {
        if self.reasoning.is_none() {
            self.reasoning = previous.reasoning.clone();
        }
        if self.tools.is_none() {
            self.tools = previous.tools;
        }
        if self.structured_output.is_none() {
            self.structured_output = previous.structured_output;
        }
        if self.modalities.is_none() {
            self.modalities = previous.modalities.clone();
        }
        if self.cost.is_none() {
            self.cost = previous.cost.clone();
        }
        if self.prompt_cache.is_none() {
            self.prompt_cache = previous.prompt_cache;
        }
        if self.knowledge.is_none() {
            self.knowledge = previous.knowledge.clone();
        }
        if self.wire.is_none() {
            self.wire = previous.wire;
        }
        if self.reasoning_field.is_none() {
            self.reasoning_field = previous.reasoning_field.clone();
        }
    }
}

/// Whether a model reasons, and the effort ladder it accepts. An empty `effort` with
/// `supported: true` is a toggle-only model: it can be told to reason, but publishes no levels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningCapability {
    pub supported: bool,
    #[serde(default)]
    pub effort: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Modalities {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

/// Published unit prices in USD per million tokens, exactly as the source reports them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    #[default]
    Available,
    /// Verified retired upstream (a live completion answered `404 model not found`). A deprecated
    /// row stays stored so an operator can see it, and is never advertised.
    Deprecated,
}

/// The upstream API surface a model is served over, as the backend names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireApi {
    ChatCompletions,
    Responses,
    Messages,
    GenerateContent,
}

impl WireApi {
    /// The surface name a backend publishes, or `None` for a name this build does not know - an
    /// unknown name is not guessed into a known one.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "chat-completions" => Some(WireApi::ChatCompletions),
            "responses" => Some(WireApi::Responses),
            "messages" => Some(WireApi::Messages),
            "generate-content" => Some(WireApi::GenerateContent),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub cache_key: String,
    pub account_id: String,
    pub backend: BackendId,
    pub bound_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    QuotaExhausted {
        until_ms: i64,
    },
    /// The upstream refused this model on this account, but the account still serves other
    /// models: the cooldown applies to the model, not to the account.
    ModelQuotaExhausted {
        until_ms: i64,
    },
    AuthFailed,
    Transient,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestLogEntry {
    pub ts: i64,
    pub request_id: String,
    pub model: String,
    pub backend: Option<String>,
    pub account: Option<String>,
    pub label: Option<String>,
    pub sticky: String,
    pub status: u16,
    pub duration_ms: u64,
    /// How the response body ended; filled in when a streamed body finishes.
    pub stream_outcome: Option<String>,
    /// Sanitized upstream error code for a failed attempt or stream.
    pub error_code: Option<String>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod backend_id_tests {
    use super::*;

    #[test]
    fn max_str_width_is_the_widest_ascii_id() {
        for id in BackendId::all() {
            assert!(
                id.as_str().is_ascii(),
                "`{}` must stay ASCII so its byte length is its terminal width",
                id.as_str()
            );
        }
        let widest = BackendId::all()
            .iter()
            .map(|id| id.as_str().len())
            .max()
            .unwrap();
        assert_eq!(BackendId::max_str_width(), widest);
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    fn token(claims: serde_json::Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn plan_is_per_account_and_tracks_current_token() {
        let mut plus = crate::flows::new_account(BackendId::Codex, "plus account".into());
        plus.access_token = Some(token(
            json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "plus"}}),
        ));
        let mut pro = plus.clone();
        pro.access_token = Some(token(json!({"chatgpt_plan_type": "pro"})));
        assert_eq!(plus.chatgpt_plan().as_deref(), Some("plus"));
        assert_eq!(pro.chatgpt_plan().as_deref(), Some("pro"));
        plus.access_token = pro.access_token.clone();
        assert_eq!(plus.chatgpt_plan().as_deref(), Some("pro"));
        pro.backend = BackendId::Copilot;
        assert_eq!(pro.chatgpt_plan(), None);
    }

    #[test]
    fn plan_prefers_namespace_and_handles_missing_or_invalid_metadata() {
        let mut account = crate::flows::new_account(BackendId::Codex, "account".into());
        for claims in [
            json!({}),
            json!({"chatgpt_plan_type": 42}),
            json!({"chatgpt_plan_type": " "}),
        ] {
            account.access_token = Some(token(claims));
            assert_eq!(account.chatgpt_plan(), None);
        }
        account.access_token = Some("invalid".into());
        assert_eq!(account.chatgpt_plan(), None);
        account.access_token = None;
        assert_eq!(account.chatgpt_plan(), None);
        account.access_token = Some(token(
            json!({"chatgpt_plan_type": "free", "https://api.openai.com/auth": {"chatgpt_plan_type": "business"}}),
        ));
        assert_eq!(account.chatgpt_plan().as_deref(), Some("business"));
    }

    #[test]
    fn free_plan_disables_and_upgrade_re_enables() {
        let mut account = crate::flows::new_account(BackendId::Codex, "account".into());
        account.access_token = Some(token(json!({"chatgpt_plan_type": "free"})));
        assert_eq!(account.plan_gate(), Some(PlanGate::Disable));
        account.status = AccountStatus::Disabled;
        assert_eq!(
            account.plan_gate(),
            Some(PlanGate::Disable),
            "adopts an operator disable"
        );
        account.disabled_reason = Some(FREE_PLAN_REASON.into());
        assert_eq!(account.plan_gate(), None);
        account.access_token = Some(token(json!({"chatgpt_plan_type": "plus"})));
        assert_eq!(account.plan_gate(), Some(PlanGate::Enable));
        account.access_token = None;
        assert_eq!(
            account.plan_gate(),
            None,
            "an unknown plan keeps the account disabled"
        );
    }

    #[test]
    fn operator_disable_of_a_paid_account_is_never_reverted() {
        let mut account = crate::flows::new_account(BackendId::Codex, "account".into());
        account.access_token = Some(token(json!({"chatgpt_plan_type": "pro"})));
        assert_eq!(account.plan_gate(), None);
        account.status = AccountStatus::Disabled;
        assert_eq!(account.plan_gate(), None);
    }
}
