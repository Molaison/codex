# Official ChatGPT composer predictions in the CLI

This downstream branch adds optional next-message suggestions to the Codex TUI while preserving the main conversation's provider. For example, a conversation can use a custom `ywl` provider while suggestions use an eligible ChatGPT login through the official composer-prediction protocol. Tab copies a gray suggestion into the composer; it never submits it.

This is a community CLI integration, not an upstream-supported CLI feature. Its prediction companion is pinned to desktop package `26.1013.3656.0` (backend `0.163.0-alpha.10.2`). Protocol changes require revalidation.

## Trigger behavior

| Situation | Behavior |
| --- | --- |
| A live turn completes successfully | Request a live prediction once while the composer is empty and idle. |
| A completed conversation is resumed or replayed | Request catch-up only for the latest completed turn. |
| MCP startup is pending during resume, including before its first status arrives | Defer the completion until configured servers settle, then recheck the thread, turn, draft, queue, and input state. |
| Duplicate completion or snapshot for the same turn | Keep the original request; do not cancel, resend, or resample it in the same widget. |
| New turn, typing, paste, modal, thread switch, or disconnect | Cancel or reject obsolete results. A fresh reconnect may request catch-up again. |
| Draft, queued work, Plan mode, active goal, failed/interrupted turn | Do not insert a prediction. |
| Official sampling skips the request or the model returns `null` | Show nothing. Do not retry to defeat sampling. |
| Missing/expired login, ineligible account/model, unavailable backend | Fail closed. No API or ordinary ChatGPT inference fallback. |

Official sampling controls whether an eligible trigger actually produces a request. A successful prediction may also return no suggestion. Stable event handling does not promise a visible suggestion after every reply. The deduplication state is local to the current TUI widget, not a cross-process cache.

## Authentication and request boundary

The helper fetches the account's official settings from `https://chatgpt.com/backend-api/wham/predictions/config`, including prompt, effort, supported models, and separate live/catch-up sampling. It currently accepts the verified Pro login and `gpt-6.1-sol` / `gpt-6-astra` models. It rejects an access token expiring within five minutes. Refresh the shared ChatGPT login through the normal official login flow when required; this helper does not rotate its refresh token.

The pinned official backend runs in a temporary directory with an isolated `HOME`, `CODEX_HOME`, database, and configuration. Only the ChatGPT access/ID tokens and account ID are copied; the refresh token is empty. API key and base-URL environment variables are excluded. The main conversation's provider configuration and authentication files are not edited.

The only inference turn must follow a successful `thread/fork` with `experimentalPredictionMode: true`. It uses `turnTrigger: "composer_predictions"`, a unique `clientUserMessageId`, and the real source thread ID, turn ID, and `live`/`catch_up` trigger in client metadata. The returned model/provider must match. Tools are disabled; prediction context contains at most eight recent user/final-assistant text messages. Failure at any boundary stops prediction.

