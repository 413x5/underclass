use hegel::TestCase;
use hegel::generators as gs;
use std::collections::HashMap;
use underclass::models::{Account, AccountStatus, BackendId, Outcome, Surface};
use underclass::pool::{PoolCore, SelectError};
use underclass::resets::{
    Candidate, RateLimit, Window, blocked_cooling_deadline, choose_candidate, natural_recovery_ms,
};
use underclass::stats::{self, Dim};
use underclass::store::UpstreamEvent;
use underclass::store::{Store, UsageQuery, UsageRecord};
use underclass::tokens::{ROTATION_INTERVAL_MS, rotation_due};
use underclass::usage::{StreamEnd, TokenCounts, UsageDialect, UsageTap};

/// The surfaces the property tests route over: the OpenAI-shaped ones.
fn openai_surfaces() -> HashMap<BackendId, Vec<Surface>> {
    HashMap::from([
        (BackendId::Codex, vec![Surface::OpenAI]),
        (BackendId::Copilot, vec![Surface::OpenAI]),
        (BackendId::Claude, vec![Surface::Anthropic]),
        (BackendId::Cline, vec![Surface::OpenAI]),
        (BackendId::OpencodeGo, vec![Surface::OpenAI]),
    ])
}

/// Catalog entries for `ids`, account-wide (no billing scope).
fn catalog_of(ids: &[&str]) -> Vec<underclass::models::ModelInfo> {
    ids.iter()
        .map(|id| underclass::models::ModelInfo {
            id: (*id).to_string(),
            name: (*id).to_string(),
            context: 0,
            input: 0,
            output: 0,
            scope: String::new(),
        })
        .collect()
}

#[hegel::test]
fn test_monitor_minute_bins_partition_recent_attempts(tc: TestCase) {
    let offsets: Vec<i64> = tc.draw(
        gs::vecs(
            gs::integers::<i64>()
                .min_value(-3_800_000)
                .max_value(60_000),
        )
        .max_size(40),
    );
    let now: i64 = tc.draw(
        gs::integers::<i64>()
            .min_value(-1_000_000)
            .max_value(5_000_000),
    );
    let first_minute = now.div_euclid(60_000) - 59;
    let mut expected = vec![0i64; 60];
    let store = Store::in_memory().unwrap();
    for (i, offset) in offsets.iter().enumerate() {
        let ts = now + offset;
        store
            .insert_usage(&UsageRecord {
                id: 0,
                request_id: format!("monitor-{i}"),
                ts,
                endpoint: "/v1/responses".into(),
                backend: "codex".into(),
                model: "model".into(),
                account_id: "account".into(),
                account_label: "label".into(),
                cache_key: None,
                status: 200,
                input_tokens: None,
                output_tokens: None,
                ..Default::default()
            })
            .unwrap();
        let minute = ts.div_euclid(60_000);
        if minute >= first_minute && ts <= now {
            expected[(minute - first_minute) as usize] += 1;
        }
    }
    assert_eq!(store.monitor_minute_bins(now).unwrap(), expected);
}

#[hegel::test]
fn test_monitor_account_totals_partition_month_and_unknown_usage(tc: TestCase) {
    let samples: Vec<(i64, u8, u8)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers::<i64>().min_value(0).max_value(150_000),
            gs::integers::<u8>().min_value(0).max_value(5),
            gs::integers::<u8>().min_value(0).max_value(200)
        ))
        .max_size(40),
    );
    let store = Store::in_memory().unwrap();
    let mut expected: HashMap<String, (i64, i64, i64, i64)> = HashMap::new();
    for (i, (ts, account, amount)) in samples.iter().enumerate() {
        let id = format!("account-{account}");
        let known = amount % 3 != 0;
        let input = known.then_some(*amount as i64);
        let output = known.then_some((*amount / 2) as i64);
        store
            .insert_usage(&UsageRecord {
                id: 0,
                request_id: format!("monitor-{i}"),
                ts: *ts,
                endpoint: "/v1/responses".into(),
                backend: "codex".into(),
                model: "model".into(),
                account_id: id.clone(),
                account_label: id.clone(),
                cache_key: None,
                status: 200,
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            })
            .unwrap();
        if (1_000..100_000).contains(ts) {
            let total = expected.entry(id).or_default();
            total.0 += 1;
            total.1 += i64::from(!known);
            total.2 += input.unwrap_or(0);
            total.3 += output.unwrap_or(0);
        }
    }
    let actual: HashMap<String, (i64, i64, i64, i64)> = store
        .monitor_account_totals(1_000, 100_000)
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.account_id.unwrap(),
                (
                    row.requests,
                    row.unknown_requests,
                    row.input_tokens,
                    row.output_tokens,
                ),
            )
        })
        .collect();
    assert_eq!(actual, expected);
}

#[hegel::test]
fn test_usage_parser_independent_of_chunk_boundaries(tc: TestCase) {
    let input: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
    let output: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
    let splits: Vec<usize> =
        tc.draw(gs::vecs(gs::integers::<usize>().min_value(1).max_value(40)).max_size(30));
    let event = format!(
        "event: response.completed\r\ndata: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}}}\r\n\r\n"
    );
    let mut tap = UsageTap::new(true, UsageDialect::Responses);
    let mut position = 0;
    for size in splits {
        if position >= event.len() {
            break;
        }
        let end = (position + size).min(event.len());
        tap.feed(&event.as_bytes()[position..end]);
        position = end;
    }
    tap.feed(&event.as_bytes()[position..]);
    assert_eq!(
        tap.counts(),
        Some(TokenCounts {
            input_tokens: input,
            output_tokens: output
        })
    );
}

#[hegel::test]
fn test_unknown_usage_never_becomes_zero(tc: TestCase) {
    let input: i64 = tc.draw(gs::integers::<i64>().min_value(-1_000_000).max_value(-1));
    let body = format!("{{\"usage\":{{\"input_tokens\":{input},\"output_tokens\":0}}}}");
    let mut tap = UsageTap::new(false, UsageDialect::Responses);
    tap.feed(body.as_bytes());
    assert_eq!(tap.counts(), None);
}

