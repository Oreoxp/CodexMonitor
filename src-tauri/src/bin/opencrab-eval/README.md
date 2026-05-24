# opencrab-eval

Behavioural eval harness for OpenCrab Team Mode prompts.

P6 Step 9 deliverable. Drives the real OpenCrab agent-startup stack
(prompt assembly → codex-app-server → ws thread/start) against the
configured model provider and runs structural assertions on the agent's
output. **Not hermetic** — calls a real LLM, costs tokens.

## What it tests

Three single-agent fixtures, each pinning one behavioural concern:

| id | scenario | assertion |
|---|---|---|
| `01-pm-clear` | PM gets a clear, scoped request | Output contains a `<propose_plan>` block with at least one `<task>` child. Phase 3 regression guard (PM emits XML not Markdown). |
| `02-pm-ambiguous` | PM gets a vague one-line request | PM asks clarifying questions (`?` or "could you" / "to confirm" / ...) before proposing. Either clarify-without-plan or clarify-alongside-plan is acceptable per the PM charter. |
| `03-qa-verification` | QA gets a verification ask | QA produces finding-language without invoking any code-modifying tool call (`apply_patch` / `edit_file` / `write_file`). |

## How it runs

```
opencrab-eval                                # runs all 3 fixtures
```

Per fixture:

```
1. mkdir tempdir/home/.opencrab/
2. write team.json + config.toml (Qwen provider, bearer from env)
3. bootstrap::ensure_user_layer_at(...)      ← prod path; seeds SOUL / IDENTITY /
                                               ROLE / USER / MEMORY into the tempdir
4. bootstrap::ensure_project_layer_at(...)   ← seeds KANBAN / project-memory / …
5. compose.mts                               ← real composeDeveloperInstructions
                                               + finalizeSystemPromptForCodex
6. spawn codex-app-server with HOME=tempdir
7. ws://127.0.0.1:<random> initialize → thread/start with prod developer_instructions
8. sendUserMessage(kickoff) → drain to turn/completed
9. sendUserMessage(fixture.user_turn) → drain to turn/completed
10. extract final text + run assertion
11. tear down: kill codex-app-server, remove tempdir
```

## Required env vars

| Var | What | Default |
|---|---|---|
| `OPENCRAB_EVAL_BEARER` | DashScope (Qwen) API token. **Do not commit.** | unset → harness skips |
| `OPENCRAB_EVAL_CODEX_APP_SERVER` | Absolute path to the built `codex-app-server` binary. | unset → harness skips |

Optional:

| Var | What | Default |
|---|---|---|
| `OPENCRAB_EVAL_SIDECAR_ROOT` | Absolute path to the `sidecar/` source root. | auto-detected by walking up from the running binary |
| `OPENCRAB_EVAL_VERBOSE` | `RUST_LOG`-style value forwarded to codex-app-server children. | `warn` |

If either of the two required vars is unset, every fixture prints `SKIPPED`
and the process exits 0 — safe to leave in `cargo build` output and CI matrices.

## Setup

```bash
# 1. Build codex-app-server (one-time per code change).
cd codex-cli/codex-rs
cargo build -p codex-app-server --bin codex-app-server

# 2. Build the eval binary.
cd CodexMonitor/src-tauri
cargo build --bin opencrab-eval

# 3. Make sure sidecar's node_modules is populated (one-time).
cd ../../sidecar
npm install

# 4. Run.
export OPENCRAB_EVAL_BEARER="sk-..."     # your DashScope token
export OPENCRAB_EVAL_CODEX_APP_SERVER="$(pwd)/../codex-cli/codex-rs/target/debug/codex-app-server"
cd ../CodexMonitor/src-tauri
./target/debug/opencrab-eval
```

## Output

Per fixture: human-readable transcript (user turn → agent final text →
verdict + notes). A final summary line counts passed / failed / errored.
Exit code is 0 only when every fixture passed.

## Wrinkle handling (from step 8 §E.4)

1. **bearer secret** — `$OPENCRAB_EVAL_BEARER`. Never written to disk except into the per-fixture tempdir `config.toml`, which is removed at teardown.
2. **codex-app-server fixture-aware startup** — each fixture spawns its own codex-app-server child with `HOME` + `CODEX_HOME` pointing at its tempdir; the child is killed and the tempdir removed at teardown.
3. **per-fixture codex-app-server instance** — required because codex-rs freezes `CODEX_HOME` at startup (`find_codex_home()`). Cost: ~2-3s startup per fixture; for 3 fixtures the overhead is well under a minute.
4. **Rust ↔ Node prompt assembly** — `compose.mts` (embedded via `include_str!`, written to the tempdir per run) runs under `npx tsx` from the sidecar source root. Module resolution finds sidecar's `node_modules`; dynamic `import()` against the absolute sidecar source paths means no compile step.

## Non-goals (deferred per step 8 §E.5)

- Hermetic / mock-model runs. The codex-cli workspace has `tests/common/mock_model_server.rs` if a hermetic CI lane is ever wanted; not wired here.
- Multi-agent fixtures (PM → Dev delegation, PM ↔ QA handoff). The 4th fixture per step 9.
- Token-consumption / cache-hit assertions. Those belong in prompt-strategy tests, not behavioural eval.
