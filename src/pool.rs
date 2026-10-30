use crate::models::{Account, AccountStatus, BackendId, Binding, ModelInfo, Outcome, Surface};
use crate::store::Store;
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

pub const BINDING_TTL_MS: i64 = 24 * 60 * 60 * 1000;
pub const DEFAULT_BINDING_CAP: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    StickyHit,
    StickyRebind,
    NewBinding,
    Unsticky,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub account_id: String,
    pub backend: BackendId,
    pub decision: Decision,
    pub concrete_model: String,
}

/// What `PoolCore::report_for` changed, so callers can persist and log it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthTransition {
    /// The account-wide `status`/`reset_at` pair changed.
    Account(AccountStatus),
    /// Only the `scope` named here entered cooling.
    Scope(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SelectError {
    #[error("all eligible accounts are cooling until {until_ms}")]
    Saturated { until_ms: i64 },
    #[error("no accounts configured for this model")]
    NoAccounts,
}

pub struct AccountState {
    pub account: Account,
    pub inflight: u32,
}

pub struct PoolCore {
    pub accounts: HashMap<String, AccountState>,
    bindings: LruCache<String, (String, i64)>,
    /// The models each backend serves, mapped to the quota scope each one bills against. An empty
    /// scope means the model draws on the account's whole quota.
    catalog: HashMap<BackendId, HashMap<String, String>>,
    /// Which request surfaces each backend can serve, as declared by the backend itself.
    surfaces: HashMap<BackendId, Vec<Surface>>,
    /// Per-account, per-model cooldowns: the upstream refused this model on this account while
    /// the account keeps serving its other models.
    model_cooldowns: HashMap<String, HashMap<String, i64>>,
    /// Derived model aliases: alias string -> list of (backend, concrete model id).
    pub aliases: crate::aliases::AliasTable,
    /// Config used to derive aliases.
    pub alias_config: crate::aliases::AliasConfig,
}

impl PoolCore {
    /// @cc [owner:ghuntley,label:pool] pool-surfaces-from-backends
    /// `PoolCore::new` MUST take the surface declarations of the registered backends and MUST
    /// NOT infer them from backend names or model catalogs.
    pub fn new(store: &Store, surfaces: HashMap<BackendId, Vec<Surface>>) -> Self {
        let mut core = Self {
            accounts: HashMap::new(),
            bindings: LruCache::new(NonZeroUsize::new(DEFAULT_BINDING_CAP).unwrap()),
            catalog: HashMap::new(),
            surfaces,
            model_cooldowns: HashMap::new(),
            aliases: crate::aliases::AliasTable::new(),
            alias_config: crate::aliases::AliasConfig::default_config(),
        };
        for a in store.list_accounts() {
            core.accounts.insert(
                a.id.clone(),
                AccountState {
                    account: a,
                    inflight: 0,
                },
            );
        }
        for b in store.list_bindings() {
            core.bindings.put(b.cache_key, (b.account_id, b.bound_at));
        }
        for backend in BackendId::all() {
            core.set_catalog(backend, &store.catalog(backend));
        }
        core
    }

    pub fn set_catalog(&mut self, backend: BackendId, models: &[ModelInfo]) {
        let models = models
            .iter()
            .map(|model| (model.id.clone(), model.scope.clone()))
            .collect();
        self.catalog.insert(backend, models);
    }

    pub fn set_aliases(&mut self, aliases: crate::aliases::AliasTable) {
        self.aliases = aliases;
    }
    pub fn set_alias_config(&mut self, config: crate::aliases::AliasConfig) {
        self.alias_config = config;
    }
    /// Rebuild aliases from current catalogs.
    pub fn rebuild_aliases(&mut self, store: &crate::store::Store) {
        let mut catalogs = std::collections::HashMap::new();
        for b in crate::models::BackendId::all() {
            catalogs.insert(b, store.catalog(b));
        }
        self.aliases = crate::aliases::build_alias_table(&catalogs, &self.alias_config);
    }

    /// Whether `requested` is an alias present in the table. (Concrete-match precedence is
    /// handled per-backend in `serves`/`concrete_for`, not here, so a colliding concrete id in
    /// one backend does not disable the alias for others.)
    pub fn is_alias(&self, requested: &str) -> bool {
        crate::aliases::is_alias(&self.aliases, requested)
    }

    /// Resolve `requested` to the concrete model id for `backend`. Returns `Some(requested)`
    /// when it is not an alias but the backend serves it directly; `None` otherwise.
    pub fn concrete_for(&self, backend: BackendId, requested: &str) -> Option<String> {
        if self.catalog.get(&backend).is_some_and(|set| set.contains_key(requested)) {
            return Some(requested.to_string());
        }
        if crate::aliases::is_alias(&self.aliases, requested) {
            return crate::aliases::concrete_for(&self.aliases, backend, requested);
        }
        None
    }

    pub fn serves(&self, backend: BackendId, model: &str) -> bool {
        if self.catalog.get(&backend).is_some_and(|set| set.contains_key(model)) {
            return true;
        }
        if crate::aliases::is_alias(&self.aliases, model) {
            return crate::aliases::concrete_for(&self.aliases, backend, model).is_some();
        }
        false
    }

    /// The scope `backend`'s catalog stores for `model`: `Some(None)` when the backend lists the
    /// model account-wide, `Some(Some(scope))` when it names a surface, and `None` when the backend
    /// does not list the model at all. The distinction matters: an explicit empty scope is an
    /// account-wide quota, not a missing entry.
    fn declared_scope(&self, backend: BackendId, model: &str) -> Option<Option<String>> {
        let concrete = if self.catalog.get(&backend).is_some_and(|set| set.contains_key(model)) {
            model.to_string()
        } else if crate::aliases::is_alias(&self.aliases, model) {
            match crate::aliases::concrete_for(&self.aliases, backend, model) {
                Some(c) => c,
                None => return None,
            }
        } else {
            return None;
        };
        self.catalog
            .get(&backend)
            .and_then(|models| models.get(&concrete))
            .map(|scope| Some(scope.clone()).filter(|scope| !scope.is_empty()))
    }

    /// The scope a model bills against *for this account*: the reporting backend's declaration is
    /// authoritative, so a backend that lists the model account-wide never inherits another
    /// backend's surface. Only a backend that does not list the model falls back to the routing
    /// view.
    fn scope_for_account(&self, account_id: &str, model: &str) -> Option<String> {
        self.account(account_id)
            .and_then(|account| self.declared_scope(account.backend, model))
            .unwrap_or_else(|| self.model_scope(model))
    }

    /// The quota scope `model` bills against for selection, or `None` for account-wide quota. The
    /// first backend that lists the id decides, so an account-wide declaration wins over a later
    /// backend's surface.
    fn model_scope(&self, model: &str) -> Option<String> {
        BackendId::all()
            .into_iter()
            .find_map(|backend| self.declared_scope(backend, model))
            .flatten()
    }

    /// The billing scope this model draws on for this account. The account's own backend decides;
    /// only an account whose backend does not list the model inherits the routing view.
    fn account_scope(&self, account: &Account, model: &str) -> Option<String> {
        self.declared_scope(account.backend, model)
            .unwrap_or_else(|| self.model_scope(model))
    }

    /// Whether selection may return this account for `model` at `now`, given the backends eligible
    /// for the requested surface and model. `backends` already encodes surface and catalog
    /// membership, including the unknown-model pass-through fallback, so this only adds the health
    /// dimensions: the account must be healthy for the model's billing scope and must not be
    /// cooling for the model itself.
    fn eligible_at(
        &self,
        account: &Account,
        model: &str,
        backends: &HashSet<BackendId>,
        now: i64,
    ) -> bool {
        backends.contains(&account.backend)
            && account.healthy_for(self.account_scope(account, model).as_deref(), now)
            && self
                .model_cooling_until(&account.id, model)
                .is_none_or(|until_ms| until_ms <= now)
    }

    fn eligible_backends(&self, model: &str, surface: Surface) -> Vec<BackendId> {
        let mut out: Vec<BackendId> = BackendId::all()
            .into_iter()
            .filter(|b| self.serves_surface(*b, surface))
            .filter(|b| self.serves(*b, model))
            .collect();
        if out.is_empty()
            && let Some(fallback) = self.fallback_backend(surface)
        {
            out.push(fallback);
        }
        out
    }

    /// Whether a backend can serve the given request surface at all.
    pub fn serves_surface(&self, backend: BackendId, surface: Surface) -> bool {
        self.surfaces
            .get(&backend)
            .is_some_and(|list| list.contains(&surface))
    }

    /// The backend an unknown model falls back to on a surface: the first registered backend
    /// that declares the surface, in a stable order.
    fn fallback_backend(&self, surface: Surface) -> Option<BackendId> {
        BackendId::all()
            .into_iter()
            .find(|b| self.serves_surface(*b, surface))
    }

    /// @cc [owner:ghuntley,label:pool] sweep-readmits-expired-cooling
    /// `sweep(now)` MUST return every account that became selectable again - whose account-wide
    /// `Cooling` deadline, or any of whose scoped deadlines, has passed - and MUST clear exactly the
    /// expired deadlines it readmits. It MUST evict expired per-model cooldowns, and MUST evict
    /// sticky bindings older than `BINDING_TTL_MS`. It MUST NOT clear a deadline that has not
    /// passed.
    pub fn sweep(&mut self, now: i64) -> Vec<String> {
        let mut readmitted = Vec::new();
        for state in self.accounts.values_mut() {
            let mut changed = false;
            if state.account.status == AccountStatus::Cooling && state.account.reset_at <= now {
                state.account.status = AccountStatus::Healthy;
                state.account.reset_at = 0;
                changed = true;
            }
            let expired: Vec<String> = state
                .account
                .scope_cooling
                .iter()
                .filter(|(_, until)| **until <= now)
                .map(|(scope, _)| scope.clone())
                .collect();
            for scope in expired {
                state.account.scope_cooling.remove(&scope);
                changed = true;
            }
            if changed {
                readmitted.push(state.account.id.clone());
            }
        }
        let expired: Vec<String> = self
            .model_cooldowns
            .iter_mut()
            .filter_map(|(account_id, models)| {
                models.retain(|_, until_ms| *until_ms > now);
                if models.is_empty() {
                    Some(account_id.clone())
                } else {
                    None
                }
            })
            .collect();
        for account_id in expired {
            self.model_cooldowns.remove(&account_id);
        }
        loop {
            let expired = match self.bindings.peek_lru() {
                Some((key, (_, bound_at))) if now - *bound_at > BINDING_TTL_MS => Some(key.clone()),
                _ => None,
            };
            match expired {
                Some(key) => {
                    self.bindings.pop(&key);
                }
                None => break,
            }
        }
        readmitted
    }

    /// @cc [owner:ghuntley,label:pool] model-cooldown-scopes-one-model
    /// `report_model_quota` MUST record a cooldown for one model on one account until
    /// `until_ms`, MUST NOT change the account's status, and MUST never shorten an existing
    /// cooldown for the same account and model.
    pub fn report_model_quota(&mut self, account_id: &str, model: &str, until_ms: i64) {
        let models = self
            .model_cooldowns
            .entry(account_id.to_string())
            .or_default();
        let entry = models.entry(model.to_string()).or_insert(until_ms);
        if until_ms > *entry {
            *entry = until_ms;
        }
    }

    /// @cc [owner:ghuntley,label:pool] reset-clears-model-cooldowns
    /// `clear_model_cooldowns` MUST drop every per-model cooldown recorded for the account, so a
    /// redeemed reset credit restores the models it paid for. It MUST NOT touch the account-wide
    /// status or any billing-scope deadline, which describe different exhaustion.
    pub fn clear_model_cooldowns(&mut self, account_id: &str) {
        self.model_cooldowns.remove(account_id);
    }

    /// Whether the account is cooling for the given model.
    pub fn model_cooling_until(&self, account_id: &str, model: &str) -> Option<i64> {
        self.model_cooldowns
            .get(account_id)
            .and_then(|models| models.get(model))
            .copied()
    }

    /// @cc [owner:ghuntley,label:pool] select-sticky-healthy-never-unhealthy
    /// Selection MUST return, in priority order: (1) the account bound to the sticky key when it
    /// is Healthy, not cooling for the model's quota scope or for the model itself, serves the
    /// model, and declares the requested `surface` (decision `StickyHit`); (2) on rebinding, an
    /// equally eligible account of the bound account's backend when one exists, else any equally
    /// eligible account; (3) for sticky misses, the least-in-flight eligible account. It MUST NOT
    /// ever return an account that is Cooling, AuthError or Disabled, cooling for the requested
    /// model's scope, or cooling for the model itself. When no eligible account exists it MUST fail
    /// with `Saturated { until_ms = the earliest moment an eligible account can serve again }` or,
    /// if none is cooling, `NoAccounts`. Requests without a sticky key MUST NOT create bindings.
    pub fn select(
        &mut self,
        now: i64,
        sticky: Option<&str>,
        model: &str,
        surface: Surface,
    ) -> Result<Selection, SelectError> {
        self.select_excluding(now, sticky, model, surface, &HashSet::new())
    }

    /// @cc [owner:ghuntley,label:pool] select-excluding-skips-excluded
    /// `select_excluding` MUST behave as `select` with every account in `exclude` treated as
    /// ineligible: it MUST NOT return an excluded account, a sticky key bound to an excluded
    /// account MUST rebind as if that account were unhealthy, and with an empty `exclude` it MUST
    /// be identical to `select`.
    pub fn select_excluding(
        &mut self,
        now: i64,
        sticky: Option<&str>,
        model: &str,
        surface: Surface,
        exclude: &HashSet<String>,
    ) -> Result<Selection, SelectError> {
        self.sweep(now);
        let backends: HashSet<BackendId> =
            self.eligible_backends(model, surface).into_iter().collect();

        let find_healthy = |core: &Self, backend_filter: Option<BackendId>| -> Option<String> {
            let mut candidates: Vec<(u32, &String)> = core
                .accounts
                .iter()
                .filter(|(id, s)| {
                    !exclude.contains(*id)
                        && core.eligible_at(&s.account, model, &backends, now)
                        && backend_filter.is_none_or(|b| s.account.backend == b)
                })
                .map(|(id, s)| (s.inflight, id))
                .collect();
            candidates.sort();
            candidates.into_iter().next().map(|(_, id)| id.clone())
        };

        if let Some(key) = sticky {
            let bound = self.bindings.get(key).map(|(id, at)| (id.clone(), *at));
            if let Some((bound_id, bound_at)) = bound {
                if now - bound_at <= BINDING_TTL_MS {
                    if let Some(state) = self.accounts.get(&bound_id)
                        && !exclude.contains(&bound_id)
                        && self.eligible_at(&state.account, model, &backends, now)
                    {
                        let concrete = match self.concrete_for(state.account.backend, model) {
                            Some(c) => c,
                            // Unreachable when selection is consistent with `serves`; fall back to
                            // the requested id rather than panicking. The upstream will reject an
                            // unresolved alias, surfacing the bug instead of silently misrouting.
                            None => model.to_string(),
                        };
                        return Ok(Selection {
                            account_id: bound_id.clone(),
                            backend: state.account.backend,
                            decision: Decision::StickyHit,
                            concrete_model: concrete,
                        });
                    }
                    let preferred = self.accounts.get(&bound_id).map(|s| s.account.backend);
                    if let Some(next) = find_healthy(self, preferred) {
                        let backend = self.accounts[&next].account.backend;
                        self.bind(key, &next, now);
                        let concrete = self.concrete_for(backend, model).unwrap_or_else(|| model.to_string());
                        return Ok(Selection {
                            account_id: next,
                            backend,
                            decision: Decision::StickyRebind,
                            concrete_model: concrete,
                        });
                    }
                } else {
                    self.bindings.pop(key);
                }
            }
        }

        if let Some(next) = find_healthy(self, None) {
            let backend = self.accounts[&next].account.backend;
            let decision = if sticky.is_some() {
                Decision::NewBinding
            } else {
                Decision::Unsticky
            };
            if let Some(key) = sticky {
                self.bind(key, &next, now);
            }
            let concrete = self.concrete_for(backend, model).unwrap_or_else(|| model.to_string());
            return Ok(Selection {
                account_id: next,
                backend,
                decision,
                concrete_model: concrete,
            });
        }

        match self.min_reset(
            |a| backends.contains(&a.backend) && !exclude.contains(&a.id),
            model,
            now,
        ) {
            Some(until_ms) => Err(SelectError::Saturated { until_ms }),
            None => Err(SelectError::NoAccounts),
        }
    }

    /// @cc [owner:ghuntley,label:pool] saturation-deadline-is-reachable
    /// `min_reset` MUST only report deadlines from accounts selection could actually return once the
    /// deadline passes: `Healthy` accounts with a scoped or per-model deadline, and `Cooling`
    /// accounts (account wide or scoped). A `Disabled` or `AuthError` account MUST NOT contribute a
    /// deadline, because the proxy advertises that value as `Retry-After` and the account would not
    /// serve at that time. For one account, it MUST contribute the later of its account-wide
    /// deadline and its latest scoped or per-model deadline, since the account serves only after
    /// every applicable deadline has passed.
    fn min_reset(&self, eligible: impl Fn(&Account) -> bool, model: &str, now: i64) -> Option<i64> {
        self.accounts
            .values()
            .filter(|s| eligible(&s.account))
            .filter(|s| {
                matches!(
                    s.account.status,
                    AccountStatus::Healthy | AccountStatus::Cooling
                )
            })
            .filter_map(|s| {
                let account_wide =
                    (s.account.status == AccountStatus::Cooling).then_some(s.account.reset_at);
                let scope = self.account_scope(&s.account, model);
                let scoped = s
                    .account
                    .scope_cooling_until(scope.as_deref())
                    .filter(|until| *until > now);
                let per_model = self
                    .model_cooling_until(&s.account.id, model)
                    .filter(|until| *until > now);
                // One account serves only once *every* applicable deadline has passed, so the
                // advertised retry time is the latest of them.
                let latest = [scoped, per_model].into_iter().flatten().max();
                match (account_wide, latest) {
                    (Some(wide), Some(latest)) => Some(wide.max(latest)),
                    (wide, latest) => wide.or(latest),
                }
            })
            .min()
    }

    /// @cc [owner:ghuntley,label:pool] rebind-reverted-for-transient-exclusion
    /// `restore_binding` MUST put `key` back on `account_id` at `bound_at`. Selection rebinds when
    /// it skips an account, and a skip caused by a merely deferred refresh must not displace the
    /// session's prompt-cache affinity; the caller reverts it once the request finishes.
    pub fn restore_binding(&mut self, key: &str, account_id: &str, bound_at: i64) {
        self.bindings
            .put(key.to_string(), (account_id.to_string(), bound_at));
    }

    /// The account a sticky key is bound to, with its bind time.
    pub fn binding(&self, key: &str) -> Option<(String, i64)> {
        self.bindings.peek(key).map(|(id, at)| (id.clone(), *at))
    }

    fn bind(&mut self, key: &str, account_id: &str, now: i64) {
        self.bindings
            .put(key.to_string(), (account_id.to_string(), now));
    }

    pub fn bindings(&self) -> Vec<Binding> {
        self.bindings
            .iter()
            .filter_map(|(key, (account_id, bound_at))| {
                let backend = self.accounts.get(account_id)?.account.backend;
                Some(Binding {
                    cache_key: key.clone(),
                    account_id: account_id.clone(),
                    backend,
                    bound_at: *bound_at,
                })
            })
            .collect()
    }

    /// @cc [owner:ghuntley,label:pool] inflight-saturating
    /// `acquire` MUST increment and `release` decrement the account's in-flight counter with
    /// saturating semantics: `release` MUST NOT move the counter below zero.
    pub fn acquire(&mut self, account_id: &str) {
        if let Some(s) = self.accounts.get_mut(account_id) {
            s.inflight = s.inflight.saturating_add(1);
        }
    }

    pub fn release(&mut self, account_id: &str) {
        if let Some(s) = self.accounts.get_mut(account_id) {
            s.inflight = s.inflight.saturating_sub(1);
        }
    }

    /// @cc [owner:ghuntley,label:pool] report-quota-transitions
    /// `report` MUST transition the account to `Cooling` on `Outcome::QuotaExhausted`, keeping the
    /// later of `until_ms` and any deadline already recorded (a later signal MUST NOT shorten an
    /// existing cooldown), and to `AuthError` on `Outcome::AuthFailed`, and MUST NOT change
    /// the account's status on `Outcome::Ok`, `Outcome::Transient`, or a model-scoped quota
    /// rejection. It is the account-wide entry point: it MUST NOT create a scoped or per-model
    /// deadline.
    pub fn report(&mut self, account_id: &str, outcome: Outcome) -> Option<AccountStatus> {
        let state = self.accounts.get_mut(account_id)?;
        let new_status = match outcome {
            // Never shorten an existing account-wide cooldown: a second signal may name an earlier
            // window than the one already recorded (the 5h reset before the 7d one), and the
            // account serves only once every spent window has reset. This mirrors the scoped and
            // per-model paths, which already keep the later deadline.
            Outcome::QuotaExhausted { until_ms } => Some((
                AccountStatus::Cooling,
                if state.account.status == AccountStatus::Cooling {
                    until_ms.max(state.account.reset_at)
                } else {
                    until_ms
                },
            )),
            Outcome::AuthFailed => Some((AccountStatus::AuthError, 0)),
            Outcome::Transient | Outcome::Ok | Outcome::ModelQuotaExhausted { .. } => None,
        };
        if let Some((status, reset_at)) = new_status {
            state.account.status = status;
            state.account.reset_at = reset_at;
            Some(status)
        } else {
            None
        }
    }

    /// @cc [owner:ghuntley,label:pool] report-for-model-scopes-quota
    /// `report_for` MUST cool only the requested model's quota scope - recording
    /// `scope_cooling[scope] = until_ms` - when that model has a non-empty scope for the reporting
    /// account's backend and the outcome is `QuotaExhausted`, and MUST leave the account-wide
    /// `status`/`reset_at` untouched so another scope of the same account stays selectable. For a
    /// model without a scope, and for every non-quota outcome, it MUST delegate to the account-wide
    /// rules of `report`. When a scoped deadline is already later than `until_ms`, the later
    /// deadline MUST be kept.
    pub fn report_for(
        &mut self,
        account_id: &str,
        model: &str,
        outcome: Outcome,
    ) -> Option<HealthTransition> {
        let scope = match &outcome {
            Outcome::QuotaExhausted { .. } => self.scope_for_account(account_id, model),
            _ => None,
        };
        if let Some(scope) = scope {
            let Outcome::QuotaExhausted { until_ms } = outcome else {
                unreachable!("scope is only resolved for quota outcomes");
            };
            let state = self.accounts.get_mut(account_id)?;
            let deadline = state
                .account
                .scope_cooling
                .get(&scope)
                .copied()
                .map_or(until_ms, |current| current.max(until_ms));
            state.account.scope_cooling.insert(scope.clone(), deadline);
            return Some(HealthTransition::Scope(scope));
        }
        self.report(account_id, outcome)
            .map(HealthTransition::Account)
    }

    /// The scope deadline currently stored for `account_id`, if any.
    pub fn scope_reset(&self, account_id: &str, scope: &str) -> Option<i64> {
        self.accounts
            .get(account_id)
            .and_then(|s| s.account.scope_cooling.get(scope).copied())
    }

    pub fn set_status(&mut self, account_id: &str, status: AccountStatus, reset_at: i64) {
        if let Some(s) = self.accounts.get_mut(account_id) {
            s.account.status = status;
            s.account.reset_at = reset_at;
        }
    }

    pub fn insert_account(&mut self, account: Account) {
        self.accounts.insert(
            account.id.clone(),
            AccountState {
                account,
                inflight: 0,
            },
        );
    }

    pub fn sync_account(&mut self, account: Account) {
        match self.accounts.get_mut(&account.id) {
            Some(state) => {
                let inflight = state.inflight;
                state.account = account;
                state.inflight = inflight;
            }
            None => self.insert_account(account),
        }
    }

    pub fn sync_from_store(&mut self, store: &Store) {
        let now = crate::models::now_ms();
        for account in store.list_accounts() {
            let mut account = account;
            let was_cooling = account.status == AccountStatus::Cooling && account.reset_at <= now;
            let scopes_before = account.scope_cooling.len();
            if was_cooling {
                account.status = AccountStatus::Healthy;
                account.reset_at = 0;
            }
            account.scope_cooling.retain(|_, until| *until > now);
            if was_cooling || account.scope_cooling.len() != scopes_before {
                store.update_account_health(&account);
            }
            self.sync_account(account);
        }
        let live: std::collections::HashSet<String> = self.accounts.keys().cloned().collect();
        let stale: Vec<String> = store
            .list_accounts()
            .into_iter()
            .filter(|a| !live.contains(&a.id))
            .map(|a| a.id)
            .collect();
        for id in stale {
            self.remove_account(&id);
        }
    }

    pub fn remove_account(&mut self, account_id: &str) {
        self.accounts.remove(account_id);
        let keys: Vec<String> = self
            .bindings
            .iter()
            .filter(|(_, (id, _))| id == account_id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.bindings.pop(&k);
        }
    }

    pub fn account(&self, account_id: &str) -> Option<&Account> {
        self.accounts.get(account_id).map(|s| &s.account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, backend: BackendId) -> Account {
        Account {
            id: id.into(),
            backend,
            label: id.into(),
            refresh_token: None,
            access_token: None,
            expires_at: 0,
            token_refreshed_at: 0,
            account_id: None,
            residency: None,
            enterprise_url: None,
            plan: None,
            disabled_reason: None,
            status: AccountStatus::Healthy,
            reset_at: 0,
            scope_cooling: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn core_with(backends: &[(BackendId, &[&str], &[&str])]) -> PoolCore {
        let store = Store::in_memory().unwrap();
        let surfaces = backends
            .iter()
            .map(|(backend, _, _)| {
                let surface = match backend {
                    BackendId::Claude => Surface::Anthropic,
                    _ => Surface::OpenAI,
                };
                (*backend, vec![surface])
            })
            .collect();
        let mut core = PoolCore::new(&store, surfaces);
        for (backend, models, ids) in backends {
            core.set_catalog(*backend, &catalog(models));
            for id in *ids {
                core.insert_account(account(id, *backend));
            }
        }
        core
    }

    fn catalog(models: &[&str]) -> Vec<ModelInfo> {
        models
            .iter()
            .map(|id| ModelInfo {
                id: (*id).to_string(),
                name: (*id).to_string(),
                context: 0,
                input: 0,
                output: 0,
                scope: String::new(),
            })
            .collect()
    }

    #[test]
    fn sticky_key_pins_to_one_account() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a", "b"])]);
        let s1 = core
            .select(0, Some("k1"), "gpt-5.5", Surface::OpenAI)
            .unwrap();
        let s2 = core
            .select(1, Some("k1"), "gpt-5.5", Surface::OpenAI)
            .unwrap();
        assert_eq!(s1.account_id, s2.account_id);
        assert_eq!(s2.decision, Decision::StickyHit);
    }

    #[test]
    fn sticky_rebinds_within_same_backend_when_cooling() {
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["a", "b"]),
            (BackendId::Copilot, &["gpt-5.5"], &["c"]),
        ]);
        let first = core
            .select(0, Some("k"), "gpt-5.5", Surface::OpenAI)
            .unwrap();
        core.report(
            &first.account_id,
            Outcome::QuotaExhausted { until_ms: 10_000 },
        );
        let second = core
            .select(1, Some("k"), "gpt-5.5", Surface::OpenAI)
            .unwrap();
        assert_ne!(second.account_id, first.account_id);
        assert_eq!(second.backend, BackendId::Codex);
        assert_eq!(second.decision, Decision::StickyRebind);
    }

    #[test]
    fn selection_never_returns_unhealthy_accounts() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a"])]);
        core.report("a", Outcome::QuotaExhausted { until_ms: 100 });
        assert!(matches!(
            core.select(50, Some("k"), "gpt-5.5", Surface::OpenAI),
            Err(SelectError::Saturated { until_ms: 100 })
        ));
        assert!(matches!(
            core.select(150, Some("k"), "gpt-5.5", Surface::OpenAI),
            Ok(Selection { .. })
        ));
    }

    #[test]
    fn unknown_model_routes_to_codex_pass_through() {
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["a"]),
            (BackendId::Copilot, &["gpt-4.1"], &["c"]),
        ]);
        let s = core
            .select(0, Some("k"), "totally-unknown-model", Surface::OpenAI)
            .unwrap();
        assert_eq!(s.backend, BackendId::Codex);
        assert_eq!(s.account_id, "a");
    }

    #[test]
    fn model_eligibility_respects_catalog() {
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["a"]),
            (BackendId::Copilot, &["gpt-4.1"], &["c"]),
        ]);
        let s = core.select(0, None, "gpt-4.1", Surface::OpenAI).unwrap();
        assert_eq!(s.backend, BackendId::Copilot);
    }

    #[test]
    fn least_in_flight_is_preferred() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a", "b"])]);
        core.acquire("a");
        core.acquire("a");
        let s = core.select(0, None, "gpt-5.5", Surface::OpenAI).unwrap();
        assert_eq!(s.account_id, "b");
        core.release("a");
        core.release("a");
    }

    #[test]
    fn in_flight_never_goes_negative() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a"])]);
        core.release("a");
        core.release("a");
        assert_eq!(core.accounts["a"].inflight, 0);
    }

    #[test]
    fn saturation_reports_earliest_reset() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a", "b"])]);
        core.report("a", Outcome::QuotaExhausted { until_ms: 500 });
        core.report("b", Outcome::QuotaExhausted { until_ms: 300 });
        match core.select(100, Some("k"), "gpt-5.5", Surface::OpenAI) {
            Err(SelectError::Saturated { until_ms }) => assert_eq!(until_ms, 300),
            other => panic!("expected saturated, got {other:?}"),
        }
    }

    #[test]
    fn no_accounts_error_when_nothing_configured() {
        let mut core = core_with(&[]);
        assert!(matches!(
            core.select(0, Some("k"), "gpt-5.5", Surface::OpenAI),
            Err(SelectError::NoAccounts)
        ));
    }

    #[test]
    fn unsticky_requests_do_not_bind() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a"])]);
        let s = core.select(0, None, "gpt-5.5", Surface::OpenAI).unwrap();
        assert_eq!(s.decision, Decision::Unsticky);
        assert!(core.bindings().is_empty());
    }

    #[test]
    fn new_binding_created_for_sticky_request_on_miss() {
        let mut core = core_with(&[(BackendId::Codex, &["gpt-5.5"], &["a"])]);
        let s = core
            .select(0, Some("k"), "gpt-5.5", Surface::OpenAI)
            .unwrap();
        assert_eq!(s.decision, Decision::NewBinding);
        let bindings = core.bindings();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].account_id, "a");
        assert_eq!(bindings[0].cache_key, "k");
    }

    fn cline_core() -> PoolCore {
        let mut core = core_with(&[(
            BackendId::Cline,
            &[
                "cline/anthropic/claude-sonnet-4-6",
                "cline/cline-pass/kimi-k3",
            ],
            &["a"],
        )]);
        core.set_catalog(
            BackendId::Cline,
            &[
                ModelInfo {
                    scope: "credits".into(),
                    ..catalog(&["cline/anthropic/claude-sonnet-4-6"])[0].clone()
                },
                ModelInfo {
                    scope: "cline-pass".into(),
                    ..catalog(&["cline/cline-pass/kimi-k3"])[0].clone()
                },
            ],
        );
        core
    }

    /// The two quota dimensions the fork and upstream each added must compose: a billing scope and
    /// a per-model cooldown block their own slice, and the advertised deadline is the moment the
    /// account serves again on *both*.
    /// A model cooldown on an account that is no longer selectable must not advertise a
    /// `Retry-After`: the account would not serve at that time, so the pool has no reachable
    /// deadline and must report `NoAccounts` instead of a saturated 429.
    #[test]
    fn model_cooldown_on_an_unusable_account_is_not_a_retry_after() {
        let mut core = core_with(&[(BackendId::Cline, &["cline/cline-pass/kimi-k3"], &["a"])]);
        core.report_model_quota("a", "cline/cline-pass/kimi-k3", 10_000);
        assert!(matches!(
            core.select(1, None, "cline/cline-pass/kimi-k3", Surface::OpenAI),
            Err(SelectError::Saturated { until_ms: 10_000 })
        ));
        for status in [AccountStatus::AuthError, AccountStatus::Disabled] {
            core.set_status("a", status, 0);
            assert!(
                matches!(
                    core.select(1, None, "cline/cline-pass/kimi-k3", Surface::OpenAI),
                    Err(SelectError::NoAccounts)
                ),
                "{status:?} must not contribute a model deadline"
            );
        }
    }

    #[test]
    fn billing_scope_and_model_cooldown_compose() {
        let mut core = core_with(&[
            (BackendId::Cline, &["cline/cline-pass/kimi-k3"], &["a"]),
            (BackendId::Cline, &["cline/cline-pass/kimi-k3"], &["b"]),
        ]);
        let pass = ModelInfo {
            scope: "cline-pass".into(),
            ..catalog(&["cline/cline-pass/kimi-k3"])[0].clone()
        };
        core.set_catalog(BackendId::Cline, &[pass]);

        // A scope deadline and a model deadline on the same account: it serves only after both.
        core.report_for(
            "a",
            "cline/cline-pass/kimi-k3",
            Outcome::QuotaExhausted { until_ms: 1_000 },
        );
        core.report_model_quota("a", "cline/cline-pass/kimi-k3", 2_000);
        assert!(matches!(
            core.select(500, None, "cline/cline-pass/kimi-k3", Surface::OpenAI),
            Ok(_)
        ));
        // Account `b` is untouched and absorbs selection at `now`; the deadline must still name
        // the later of `a`'s two deadlines rather than the earlier one.
        core.report("b", Outcome::QuotaExhausted { until_ms: 5_000 });
        // The model cooldown outlasts the scope deadline.
        assert!(matches!(
            core.select(500, None, "cline/cline-pass/kimi-k3", Surface::OpenAI),
            Err(SelectError::Saturated { until_ms: 2_000 })
        ));
        // Past both of `a`'s deadlines it serves again, even though `b` is still cooling: the
        // dimensions scope one account, they do not pin the request to it.
        let resumed = core
            .select(2_500, None, "cline/cline-pass/kimi-k3", Surface::OpenAI)
            .unwrap();
        assert_eq!(resumed.account_id, "a");
    }

    #[test]
    fn credits_cooling_leaves_the_other_surface_selectable() {
        let mut core = cline_core();
        let transition = core
            .report_for(
                "a",
                "cline/anthropic/claude-sonnet-4-6",
                Outcome::QuotaExhausted { until_ms: 10_000 },
            )
            .expect("reported");
        assert_eq!(transition, HealthTransition::Scope("credits".into()));
        assert!(
            core.accounts["a"].account.healthy(),
            "a scoped quota must not cool the whole account"
        );

        assert!(matches!(
            core.select(
                1,
                Some("k"),
                "cline/anthropic/claude-sonnet-4-6",
                Surface::OpenAI
            ),
            Err(SelectError::Saturated { until_ms: 10_000 })
        ));
        let pass = core
            .select(1, Some("k"), "cline/cline-pass/kimi-k3", Surface::OpenAI)
            .unwrap();
        assert_eq!(pass.account_id, "a", "the pass surface must still be tried");
    }

    #[test]
    fn account_wide_cooling_blocks_every_surface() {
        let mut core = cline_core();
        core.report("a", Outcome::QuotaExhausted { until_ms: 10_000 });
        for model in [
            "cline/anthropic/claude-sonnet-4-6",
            "cline/cline-pass/kimi-k3",
        ] {
            assert!(matches!(
                core.select(1, None, model, Surface::OpenAI),
                Err(SelectError::Saturated { until_ms: 10_000 })
            ));
        }
    }

    #[test]
    fn auth_failure_is_never_scoped() {
        let mut core = cline_core();
        let transition = core
            .report_for("a", "cline/cline-pass/kimi-k3", Outcome::AuthFailed)
            .expect("reported");
        assert_eq!(
            transition,
            HealthTransition::Account(AccountStatus::AuthError)
        );
        assert!(core.accounts["a"].account.scope_cooling.is_empty());
        assert!(
            core.select(
                1,
                None,
                "cline/anthropic/claude-sonnet-4-6",
                Surface::OpenAI
            )
            .is_err()
        );
    }

    #[test]
    fn scoped_deadlines_are_pruned_when_they_pass() {
        let mut core = cline_core();
        core.report_for(
            "a",
            "cline/cline-pass/kimi-k3",
            Outcome::QuotaExhausted { until_ms: 500 },
        );
        assert_eq!(core.scope_reset("a", "cline-pass"), Some(500));
        assert!(core.sweep(500).contains(&"a".to_string()));
        assert_eq!(core.scope_reset("a", "cline-pass"), None);
        assert!(
            core.select(500, None, "cline/cline-pass/kimi-k3", Surface::OpenAI)
                .is_ok()
        );
    }

    #[test]
    fn an_account_wide_declaration_never_inherits_another_backends_scope() {
        // A cross-listed id: Codex lists it account-wide, Cline names a surface for it. A Codex 429
        // must cool the whole Codex account, not a foreign `credits` surface.
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["a"]),
            (BackendId::Cline, &["gpt-5.5"], &["c"]),
        ]);
        core.set_catalog(
            BackendId::Cline,
            &[ModelInfo {
                scope: "credits".into(),
                ..catalog(&["gpt-5.5"])[0].clone()
            }],
        );

        let transition = core
            .report_for("a", "gpt-5.5", Outcome::QuotaExhausted { until_ms: 5_000 })
            .expect("reported");
        assert_eq!(
            transition,
            HealthTransition::Account(AccountStatus::Cooling),
            "an account whose backend declares the model account-wide must cool account-wide"
        );
        assert!(
            core.accounts["a"].account.scope_cooling.is_empty(),
            "the foreign surface must not be recorded: {:?}",
            core.accounts["a"].account.scope_cooling
        );

        // The Cline account's own declaration is still authoritative for its own 429.
        let transition = core
            .report_for("c", "gpt-5.5", Outcome::QuotaExhausted { until_ms: 5_000 })
            .expect("reported");
        assert_eq!(transition, HealthTransition::Scope("credits".into()));
    }

    #[test]
    fn disabled_accounts_do_not_advertise_a_saturation_deadline() {
        // The advertised `Retry-After` must be a time at which some account can actually serve.
        let mut core = cline_core();
        core.report_for(
            "a",
            "cline/cline-pass/kimi-k3",
            Outcome::QuotaExhausted { until_ms: 9_000 },
        );
        core.set_status("a", AccountStatus::Disabled, 0);
        assert!(matches!(
            core.select(1, None, "cline/cline-pass/kimi-k3", Surface::OpenAI),
            Err(SelectError::NoAccounts)
        ));

        // The same account cooling (not disabled) does advertise the deadline.
        core.set_status("a", AccountStatus::Healthy, 0);
        assert!(matches!(
            core.select(1, None, "cline/cline-pass/kimi-k3", Surface::OpenAI),
            Err(SelectError::Saturated { until_ms: 9_000 })
        ));
    }

    #[test]
    fn saturation_waits_for_both_the_account_and_scope_deadlines() {
        // The advertised retry time must be a time the account can actually serve.
        let mut core = cline_core();
        core.report_for(
            "a",
            "cline/anthropic/claude-sonnet-4-6",
            Outcome::QuotaExhausted { until_ms: 7_000 },
        );
        core.report("a", Outcome::QuotaExhausted { until_ms: 5_000 });
        match core.select(
            1,
            None,
            "cline/anthropic/claude-sonnet-4-6",
            Surface::OpenAI,
        ) {
            Err(SelectError::Saturated { until_ms }) => assert_eq!(
                until_ms, 7_000,
                "the account is still scope-cooled after its account-wide deadline"
            ),
            other => panic!("expected saturated, got {other:?}"),
        }
    }

    #[test]
    fn later_scoped_deadline_wins() {
        let mut core = cline_core();
        for until in [9_000, 5_000] {
            core.report_for(
                "a",
                "cline/anthropic/claude-sonnet-4-6",
                Outcome::QuotaExhausted { until_ms: until },
            );
        }
        assert_eq!(core.scope_reset("a", "credits"), Some(9_000));
    }

    #[test]
    fn scoped_cooling_rebinds_sticky_sessions_to_another_surface() {
        let mut core = core_with(&[(
            BackendId::Cline,
            &[
                "cline/anthropic/claude-sonnet-4-6",
                "cline/cline-pass/kimi-k3",
            ],
            &["a"],
        )]);
        core.set_catalog(
            BackendId::Cline,
            &[
                ModelInfo {
                    scope: "credits".into(),
                    ..catalog(&["cline/anthropic/claude-sonnet-4-6"])[0].clone()
                },
                ModelInfo {
                    scope: "cline-pass".into(),
                    ..catalog(&["cline/cline-pass/kimi-k3"])[0].clone()
                },
            ],
        );
        let first = core
            .select(
                0,
                Some("k"),
                "cline/anthropic/claude-sonnet-4-6",
                Surface::OpenAI,
            )
            .unwrap();
        core.report_for(
            &first.account_id,
            "cline/anthropic/claude-sonnet-4-6",
            Outcome::QuotaExhausted { until_ms: 1_000 },
        );
        // The same sticky key must not keep the credits-only session pinned to a cooling surface;
        // the pass surface is a different model, so a fresh selection hits the account again.
        let second = core
            .select(1, Some("k"), "cline/cline-pass/kimi-k3", Surface::OpenAI)
            .unwrap();
        assert_eq!(second.account_id, "a");
        assert_eq!(second.decision, Decision::StickyHit);
    }

    /// @cc [owner:ghuntley,label:pool] surfaces-partition-selection
    /// Selection MUST only return accounts whose backend declares the requested surface, and an
    /// unknown model MUST fall back to a backend that declares that surface.
    #[test]
    fn surfaces_partition_selection() {
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["codex-a"]),
            (BackendId::Copilot, &["gpt-5.5"], &["copilot-a"]),
            (BackendId::Claude, &["claude-opus-5-5"], &["claude-a"]),
        ]);
        let openai = core
            .select(0, Some("k"), "gpt-5.5", Surface::OpenAI)
            .unwrap();
        assert!(openai.backend != BackendId::Claude);
        let anthropic = core
            .select(0, Some("k2"), "claude-opus-5-5", Surface::Anthropic)
            .unwrap();
        assert_eq!(anthropic.backend, BackendId::Claude);
        assert_eq!(anthropic.account_id, "claude-a");
        // An unknown model falls back to a backend that serves the requested surface.
        let unknown = core
            .select(0, Some("k3"), "claude-unknown-model", Surface::Anthropic)
            .unwrap();
        assert_eq!(unknown.backend, BackendId::Claude);
    }

    #[test]
    fn anthropic_surface_never_selects_an_openai_account() {
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["codex-a"]),
            (BackendId::Claude, &["claude-opus-5-5"], &["claude-a"]),
        ]);
        for _ in 0..5 {
            let selection = core
                .select(0, None, "claude-opus-5-5", Surface::Anthropic)
                .unwrap();
            assert_eq!(selection.backend, BackendId::Claude);
        }
    }

    #[test]
    fn saturation_is_measured_per_surface() {
        let mut core = core_with(&[
            (BackendId::Codex, &["gpt-5.5"], &["codex-a"]),
            (BackendId::Claude, &["claude-opus-5-5"], &["claude-a"]),
        ]);
        core.report("codex-a", Outcome::QuotaExhausted { until_ms: 10_000 });
        // The Anthropic surface has a healthy account and must not report saturation.
        assert!(
            core.select(0, None, "claude-opus-5-5", Surface::Anthropic)
                .is_ok()
        );
        // The OpenAI surface is saturated until its account's reset.
        assert!(matches!(
            core.select(0, None, "gpt-5.5", Surface::OpenAI),
            Err(SelectError::Saturated { until_ms: 10_000 })
        ));
    }
}