#[hegel::test]
fn test_usage_groups_partition_measured_and_unknown_attempts(tc: TestCase) {
    let samples: Vec<u8> =
        tc.draw(gs::vecs(gs::integers::<u8>().min_value(0).max_value(200)).max_size(20));
    let store = Store::in_memory().unwrap();
    let mut measured = 0i64;
    let mut unknown = 0i64;
    let mut expected_input = 0i64;
    for (i, sample) in samples.iter().enumerate() {
        let input = if sample % 2 == 0 {
            Some(*sample as i64)
        } else {
            None
        };
        if let Some(value) = input {
            measured += 1;
            expected_input += value;
        } else {
            unknown += 1;
        }
        store
            .insert_usage(&UsageRecord {
                id: 0,
                request_id: format!("request-{i}"),
                ts: i as i64,
                endpoint: "/v1/responses".into(),
                backend: "codex".into(),
                model: format!("model-{}", i % 2),
                account_id: format!("account-{}", i % 3),
                account_label: format!("label-{}", i % 3),
                cache_key: Some(format!("key-{}", i % 4)),
                status: 200,
                input_tokens: input,
                output_tokens: input,
                ..Default::default()
            })
            .unwrap();
    }
    let total = store.usage_summary(&UsageQuery::default()).unwrap();
    assert_eq!(total[0].requests, samples.len() as i64);
    assert_eq!(total[0].measured_requests, measured);
    assert_eq!(total[0].unknown_requests, unknown);
    assert_eq!(total[0].input_tokens, expected_input);
    let grouped = store
        .usage_summary(&UsageQuery {
            group_by: Some("model,account_id,cache_key".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        grouped.iter().map(|g| g.requests).sum::<i64>(),
        samples.len() as i64
    );
    assert_eq!(
        grouped.iter().map(|g| g.input_tokens).sum::<i64>(),
        expected_input
    );
}

#[hegel::test]
fn test_reset_candidate_has_latest_natural_recovery(tc: TestCase) {
    let waits: Vec<i64> = tc.draw(
        gs::vecs(gs::integers::<i64>().min_value(1).max_value(604_800_000))
            .min_size(1)
            .max_size(20),
    );
    let expected = *waits.iter().max().unwrap();
    let candidates = waits
        .into_iter()
        .enumerate()
        .map(|(i, recovery_ms)| Candidate {
            account_id: format!("account-{i}"),
            recovery_ms,
            credit_id: format!("credit-{i}"),
            credit_expiry_ms: None,
        });
    assert_eq!(choose_candidate(candidates).unwrap().recovery_ms, expected);
}

#[hegel::test]
fn test_recovery_is_latest_exhausted_window(tc: TestCase) {
    let primary: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(500_000));
    let secondary: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(500_000));
    let primary_exhausted = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let secondary_exhausted = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let make_window = |deadline: i64, exhausted: bool| Window {
        used_percent: if exhausted { 100.0 } else { 40.0 },
        reset_at: Some(deadline),
        limit_window_seconds: None,
    };
    let limit = RateLimit {
        allowed: Some(false),
        limit_reached: Some(true),
        primary_window: Some(make_window(primary, primary_exhausted)),
        secondary_window: Some(make_window(secondary, secondary_exhausted)),
    };
    let expected = [
        primary_exhausted.then_some(primary * 1000),
        secondary_exhausted.then_some(secondary * 1000),
    ]
    .into_iter()
    .flatten()
    .max();
    assert_eq!(natural_recovery_ms(&limit, 0), expected);
}

#[hegel::test]
fn test_blocked_usage_never_shortens_cooling(tc: TestCase) {
    let current: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000_000));
    let recovery: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000_000));
    let result = blocked_cooling_deadline(AccountStatus::Cooling, current, recovery);
    assert!(result.is_none_or(|deadline| deadline >= current && deadline == recovery));
    assert_eq!(
        blocked_cooling_deadline(AccountStatus::Disabled, current, recovery),
        None
    );
    assert_eq!(
        blocked_cooling_deadline(AccountStatus::AuthError, current, recovery),
        None
    );
}

/// A rotation writes `token_refreshed_at`, which is what the next due check reads. Whatever the
/// clock said before, a freshly rotated account must not come due again until a full interval has
/// elapsed, and must come due again once it has.
#[hegel::test]
fn test_rotation_due_respects_interval_after_each_rotation(tc: TestCase) {
    let now: i64 = tc.draw(
        gs::integers::<i64>()
            .min_value(0)
            .max_value(1_000_000_000_000),
    );
    let mut rotated = account("acct", BackendId::Codex);
    rotated.refresh_token = Some("refresh".into());
    rotated.token_refreshed_at = now;
    assert!(!rotation_due(&rotated, now));
    assert!(!rotation_due(&rotated, now + ROTATION_INTERVAL_MS - 1));
    assert!(rotation_due(&rotated, now + ROTATION_INTERVAL_MS));
    assert!(rotation_due(&rotated, now + ROTATION_INTERVAL_MS + 1));
    // Copilot tokens are never rotated, however old they are.
    let mut copilot = account("acct", BackendId::Copilot);
    copilot.refresh_token = Some("gh".into());
    copilot.token_refreshed_at = 0;
    assert!(!rotation_due(&copilot, now + ROTATION_INTERVAL_MS));
}

/// Applying whatever the plan gate asks for settles the account: a second pass asks for nothing.
/// A free plan always ends disabled, a paid plan never ends up disabled by the gate, and an
/// operator's disable of a paid account is never reverted.
#[hegel::test]
fn test_plan_gate_settles_and_respects_operator_disables(tc: TestCase) {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use underclass::models::{FREE_PLAN_REASON, PlanGate};
    let plans = ["free", "FREE", "plus", "pro", "promax", "business"];
    let plan = plans[tc.draw(gs::integers::<usize>().max_value(plans.len() - 1))];
    let statuses = [
        AccountStatus::Healthy,
        AccountStatus::Cooling,
        AccountStatus::AuthError,
        AccountStatus::Disabled,
    ];
    let mut a = account("acct", BackendId::Codex);
    a.status = statuses[tc.draw(gs::integers::<usize>().max_value(3))];
    a.disabled_reason = tc
        .draw(gs::booleans())
        .then(|| FREE_PLAN_REASON.to_string());
    let claims = format!(r#"{{"chatgpt_plan_type":"{plan}"}}"#);
    a.access_token = Some(format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims)));
    let operator_disabled = a.status == AccountStatus::Disabled && a.disabled_reason.is_none();

    match a.plan_gate() {
        Some(PlanGate::Disable) => {
            a.status = AccountStatus::Disabled;
            a.disabled_reason = Some(FREE_PLAN_REASON.into());
        }
        Some(PlanGate::Enable) => {
            a.status = AccountStatus::Healthy;
            a.disabled_reason = None;
        }
        None => {}
    }
    assert_eq!(a.plan_gate(), None);
    let free = plan.eq_ignore_ascii_case("free");
    if free {
        assert_eq!(a.status, AccountStatus::Disabled);
    } else if operator_disabled {
        assert_eq!(a.status, AccountStatus::Disabled);
        assert_eq!(a.disabled_reason, None);
    } else {
        assert!(a.status != AccountStatus::Disabled || a.disabled_reason.is_none());
    }
}

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

