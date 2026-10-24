# 0025 - Free-Plan Gate

## Status

Accepted

## Context

A Codex account whose ChatGPT subscription lapses to `free` stays in the pool. Its access token
carries `chatgpt_plan_type: free`, and every request routed to it fails on plan limits until an
operator notices and disables it by hand. On 2026-10-22, `codex02` was in exactly this state:
manually `Disabled`, plan `free`. Nothing would bring it back if the subscription were renewed;
the operator would have to remember to enable it.

`Disabled` is otherwise purely an operator state. An automatic rule that both sets and clears it
must not revert a disable the operator chose for some other reason.

## Decision

- `Account::plan_gate` (pure, no I/O) decides from `plan_label()`: a `free` plan (case-insensitive)
  yields `Disable`; an account disabled *for* a free plan whose plan is now known and not free
  yields `Enable`; an unknown plan yields nothing, so a missing or unparsable token never flips
  status.
- A new nullable `accounts.disabled_reason` column records why underclass disabled an account.
  The gate writes `free_plan`; operator enable/disable through the admin API clears it, so an
  operator decision always supersedes the gate and an operator disable of a paid account is never
  auto-reverted.
- A free-plan account that is already operator-disabled is adopted (its reason becomes
  `free_plan`), so it returns on upgrade.
- `enforce_plan_gates` runs on the existing 10-second housekeeping tick, before the pool resyncs
  from the store. Re-enabled accounts go to `Healthy` with `reset_at = 0`.
- Codex plans are read from the stored access token's claims. ADR 0019's daily rotation refreshes
  every Codex account regardless of status, so an upgrade is picked up within a day (sooner if a
  refresh happens for another reason).

## Consequences

- Free accounts leave rotation automatically and come back automatically after an upgrade.
- An operator who enables a free account sees it disabled again within ten seconds; enabling is
  only meaningful once the plan changes.
- Claude Code accounts are covered by the same gate, but `claude::plan_for` never reports `free`
  today and `poll_claude_usage` skips disabled accounts, so in practice the gate acts on Codex.
