# Active-run history pruning plan

## Scope and lifecycle decision

Prune only the selected project's existing active `Run::agents` map when `action start` enters `App::start()`. Perform the locked state update before the `agent_exists` check, so the same rule runs before either focusing a live Lead or creating and relaunching a missing Lead. Do not add pruning to the global `App::startup()` reconciliation hook, Lead exit/idle events, `integrate_agent`, `cleanup_agent`, or `finish_run`.

This intentionally accepts growth during a Lead session: integrated records created after startup remain available until the next explicit `action start`. Cleanup and attribution can therefore finish against a stable within-session history, while repeated starts compact history at the session boundary.

## Current findings

- **High (Blockers):** Shared-checkout completion in `src/app.rs::complete_agent` excludes commits attributed by another shared-checkout agent's `claimed_commits`, or by the legacy `report.commit_sha` fallback when claims are empty. Removing that evidence while an older shared-checkout agent can still complete or resume could make it claim another agent's commit, fail scope validation, or report false ownership.
- **Mid:** `App::start` currently returns the active run from its state transaction and immediately branches to focus or relaunch. The pruning hook belongs inside that transaction, immediately before cloning the active run, to make persistence atomic and guarantee it precedes both branches.
- **Mid:** Successful `cleanup_agent` clears `workspace_id`, `tab_id`, `pane_id`, and `checkout_path` and resets `cleanup_attempts`; failed cleanup retains resource identifiers when known and increments attempts. `Agent::error` is additional retained recovery/failure evidence.
- **Low:** The shared `Store` contains multiple projects, and a run carries identity and sequencing beyond its agent map. Replacing the project or run would risk unrelated state; pruning must be a narrow `agents.retain` operation.
- **Wish:** None; no schema or new persisted cleanup marker is required for this change.

## Eligibility and safety rules

Add a small, pure helper in `src/app.rs` that mutates only `Run::agents`. An agent is a base pruning candidate only when every condition below is true:

1. `status == AgentStatus::Integrated`.
2. `workspace_id`, `tab_id`, `pane_id`, and `checkout_path` are all `None`.
3. `cleanup_attempts == 0`.
4. `error.is_none()`.

Every non-`Integrated` status is retained without exception. Any candidate with a resource locator, failed-cleanup counter, error, or future explicitly identified recoverability dependency is retained. Fields such as the report are not generally recovery evidence after successful resource cleanup, except for commit attribution as described next.

Before retaining candidates, compute whether the run contains any non-`Integrated`, shared-checkout agent (`use_worktree == false`). Treat all such statuses as unfinished/resumable, including `Failed` and `Cancelled`: `prompt_agent` can move a still-running record back to `Working`, and completion does not currently impose a narrower prior-status gate. If one exists, preserve every cleaned candidate that could contribute shared-checkout attribution:

- preserve any candidate with non-empty `claimed_commits` (including anomalous older data, to fail closed); and
- preserve a shared-checkout candidate with `report.commit_sha` when `claimed_commits` is empty, matching the current legacy fallback.

Do not try to prove safety with commit reachability or range heuristics. When no retained unfinished/resumable shared-checkout agent exists, those attribution records may be removed: a later shared-checkout agent is spawned from the then-current `HEAD`, so its completion range starts after already integrated commits. Cleaned worktree records with only their normal report commit are not attribution dependencies because the attribution loop explicitly excludes worktree agents.

## Implementation touchpoints

1. **`src/app.rs` — pure pruning predicate/helper.** Keep the policy beside the lifecycle and attribution code rather than changing the serialized model. Derive the resumable-shared-checkout guard from the full pre-prune map, then retain records according to the rules above. Document why legacy `report.commit_sha` and `claimed_commits` mirror `complete_agent` semantics.
2. **`src/app.rs` — `App::start`.** In the existing `StateStore::update` closure, when `project.active_run` resolves to an active run, prune that run in place and return its clone. Leave new-run creation and the existing completed-run cleanup behavior unchanged. Do not change `Run.id`, `Run.status`, branch/workspace identity, `lead`, `created_unix_ms`, `next_agent`, `last_error`, `project.active_run`, non-active runs, or any other `Store::projects` entry.
3. **`tests/cli_flow.rs` — start-boundary coverage.** Add focused fixtures using a hand-written schema-version-1 store and the fake Herdr executable. Exercise both a live-Lead focus and a missing-Lead relaunch. Assert pruning has already persisted on each path, the same active run remains selected, and normal relaunch-only Lead terminal/config refreshes are the only allowed non-agent differences.
4. **`src/app.rs` tests — policy matrix.** Add compact unit fixtures for the pure helper so every status and recovery/attribution guard is covered without duplicating large CLI flows.

No `src/model.rs` or `src/state.rs` schema change is needed. Existing `schema_version: 1` files deserialize through current defaults and remain version 1 when rewritten; there is no migration, version bump, or new required field.

## Exact tests and acceptance

Add tests with these assertions:

- A cleaned `Integrated` record is removed, while each non-`Integrated` status (`Starting`, `Working`, `Blocked`, `Failed`, `Cancelled`, `Completed`, `Integrating`, and `Conflict`) remains byte-for-byte equivalent.
- Separate `Integrated` records remain when any one of `workspace_id`, `tab_id`, `pane_id`, `checkout_path`, nonzero `cleanup_attempts`, or `error` is present. This covers cleanup disabled, partial cleanup, retryable failure, exhausted retries, and explicit error evidence.
- With any retained non-`Integrated` shared-checkout agent, a cleaned record remains if it has `claimed_commits`; a cleaned shared-checkout record also remains when it has only legacy `report.commit_sha`. Include a failed or cancelled shared agent in the matrix to prove the conservative resumability rule.
- With no unfinished/resumable shared-checkout agent, the same safely cleaned attribution records are removed. A retained worktree-only unfinished agent does not block removal, and a cleaned Integrated worktree agent's normal report commit alone does not block removal.
- `action start` prunes before returning `focused` and before returning `started` for a Lead relaunch. The fixture's run ID/key, `active_run`, status, base fields, creation time, Lead state except existing relaunch refreshes, `next_agent`, `last_error`, non-Integrated agents, non-active runs, and a sentinel second project remain unchanged.
- A fixture explicitly marked `schema_version: 1`, with currently defaulted optional agent fields omitted where appropriate, starts successfully, remains version 1, and is pruned without migration.
- No test invokes pruning through global `startup`, Lead idle/exit events, integration, or agent cleanup. Existing cleanup retry tests continue to pass, demonstrating those triggers were not coupled to compaction.

Acceptance is met when the new targeted tests pass along with:

```text
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
sh -n scripts/*.sh
git diff --check
```