struct Built {
    core: PoolCore,
    accounts: Vec<String>,
}

fn build(backend_spread: usize, account_count: usize) -> Built {
    let store = Store::in_memory().unwrap();
    let mut core = PoolCore::new(&store, openai_surfaces());
    let models: Vec<String> = vec!["gpt-5.5".into(), "gpt-4.1".into()];
    let catalog: Vec<underclass::models::ModelInfo> = models
        .iter()
        .map(|id| underclass::models::ModelInfo {
            id: id.clone(),
            name: id.clone(),
            context: 0,
            input: 0,
            output: 0,
            scope: String::new(),
        })
        .collect();
    core.set_catalog(BackendId::Codex, &catalog);
    if backend_spread > 1 {
        core.set_catalog(BackendId::Copilot, &catalog);
        core.set_catalog(BackendId::Cline, &catalog);
    }
    let mut accounts = Vec::new();
    for i in 0..account_count.max(1) {
        let backend = if backend_spread > 2 && i % 3 == 2 {
            BackendId::Cline
        } else if backend_spread > 1 && i % backend_spread != 0 {
            BackendId::Copilot
        } else {
            BackendId::Codex
        };
        let id = format!("acc-{i}");
        core.insert_account(account(&id, backend));
        accounts.push(id);
    }
    Built { core, accounts }
}

#[hegel::test]
fn test_stickiness_is_stable_while_healthy(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut built = build(2, account_count);
    let requests: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(20));
    let mut bindings: std::collections::HashMap<String, String> = Default::default();
    let mut now = 0i64;
    for key in requests {
        if key.is_empty() {
            continue;
        }
        now += 1;
        if let Ok(sel) = built
            .core
            .select(now, Some(&key), "gpt-5.5", Surface::OpenAI)
        {
            match bindings.get(&key) {
                Some(previous) => {
                    assert_eq!(
                        *previous, sel.account_id,
                        "sticky binding changed without health change"
                    );
                }
                None => {
                    bindings.insert(key, sel.account_id.clone());
                }
            }
        }
    }
}

#[hegel::test]
fn test_rebinding_stays_stable_after_rebind(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(2).max_value(6));
    let mut built = build(2, account_count);
    let key = "session-key";
    let steps: Vec<u8> =
        tc.draw(gs::vecs(gs::integers::<u8>().min_value(0).max_value(2)).max_size(30));
    let mut current: Option<String> = None;
    let mut rebound: Option<String> = None;
    let mut now = 0i64;
    for step in steps {
        now += 1;
        match step {
            0 => {
                if let Ok(sel) = built
                    .core
                    .select(now, Some(key), "gpt-5.5", Surface::OpenAI)
                {
                    current = Some(sel.account_id);
                }
            }
            1 => {
                if let Some(id) = &current {
                    built.core.report(
                        id,
                        Outcome::QuotaExhausted {
                            until_ms: now + 100_000,
                        },
                    );
                    rebound = None;
                }
            }
            _ => {
                if let Ok(sel) = built
                    .core
                    .select(now, Some(key), "gpt-5.5", Surface::OpenAI)
                {
                    if let Some(old) = &current {
                        if old != &sel.account_id {
                            let previous_healthy = built.core.accounts[old].account.healthy();
                            assert!(
                                !previous_healthy,
                                "binding changed while previous account was still healthy"
                            );
                            match &rebound {
                                Some(first_rebind) => {
                                    assert_eq!(*first_rebind, sel.account_id, "unstable rebinding");
                                }
                                None => rebound = Some(sel.account_id.clone()),
                            }
                        }
                    }
                    current = Some(sel.account_id);
                }
            }
        }
    }
}

#[hegel::test]
fn test_selection_never_returns_unhealthy(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let mut built = build(2, account_count);
    let events: Vec<(usize, u8)> = tc.draw(gs::vecs(gs::tuples!(
        gs::integers::<usize>().min_value(0).max_value(7),
        gs::integers::<u8>().min_value(0).max_value(3)
    )));
    let mut now = 0i64;
    for (index, kind) in events {
        now += 1;
        let Some(id) = built.accounts.get(index % built.accounts.len()).cloned() else {
            continue;
        };
        match kind {
            0 => {
                built.core.report(
                    &id,
                    Outcome::QuotaExhausted {
                        until_ms: now + 500,
                    },
                );
            }
            1 => {
                built.core.report(&id, Outcome::AuthFailed);
            }
            2 => {
                built.core.set_status(&id, AccountStatus::Disabled, 0);
            }
            _ => {
                built.core.report(
                    &id,
                    Outcome::QuotaExhausted {
                        until_ms: now + 100,
                    },
                );
                built.core.sweep(now + 101);
            }
        }
        if let Ok(sel) = built
            .core
            .select(now, Some("k"), "gpt-5.5", Surface::OpenAI)
        {
            let state = &built.core.accounts[&sel.account_id];
            assert!(
                state.account.healthy(),
                "selected unhealthy account {:?}",
                state.account.status
            );
        }
    }
}

#[hegel::test]
fn test_selection_excluding_never_returns_excluded(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let mut built = build(2, account_count);
    let mask: Vec<bool> = tc.draw(
        gs::vecs(gs::booleans())
            .min_size(account_count)
            .max_size(account_count),
    );
    let sticky: bool = tc.draw(gs::booleans());
    let key = sticky.then_some("session");
    let first = built
        .core
        .select(1, key, "gpt-5.5", Surface::OpenAI)
        .expect("all accounts healthy")
        .account_id;
    let exclude: std::collections::HashSet<String> = built
        .accounts
        .iter()
        .zip(&mask)
        .filter(|(_, excluded)| **excluded)
        .map(|(id, _)| id.clone())
        .collect();
    match built
        .core
        .select_excluding(2, key, "gpt-5.5", Surface::OpenAI, &exclude)
    {
        Ok(sel) => {
            assert!(
                !exclude.contains(&sel.account_id),
                "selected excluded account"
            );
            if sticky && !exclude.contains(&first) {
                assert_eq!(
                    sel.account_id, first,
                    "sticky binding moved off a non-excluded account"
                );
            }
        }
        Err(_) => assert_eq!(
            exclude.len(),
            built.accounts.len(),
            "a healthy account was left"
        ),
    }
    let unfiltered =
        built
            .core
            .select_excluding(3, key, "gpt-5.5", Surface::OpenAI, &Default::default());
    let plain = built.core.select(3, key, "gpt-5.5", Surface::OpenAI);
    assert_eq!(
        unfiltered.map(|s| s.account_id).ok(),
        plain.map(|s| s.account_id).ok()
    );
}