The official [composer prediction policy](https://help.openai.com/en/articles/20001601-composer-predictions-in-codex) is the basis for using the official free prediction path. This integration does not prove billing by comparing rounded usage percentages. Sending an accepted suggestion is an ordinary main-conversation request and follows that conversation's provider and billing.

## Build and installation

The initial base is upstream `rust-v0.162.1`, commit `092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`. Runtime is currently verified on Linux x86_64 with Python 3.6 or later. Windows/macOS TUI builds fail closed for prediction.

```sh
git clone --branch feature/official-cli-predictions https://github.com/Molaison/codex.git
cd codex
python3 scripts/test_official_predictions.py
cd codex-rs
# The release tag's lockfile still has 0.0.0 workspace versions.
# Normalize workspace packages before using --locked; retain dependency pins.
cargo update --workspace
RUST_MIN_STACK=8388608 NEXTEST_PROFILE=local cargo nextest run --locked \
  -p codex-tui --lib \
  -E 'test(next_prompt_prediction) | test(replay_thread_snapshot) | test(mcp_startup)'
CARGO_BUILD_JOBS=4 cargo build --release --locked -p codex-cli --bin codex
```

Install the resulting CLI at a new path and keep your existing executable/launcher for rollback. Place your own copy of the official Linux desktop backend beside the CLI with this exact name:

```text
codex-desktop-26.1013.3656.0-linux
SHA256: 70ea33b7094a4c20c007d09b83bb50e33debb9bbf99bf066709f9eb85bad524a
```

The companion is not redistributed here. Obtain it from your own official desktop installation's `app/resources/codex`; verify it is the Linux ELF executable and matches the hash. Do not substitute a stock CLI or bypass the hash check. Make the companion executable. A missing or mismatched companion disables predictions without affecting main inference.

Test the new CLI against an idle completed conversation using the same resume command and app-server endpoint as your existing setup. Confirm the gray `[Tab]` suggestion, press Tab, and check it fills the draft without submitting. Preserve unsent drafts and active work before switching an existing terminal. Never restart a shared app-server merely to update its TUI client.

Set `CODEX_CLI_PREDICTIONS=0` to disable the feature. Restoring the old launcher/executable is the full rollback; no session database migration is involved.

## Apply the standalone patch

The release provides `codex-cli-official-predictions-0.162.1.patch` and its SHA256 file. It contains the feature source, tests, documentation, and CI workflow, and applies to the upstream `rust-v0.162.1` baseline. It excludes the desktop companion and all local credentials/configuration.

```sh
sha256sum -c codex-cli-official-predictions-0.162.1.patch.sha256
# In a clean upstream checkout; use the actual downloaded patch path.
git switch -c local/official-predictions rust-v0.162.1
git apply --check /path/to/codex-cli-official-predictions-0.162.1.patch
git apply --index /path/to/codex-cli-official-predictions-0.162.1.patch
```

Then run the build and installation steps above. Applying a source patch is fast, but it still requires compilation. Keep the feature branch for ongoing upstream merges; use the standalone patch for reproducible application to the documented baseline. Do not apply it blindly to a different release.

## GitHub workflow

The **Official CLI predictions** workflow automatically runs the eight offline authentication/protocol boundary cases on branch pushes. They use mocks and send no real requests. Manual dispatch can additionally run Rust regressions and build a downloadable Linux CLI artifact with a SHA256 file. It needs no account token, API key, or desktop companion. The artifact does not contain a login or the proprietary companion.

The local 2026-10-16 verification passed all 61 tests selected by `test(next_prompt_prediction) | test(mcp_startup)`, including 15 prediction tests, plus the eight offline boundary cases. The first-MCP-status regression failed before the fix: replay could schedule a prediction before the first startup notification, which then cancelled it. The test covers both known and not-yet-populated expected-server state.

An earlier broader 79-test replay/MCP/reconnect run passed 77; two version-banner snapshots also failed identically on the unpatched upstream release source. They are `reconnect_allows_slow_hydration_but_bounds_a_stalled_server` and `reconnect_daemon_command_center_after_socket_replacement_without_a_conversation`. Their snapshots were not changed to hide those failures. This is not a claim that the complete upstream suite passed.

## Merge future upstream releases

Keep the fork's `main` as the upstream reference and use `feature/official-cli-predictions` for this feature. Do not force-push the shared feature branch. Make a temporary integration branch and merge an explicitly chosen upstream tag:

```sh
git remote add upstream https://github.com/openai/codex.git  # once
git fetch upstream --tags
git switch feature/official-cli-predictions
git switch -c merge-upstream-SELECTED_VERSION
git merge --no-commit rust-vSELECTED_VERSION
# Resolve conflicts, inspect the diff, and run the checks above.
git commit
git push -u origin HEAD
# Open a PR targeting feature/official-cli-predictions in this fork.
```

An unresolved merge must stay unresolved until reviewed; do not auto-select ours/theirs for whole files. Most implementation resides in `next_prompt_prediction.rs`, `app/next_prompt_prediction.rs`, `app/official_prediction.py`, and `chatwidget/next_prompt_prediction.rs`. Small hooks connect completion/replay, MCP startup, reconnection, composer cancellation, and Tab handling. These hooks are the main areas to review during upstream updates.

Before deploying an upstream merge, verify live completion, completed-history resume, MCP startup ordering, duplicate completion, draft preservation, cancellation, and Tab-without-submit. Verify auth isolation and no ordinary-inference fallback. Updating the official companion is a separate change: inspect the current official desktop protocol/config, update its name/hash only after validation, and retain the prior executable for rollback. No automatic upstream merge or companion update is enabled.

## Diagnose a missing suggestion

The target `codex_tui::app::next_prompt_prediction` logs the source thread/turn and trigger, without logging the conversation text or tokens:

- `Official CLI prediction queued`: eligible completion reached the app handler.
- `Official CLI prediction backend outcome`, `outcome="sampled_out"`: official rollout skipped inference.
- `outcome="completed"`: the helper returned successfully; the suggestion may be null.
- `Official ChatGPT composer prediction ready`: a current, valid suggestion is being applied.
- `Official CLI prediction cancelled`: typing/context changes cancelled work.
- `Official CLI prediction has no displayable suggestion`: empty, rejected, or stale output.
- `Official CLI prediction unavailable; no paid fallback`: authentication, companion, timeout, or protocol failure.

Inspect the TUI log destination configured by your own installation. Do not infer a failure merely from an empty composer, or success merely from unchanged quota percentages.