#[hegel::test]
fn test_saturation_reports_minimum_reset(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(2).max_value(8));
    let mut built = build(1, account_count);
    let resets: Vec<i64> = tc.draw(gs::vecs(
        gs::integers::<i64>().min_value(1).max_value(10_000),
    ));
    let mut latest_by_account: std::collections::HashMap<String, i64> = Default::default();
    for (i, reset) in resets.into_iter().enumerate() {
        let Some(id) = built.accounts.get(i % built.accounts.len()).cloned() else {
            continue;
        };
        built
            .core
            .report(&id, Outcome::QuotaExhausted { until_ms: reset });
        // Reporting never shortens an existing account-wide deadline: the account serves only
        // once every spent window has reset, so the effective deadline is the later one.
        let effective = latest_by_account
            .get(&id)
            .copied()
            .map_or(reset, |previous| previous.max(reset));
        latest_by_account.insert(id, effective);
    }
    if latest_by_account.is_empty() {
        return;
    }
    let expected = latest_by_account.values().copied().min().unwrap();
    for id in &built.accounts {
        if built.core.accounts[id].account.healthy() {
            built
                .core
                .report(id, Outcome::QuotaExhausted { until_ms: expected });
        }
    }
    match built.core.select(0, Some("k"), "gpt-5.5", Surface::OpenAI) {
        Err(SelectError::Saturated { until_ms }) => assert_eq!(until_ms, expected),
        other => panic!("expected saturated, got {other:?}"),
    }
}

#[hegel::test]
fn test_inflight_stays_nonnegative(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let mut built = build(2, account_count);
    let ops: Vec<(usize, u8)> = tc.draw(gs::vecs(gs::tuples!(
        gs::integers::<usize>().min_value(0).max_value(4),
        gs::integers::<u8>().min_value(0).max_value(1)
    )));
    for (index, op) in ops {
        let Some(id) = built.accounts.get(index % built.accounts.len()).cloned() else {
            continue;
        };
        if op == 0 {
            built.core.acquire(&id);
        } else {
            built.core.release(&id);
        }
        assert!(built.core.accounts[&id].inflight < u32::MAX);
    }
}

#[hegel::test]
fn test_cooling_expiry_restores_health(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut built = build(1, account_count);
    let events: Vec<(usize, i64)> = tc.draw(gs::vecs(gs::tuples!(
        gs::integers::<usize>().min_value(0).max_value(5),
        gs::integers::<i64>().min_value(1).max_value(1000)
    )));
    let mut now = 0i64;
    for (index, cooldown) in events {
        now += cooldown;
        let Some(id) = built.accounts.get(index % built.accounts.len()).cloned() else {
            continue;
        };
        built
            .core
            .report(&id, Outcome::QuotaExhausted { until_ms: now });
        built.core.sweep(now);
        let state = &built.core.accounts[&id];
        assert!(
            state.account.healthy(),
            "account still cooling after reset passed"
        );
    }
}

#[hegel::test]
fn test_unknown_models_still_routable_via_codex(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut built = build(2, account_count);
    let unknown: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(15));
    let mut now = 0i64;
    for model in unknown {
        if model.is_empty() {
            continue;
        }
        now += 1;
        if let Ok(sel) = built.core.select(now, None, &model, Surface::OpenAI) {
            assert_eq!(
                sel.backend,
                BackendId::Codex,
                "unknown model '{model}' routed to non-codex backend"
            );
        }
    }
}

#[hegel::test]
fn test_bindings_respect_ttl_and_cap(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let mut built = build(1, account_count);
    let mut now = 0i64;
    let keys: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(30));
    for key in keys {
        if key.is_empty() {
            continue;
        }
        now += 3_600_000;
        let _ = built
            .core
            .select(now, Some(&key), "gpt-5.5", Surface::OpenAI);
        assert!(
            built.core.bindings().len() <= underclass::pool::DEFAULT_BINDING_CAP,
            "binding cache exceeded cap"
        );
    }
    let later = now + underclass::pool::BINDING_TTL_MS + 1;
    built.core.sweep(later);
    assert!(
        built.core.bindings().is_empty(),
        "expired bindings not evicted"
    );
}

const SSE_EVENTS: [&str; 6] = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
    "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\r\n\r\n",
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"Selected model is at capacity. Please try a different model.\"}}}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
    "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\n",
    "data: {\"type\":\"error\",\"code\":\"cyber_policy\",\"message\":\"This request was blocked.\"}\n\n",
];

#[hegel::test]
fn test_stream_outcome_independent_of_chunk_boundaries(tc: TestCase) {
    let picks: Vec<usize> = tc.draw(
        gs::vecs(
            gs::integers::<usize>()
                .min_value(0)
                .max_value(SSE_EVENTS.len() - 1),
        )
        .max_size(8),
    );
    let splits: Vec<usize> =
        tc.draw(gs::vecs(gs::integers::<usize>().min_value(1).max_value(50)).max_size(40));
    let body: String = picks.iter().map(|i| SSE_EVENTS[*i]).collect();
    let mut whole = UsageTap::new(true, UsageDialect::Responses);
    whole.feed(body.as_bytes());
    let mut chunked = UsageTap::new(true, UsageDialect::Responses);
    let mut position = 0;
    for size in splits {
        if position >= body.len() {
            break;
        }
        let end = (position + size).min(body.len());
        chunked.feed(&body.as_bytes()[position..end]);
        position = end;
    }
    chunked.feed(&body.as_bytes()[position..]);
    for end in [StreamEnd::Eof, StreamEnd::UpstreamError, StreamEnd::Dropped] {
        let report = chunked.report(end);
        assert_eq!(report, whole.report(end));
        // Upstream message text never reaches the recorded code.
        if let Some(code) = &report.error_code {
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
                "{code}"
            );
        }
    }
    let first_terminal = picks.iter().find(|i| matches!(**i, 2..=5));
    let expected = match first_terminal {
        Some(2) => "failed",
        Some(3) => "completed",
        Some(4) => "incomplete",
        Some(5) => "failed",
        _ => "truncated",
    };
    assert_eq!(whole.report(StreamEnd::Eof).outcome.as_str(), expected);
}

#[hegel::test]
fn test_concurrency_matches_brute_force(tc: TestCase) {
    let raw: Vec<(i64, i64)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers::<i64>().min_value(-50).max_value(200),
            gs::integers::<i64>().min_value(1).max_value(80)
        ))
        .max_size(25),
    );
    let bucket_ms: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(60));
    let span: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(200));
    let intervals: Vec<(i64, i64)> = raw.iter().map(|(s, d)| (*s, s + d)).collect();
    let buckets = stats::concurrency(&intervals, 0, span, bucket_ms);
    assert_eq!(buckets.len(), stats::bucket_count(0, span, bucket_ms));
    let active_at = |t: i64| intervals.iter().filter(|(s, e)| *s <= t && t < *e).count() as i64;
    for (i, (peak, mean_milli)) in buckets.iter().enumerate() {
        let start = i as i64 * bucket_ms;
        let end = (start + bucket_ms).min(span);
        let brute_peak = (start..end).map(active_at).max().unwrap_or(0);
        let area: i64 = (start..end).map(active_at).sum();
        assert_eq!(*peak, brute_peak, "bucket {i}");
        assert_eq!(*mean_milli, area * 1000 / (end - start), "bucket {i}");
        assert!(*mean_milli <= peak * 1000);
    }
}

#[hegel::test]
fn test_stats_series_partitions_rows_and_events(tc: TestCase) {
    let rows: Vec<(i64, i64, u8, u8, u16)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers::<i64>().min_value(-100).max_value(3_000),
            gs::integers::<i64>().min_value(0).max_value(400),
            gs::integers::<u8>().min_value(0).max_value(5),
            gs::integers::<u8>().min_value(0).max_value(3),
            gs::integers::<u16>().min_value(0).max_value(300)
        ))
        .max_size(30),
    );
    let events: Vec<(i64, u8)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers::<i64>().min_value(-100).max_value(3_000),
            gs::integers::<u8>().min_value(0).max_value(6)
        ))
        .max_size(15),
    );
    let bucket_ms: i64 = tc.draw(gs::integers::<i64>().min_value(50).max_value(1_000));
    let (from, to) = (0i64, 2_500i64);
    let store = Store::in_memory().unwrap();
    let mut expected_tokens = 0i64;
    let mut expected_attempts = vec![0i64; stats::bucket_count(from, to, bucket_ms)];
    let mut requests = std::collections::HashSet::new();
    let mut sessions = std::collections::HashSet::new();
    for (ts, duration, request, model, tokens) in &rows {
        store
            .insert_usage(&UsageRecord {
                request_id: format!("r{request}"),
                ts: *ts,
                endpoint: "/v1/responses".into(),
                backend: "codex".into(),
                model: format!("m{model}"),
                account_id: "a".into(),
                account_label: "a".into(),
                cache_key: Some(format!("k{}", request % 3)),
                status: 200,
                input_tokens: Some(i64::from(*tokens)),
                output_tokens: Some(1),
                duration_ms: Some(*duration),
                ..Default::default()
            })
            .unwrap();
        if (from..to).contains(ts) {
            expected_tokens += i64::from(*tokens);
            expected_attempts[((ts - from) / bucket_ms) as usize] += 1;
            requests.insert(*request);
            sessions.insert(request % 3);
        }
    }
    let mut expected_events = vec![0i64; expected_attempts.len()];
    for (i, (ts, kind)) in events.iter().enumerate() {
        store
            .insert_upstream_events(&[UpstreamEvent {
                ts: *ts,
                source: "proxy".into(),
                kind: underclass::store::EVENT_KINDS[*kind as usize].into(),
                dedupe_key: Some(format!("e{i}")),
                ..Default::default()
            }])
            .unwrap();
        if (from..to).contains(ts) {
            expected_events[((ts - from) / bucket_ms) as usize] += 1;
        }
    }
    let series = store.stats_series(from, to, bucket_ms, Dim::Total).unwrap();
    let buckets = series
        .first()
        .map(|s| s.buckets.clone())
        .unwrap_or_default();
    let attempts: Vec<i64> = buckets.iter().map(|b| b.totals.attempts).collect();
    let event_counts: Vec<i64> = buckets
        .iter()
        .map(|b| b.totals.events.values().sum())
        .collect();
    if series.is_empty() {
        assert!(
            expected_attempts.iter().all(|n| *n == 0) && expected_events.iter().all(|n| *n == 0)
        );
    } else {
        assert_eq!(attempts, expected_attempts);
        assert_eq!(event_counts, expected_events);
        assert_eq!(
            buckets.iter().map(|b| b.totals.input_tokens).sum::<i64>(),
            expected_tokens
        );
        for b in &buckets {
            assert!(
                b.totals.sessions <= b.totals.requests && b.totals.requests <= b.totals.attempts
            );
            assert_eq!(b.totals.outcomes.values().sum::<i64>(), b.totals.attempts);
        }
    }
    let distinct = store.stats_distinct(from, to, Dim::Total).unwrap();
    let (r, k) = distinct.first().map(|(_, r, k)| (*r, *k)).unwrap_or((0, 0));
    assert_eq!((r, k), (requests.len() as i64, sessions.len() as i64));
    // Per-model series partition the total.
    let by_model = store.stats_series(from, to, bucket_ms, Dim::Model).unwrap();
    let model_tokens: i64 = by_model
        .iter()
        .flat_map(|s| s.buckets.iter())
        .map(|b| b.totals.input_tokens)
        .sum();
    assert_eq!(model_tokens, expected_tokens);
}

#[hegel::test]
fn test_daily_rollup_matches_raw_day(tc: TestCase) {
    let rows: Vec<(i64, i64, u8)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers::<i64>()
                .min_value(0)
                .max_value(3 * stats::DAY_MS),
            gs::integers::<i64>()
                .min_value(0)
                .max_value(5 * stats::HOUR_MS),
            gs::integers::<u8>().min_value(0).max_value(3)
        ))
        .max_size(25),
    );
    let day: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(2));
    let store = Store::in_memory().unwrap();
    for (i, (ts, duration, account)) in rows.iter().enumerate() {
        store
            .insert_usage(&UsageRecord {
                request_id: format!("r{i}"),
                ts: *ts,
                endpoint: "/v1/responses".into(),
                backend: "codex".into(),
                model: "m".into(),
                account_id: format!("a{account}"),
                account_label: format!("label{account}"),
                status: 200,
                input_tokens: Some(i as i64),
                output_tokens: Some(1),
                duration_ms: Some(*duration),
                stream_outcome: Some("completed".into()),
                ..Default::default()
            })
            .unwrap();
    }
    store.rollup_stats_day(day, 10 * stats::DAY_MS).unwrap();
    for dim in Dim::all() {
        let raw = store
            .stats_series(
                day * stats::DAY_MS,
                (day + 1) * stats::DAY_MS,
                stats::DAY_MS,
                dim,
            )
            .unwrap();
        let rolled = store.stats_daily(day, day + 1, dim).unwrap();
        let raw: Vec<(String, stats::Totals)> = raw
            .into_iter()
            .map(|s| (s.key, s.buckets[0].totals.clone()))
            .collect();
        let rolled: Vec<(String, stats::Totals)> =
            rolled.into_iter().map(|(_, k, _, t)| (k, t)).collect();
        if dim == Dim::Total && raw.is_empty() {
            assert_eq!(
                rolled,
                vec![(stats::TOTAL_KEY.to_string(), stats::Totals::default())]
            );
        } else {
            assert_eq!(rolled, raw);
        }
    }
}

#[hegel::test]
fn test_cline_model_namespace_round_trips(tc: TestCase) {
    let raw: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(20));
    for model in raw {
        if model.is_empty() {
            continue;
        }
        let upstream = underclass::cline::upstream_model_id(&model);
        let namespaced = underclass::cline::pool_model_id(upstream);
        assert_eq!(
            underclass::cline::upstream_model_id(&namespaced),
            upstream,
            "namespace must strip exactly one cline/ prefix"
        );
        assert_eq!(
            underclass::cline::pool_model_id(underclass::cline::upstream_model_id(&namespaced)),
            namespaced
        );
    }
}

#[hegel::test]
fn test_cline_catalog_always_contains_subscription_models(tc: TestCase) {
    let ids: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(20));
    let fetched: Vec<underclass::models::ModelInfo> = ids
        .into_iter()
        .filter(|id| !id.is_empty() && !id.ends_with(":batch"))
        .map(|id| underclass::models::ModelInfo {
            id: underclass::cline::pool_model_id(&id),
            name: id,
            context: 0,
            input: 0,
            output: 0,
            scope: String::new(),
        })
        .collect();
    let merged = underclass::cline::merge_catalog(fetched);
    for subscription in underclass::cline::default_catalog() {
        assert!(
            merged.iter().any(|model| model.id == subscription.id),
            "ClinePass model {} dropped from the merged catalog",
            subscription.id
        );
    }
    let mut sorted = merged
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    sorted.sort();
    let actual = merged
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(actual, sorted, "merged catalog must stay sorted");
}

/// The feed is the only source of ClinePass and free models, so merging a listing with it must be
/// monotonic: no listed model is dropped, every feed model survives, no id is duplicated, and every
/// feed row keeps the billing scope its id derives (a free model must not inherit `cline-pass`).
#[hegel::test]
fn test_cline_feed_merge_is_monotonic_and_scoped(tc: TestCase) {
    let listing_ids: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(15));
    let feed_body = underclass::models::ModelInfo {
        id: String::new(),
        name: String::new(),
        context: 0,
        input: 0,
        output: 0,
        scope: String::new(),
    };
    let feed_ids: Vec<String> = tc.draw(gs::vecs(gs::text()).max_size(15));
    let listing = listing_ids
        .into_iter()
        .filter(|id| !id.is_empty())
        .map(|id| underclass::models::ModelInfo {
            id: underclass::cline::pool_model_id(&id),
            name: id,
            ..feed_body.clone()
        })
        .collect::<Vec<_>>();
    let feed = feed_ids
        .into_iter()
        .filter(|id| !id.is_empty())
        .map(|id| underclass::models::ModelInfo {
            scope: underclass::cline::quota_scope(&id).to_string(),
            id: underclass::cline::pool_model_id(&id),
            name: id,
            ..feed_body.clone()
        })
        .collect::<Vec<_>>();

    let merged = underclass::cline::merge_discovered_models(listing.clone(), &feed);

    for listed in &listing {
        assert!(
            merged.iter().any(|model| model.id == listed.id),
            "merging the feed dropped listed model {}",
            listed.id
        );
    }
    for discovered in &feed {
        assert!(
            merged.iter().any(|model| model.id == discovered.id),
            "merging dropped feed model {}",
            discovered.id
        );
        let row = merged
            .iter()
            .find(|model| model.id == discovered.id)
            .unwrap();
        assert_eq!(
            row.scope,
            underclass::cline::quota_scope(underclass::cline::upstream_model_id(&row.id)),
            "{} must bill against the surface its id names",
            row.id
        );
    }
    let ids = merged
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(ids, unique, "merged catalog must hold one row per id");
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "merged catalog must stay sorted");

    // Whatever the feed carries, the parser returns namespaced rows that always name a surface -
    // never an empty scope, which would silently make a model account-wide.
    let body = serde_json::json!({
        "clinePass": feed.iter().map(|m| serde_json::json!({"id": underclass::cline::upstream_model_id(&m.id)})).collect::<Vec<_>>(),
        "free": feed.iter().map(|m| serde_json::json!({"id": underclass::cline::upstream_model_id(&m.id)})).collect::<Vec<_>>(),
    });
    for model in underclass::cline::parse_recommended_models(&body) {
        assert!(
            model.id.starts_with(underclass::cline::MODEL_PREFIX),
            "{} must be namespaced",
            model.id
        );
        assert_eq!(
            underclass::cline::pool_model_id(underclass::cline::upstream_model_id(&model.id)),
            model.id,
            "the pool namespace must round-trip"
        );
        assert_eq!(
            model.scope,
            underclass::cline::quota_scope(underclass::cline::upstream_model_id(&model.id)),
            "{} must name a billing surface",
            model.id
        );
        assert!(
            !model.scope.is_empty(),
            "{} must not be account-wide",
            model.id
        );
    }
}

/// Builds a chat-completions SSE payload from ordered text deltas plus a finish reason, ending with
/// the `[DONE]` sentinel only when `sentinel` is set.
fn chat_sse(
    deltas: &[String],
    reasoning: &[String],
    finish: Option<&str>,
    sentinel: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in deltas.iter().zip(reasoning.iter()) {
        let (text, think) = chunk;
        let mut delta = serde_json::Map::new();
        if !text.is_empty() {
            delta.insert("content".into(), serde_json::Value::String(text.clone()));
        }
        if !think.is_empty() {
            delta.insert("reasoning".into(), serde_json::Value::String(think.clone()));
        }
        let payload = serde_json::json!({"choices": [{"delta": delta}]});
        out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
    }
    if let Some(finish) = finish {
        let payload = serde_json::json!({"choices": [{"delta": {}, "finish_reason": finish}]});
        out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
    }
    if sentinel {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
    out
}

fn stream_frames(frames: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(frames)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

fn terminal_events(frames: &[u8]) -> Vec<String> {
    stream_frames(frames)
        .into_iter()
        .filter_map(|frame| frame.get("type")?.as_str().map(str::to_string))
        .filter(|kind| {
            matches!(
                kind.as_str(),
                "response.completed" | "response.incomplete" | "response.failed"
            )
        })
        .collect()
}

#[hegel::test]
fn test_chat_stream_translation_is_chunk_boundary_independent(tc: TestCase) {
    let deltas: Vec<String> = tc.draw(gs::vecs(gs::text().max_size(6)).max_size(6));
    let reasoning: Vec<String> = deltas.iter().map(|_| String::new()).collect();
    let sse = chat_sse(&deltas, &reasoning, Some("stop"), true);
    let splits: Vec<usize> = tc.draw(
        gs::vecs(
            gs::integers::<usize>()
                .min_value(0)
                .max_value(sse.len().max(1)),
        )
        .max_size(6),
    );

    let mut whole = underclass::responses::ChatStreamTranslator::new("m", "resp_1", 0);
    let mut expected = whole.feed(&sse);
    expected.extend(whole.finish_on_eof());

    let mut piecewise = underclass::responses::ChatStreamTranslator::new("m", "resp_1", 0);
    let mut actual = Vec::new();
    let mut boundaries = splits;
    boundaries.push(0);
    boundaries.push(sse.len());
    boundaries.sort_unstable();
    let mut previous = 0usize;
    for boundary in boundaries {
        let boundary = boundary.min(sse.len());
        actual.extend(piecewise.feed(&sse[previous..boundary]));
        previous = boundary;
    }
    actual.extend(piecewise.feed(&sse[previous..]));
    actual.extend(piecewise.finish_on_eof());

    assert_eq!(
        stream_frames(&actual),
        stream_frames(&expected),
        "chunk boundaries must not change the emitted frames"
    );
}

#[hegel::test]
fn test_chat_stream_translation_emits_exactly_one_terminal_event(tc: TestCase) {
    let deltas: Vec<String> = tc.draw(gs::vecs(gs::text().max_size(6)).max_size(6));
    let reasoning: Vec<String> = deltas.iter().map(|_| String::new()).collect();
    let finish = tc.draw(gs::sampled_from(vec![
        None,
        Some("stop"),
        Some("length"),
        Some("content_filter"),
        Some("tool_calls"),
        Some("error"),
    ]));
    let sentinel: bool = tc.draw(gs::booleans());
    let sse = chat_sse(&deltas, &reasoning, finish, sentinel);

    let mut translator = underclass::responses::ChatStreamTranslator::new("m", "resp_1", 0);
    let mut frames = translator.feed(&sse);
    frames.extend(translator.finish_on_eof());
    // Finishing twice must not duplicate the terminal event.
    frames.extend(translator.finish_on_eof());

    assert_eq!(
        terminal_events(&frames).len(),
        1,
        "the translator must emit exactly one terminal event"
    );
}

#[hegel::test]
fn test_chat_stream_text_deltas_concatenate(tc: TestCase) {
    let deltas: Vec<String> = tc.draw(gs::vecs(gs::text().max_size(6)).max_size(6));
    let reasoning: Vec<String> = deltas.iter().map(|_| String::new()).collect();
    let sse = chat_sse(&deltas, &reasoning, Some("stop"), true);

    let mut translator = underclass::responses::ChatStreamTranslator::new("m", "resp_1", 0);
    let mut frames = translator.feed(&sse);
    frames.extend(translator.finish_on_eof());

    let mut streamed = String::new();
    for frame in stream_frames(&frames) {
        if frame.get("type").and_then(|v| v.as_str()) == Some("response.output_text.delta") {
            streamed.push_str(
                frame
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            );
        }
    }
    assert_eq!(
        streamed,
        deltas.concat(),
        "streamed text must equal the concatenated deltas"
    );
}

#[hegel::test]
fn test_chat_stream_error_finish_still_yields_one_terminal(tc: TestCase) {
    let deltas: Vec<String> = tc.draw(gs::vecs(gs::text().max_size(6)).max_size(6));
    let reasoning: Vec<String> = deltas.iter().map(|_| String::new()).collect();
    let finish = tc.draw(gs::sampled_from(vec![Some("error"), Some("stop"), None]));
    let sentinel: bool = tc.draw(gs::booleans());
    let sse = chat_sse(&deltas, &reasoning, finish, sentinel);

    let mut translator = underclass::responses::ChatStreamTranslator::new("m", "resp_1", 0);
    let mut frames = translator.feed(&sse);
    // Both a clean EOF and an abrupt upstream error close through `finish_on_eof`.
    frames.extend(translator.finish_on_eof());
    frames.extend(translator.finish_on_eof());

    let terminals = terminal_events(&frames);
    assert_eq!(terminals.len(), 1, "exactly one terminal event per stream");
    if finish == Some("error") {
        assert_eq!(
            terminals[0], "response.failed",
            "an error finish must fail the response"
        );
    }
}

#[hegel::test]
fn test_completion_to_response_status_matches_finish_reason(tc: TestCase) {
    let finish = tc.draw(gs::sampled_from(vec![
        "stop",
        "length",
        "content_filter",
        "error",
        "tool_calls",
    ]));
    let chat = serde_json::json!({
        "choices": [{"finish_reason": finish, "message": {"content": "text"}}],
    });
    let response = underclass::responses::completion_to_response(&chat, "m", "resp_1", 0);
    let status = response["status"].as_str().unwrap();
    let expected = match finish {
        "length" | "content_filter" => "incomplete",
        "error" => "failed",
        _ => "completed",
    };
    assert_eq!(
        status, expected,
        "finish_reason {finish} mapped to {status}"
    );
    if finish == "error" {
        assert_eq!(response["output"], serde_json::json!([]));
        assert!(
            response.get("error").is_some(),
            "a failed response must carry an error"
        );
    }
}

#[hegel::test]
fn test_chat_stream_finish_is_idempotent(tc: TestCase) {
    let deltas: Vec<String> = tc.draw(gs::vecs(gs::text().max_size(6)).max_size(6));
    let reasoning: Vec<String> = deltas.iter().map(|_| String::new()).collect();
    let finishes: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).max_size(4));
    let sse = chat_sse(&deltas, &reasoning, Some("stop"), true);

    let mut translator = underclass::responses::ChatStreamTranslator::new("m", "resp_1", 0);
    let mut frames = translator.feed(&sse);
    // Every flush after the first must be a no-op: an upstream read error after `[DONE]` flushes a
    // second time and must not append a duplicate terminal event.
    for _ in finishes.iter().chain(std::iter::once(&true)) {
        let tail = translator.finish_on_eof();
        assert!(tail.is_empty(), "a repeated flush must emit nothing");
        frames.extend(tail);
    }
    assert_eq!(terminal_events(&frames).len(), 1);
}

/// Scoped quota cooling is a routing predicate over injected data, so it belongs in the pure-core
/// property suite: cooling one billing surface must never make a differently scoped model
/// unreachable, while account-wide cooling must block every scope.
#[hegel::test]
fn test_scoped_cooling_only_blocks_the_reported_surface(tc: TestCase) {
    let scopes: Vec<String> = tc.draw(
        gs::vecs(gs::sampled_from(vec![
            "credits".to_string(),
            "cline-pass".to_string(),
        ]))
        .min_size(4)
        .max_size(4),
    );
    // A zero-length deadline is already expired, so the generator starts at 1.
    let cooldowns: Vec<i64> = tc.draw(
        gs::vecs(gs::integers::<i64>().min_value(1).max_value(10_000))
            .min_size(4)
            .max_size(4),
    );
    // A short vector would truncate the zip and silently skip every assertion below.
    let account_wide: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).min_size(4).max_size(4));

    let store = Store::in_memory().unwrap();
    let mut core = PoolCore::new(&store, openai_surfaces());
    let catalog: Vec<underclass::models::ModelInfo> = ["credits", "cline-pass"]
        .iter()
        .map(|scope| underclass::models::ModelInfo {
            id: format!("cline/model-{scope}"),
            name: scope.to_string(),
            context: 0,
            input: 0,
            output: 0,
            scope: (*scope).to_string(),
        })
        .collect();
    core.set_catalog(BackendId::Cline, &catalog);
    core.insert_account(account("acct", BackendId::Cline));

    let mut now: i64 = 1;
    // The account-wide deadline outlives the report that set it; scoped deadlines are per surface.
    let mut account_deadline: Option<i64> = None;
    for ((scope, cooldown), wide) in scopes
        .iter()
        .zip(cooldowns.iter())
        .zip(account_wide.iter().cycle())
    {
        now += 1;
        let until = now.saturating_add(*cooldown);
        let model = format!("cline/model-{scope}");
        let outcome = Outcome::QuotaExhausted { until_ms: until };
        let status_before = core.account("acct").unwrap().status;
        let account_reset_before = core.account("acct").unwrap().reset_at;
        if *wide {
            core.report("acct", outcome);
            // Account-wide reports never shorten an existing deadline.
            account_deadline = Some(account_deadline.map_or(until, |previous| previous.max(until)));
        } else {
            core.report_for("acct", &model, outcome);
            let account = core.account("acct").unwrap();
            assert_eq!(
                account.status, status_before,
                "a scoped quota must never change the account-wide status"
            );
            assert_eq!(
                account.reset_at, account_reset_before,
                "a scoped quota must never write the account-wide deadline"
            );
        }
        for other in ["credits", "cline-pass"] {
            let blocked_by_account = account_deadline.is_some_and(|deadline| deadline > now);
            let blocked_by_scope = core
                .account("acct")
                .unwrap()
                .scope_cooling_until(Some(other))
                .is_some_and(|deadline| deadline > now);
            let selected = core
                .select(now, None, &format!("cline/model-{other}"), Surface::OpenAI)
                .is_ok();
            assert_eq!(
                selected,
                !(blocked_by_account || blocked_by_scope),
                "selection for {other} must match its own cooling state (account-wide:                  {blocked_by_account}, scoped: {blocked_by_scope})"
            );
            if !*wide && other == scope.as_str() {
                assert!(
                    !selected,
                    "{} must be blocked after its own quota error",
                    scope
                );
            }
        }
    }
}

/// Selection never crosses protocols: an OpenAI-shaped request never lands on a backend that
/// only speaks the Anthropic surface, and the other way round.
#[hegel::test]
fn test_surface_partition_never_crosses_protocols(tc: TestCase) {
    let account_count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let now: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
    let store = Store::in_memory().unwrap();
    let mut core = PoolCore::new(&store, openai_surfaces());
    core.set_catalog(BackendId::Codex, &catalog_of(&["gpt-5.5"]));
    core.set_catalog(BackendId::Claude, &catalog_of(&["claude-opus-5-5"]));
    for i in 0..account_count.max(1) {
        core.insert_account(account(&format!("codex-{i}"), BackendId::Codex));
        core.insert_account(account(&format!("claude-{i}"), BackendId::Claude));
    }
    if let Ok(selection) = core.select(now, None, "gpt-5.5", Surface::OpenAI) {
        assert_ne!(
            selection.backend,
            BackendId::Claude,
            "an OpenAI-shaped request must not select a Claude account"
        );
    }
    if let Ok(selection) = core.select(now, None, "claude-opus-5-5", Surface::Anthropic) {
        assert_eq!(
            selection.backend,
            BackendId::Claude,
            "an Anthropic-shaped request must not select an OpenAI account"
        );
    }
}

/// A model-scoped cooldown blocks that model on the account and leaves the account's other
/// models eligible, and it expires on its own.
#[hegel::test]
fn test_model_cooldown_scopes_one_model(tc: TestCase) {
    let now: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
    let window: i64 = tc.draw(gs::integers::<i64>().min_value(1).max_value(100_000));
    let store = Store::in_memory().unwrap();
    let mut core = PoolCore::new(&store, openai_surfaces());
    core.set_catalog(
        BackendId::Claude,
        &catalog_of(&["claude-opus-5-5", "claude-sonnet-5-5"]),
    );
    core.insert_account(account("claude-a", BackendId::Claude));

    core.report_model_quota("claude-a", "claude-opus-5-5", now + window);
    assert!(
        matches!(
            core.select(now, None, "claude-opus-5-5", Surface::Anthropic),
            Err(SelectError::Saturated { .. })
        ),
        "the cooled model is not selectable"
    );
    assert!(
        core.select(now, None, "claude-sonnet-5-5", Surface::Anthropic)
            .is_ok(),
        "the account still serves its other models"
    );
    assert!(
        core.select(
            now + window + 1,
            None,
            "claude-opus-5-5",
            Surface::Anthropic
        )
        .is_ok(),
        "the cooldown expires on its own"
    );
}
