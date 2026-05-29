# Corpus CHANGELOG

## Phase 1 — 14 cases (C001–C014)

First batch: close the end-to-end loop (directory layout, ground-truth field
machine-verifiability, `parse_line` compatibility) before scaling to 50–80.

### Added cases

| Case | kind / role | Covers |
|---|---|---|
| C001 | decision (Postgres) | canonical §7 sanity case; tool-pair; cross-topic search; contradiction w/ C002 |
| C002 | decision (Redis) | contradiction w/ C001 (same question, opposite answer) |
| C003 | failure | flaky test, noisy-tool-output, tool-pair |
| C004 | failure | pure-en, CORS preflight |
| C005 | pattern | codebase idiom (Tauri command registration) |
| C006 | fact | utf8-multibyte (中文 + emoji), readiness/liveness |
| C007 | fact | noisy-tool-output (config dump), env-var DSN |
| C008 | reverse → [] | greeting-only (No-Echo; mirrors prompt Example 3) |
| C009 | reverse → [] | task-complete-no-learning (Skip-trivial) |
| C010 | reverse → [] | chitchat, pure-zh |
| C011 | reverse → [] | aborted/no-conclusion, pure-zh |
| C012 | decision (post-boundary) | **multi-segment-compaction + with-prior-summary**; seg1=0 KP, seg2=1 KP; asserts prior_summary is not mined |
| C013 | — → [] | all-dropped (no Kept line → empty transcript → no LLM call) |
| C014 | decision (day.js) | cross-topic search target for C001 |

### Coverage dimensions hit (see matrix.yaml)

- **S2:** single-thread, multi-segment-compaction, all-dropped, mixed-keep-drop,
  tool-call-output-pairs, utf8-multibyte.
- **S3 positive:** kind-decision ×4, kind-failure ×2, kind-pattern ×1, kind-fact ×2.
- **S3 reverse:** greeting-only, task-complete-no-learning, chitchat, aborted (4 — the rule-tripping table from anchors §3.1).
- **S3 boundary/lang:** pure-zh ×2, pure-en ×2, mixed-zh-en ×9, with-prior-summary, noisy-tool-output ×2.
- **S4:** cross-topic-false-neighbor (C001 carries a negative query).
- **S5:** contradiction (C001↔C002).

### Verification status

- **S2 mechanical (offline, verified):** `corpus_tool.py verify` → all 14 PASS.
  Every line's classification self-checks against the `parse_line` port;
  `expected_ingest` derived from jsonl truth; physical-line-count == raw_event_count
  for all files; all jsonl lines valid single-line JSON with trailing newline;
  all 14 `ground_truth.yaml` parse as valid YAML.
- **S3/S4/S5 semantic (live):** authored as best-estimate broad assertions;
  to be tuned against the configured LLM/embedder by the runner harness per
  spec §7. `expected_kp_count` for positives is the most-likely single count;
  harness may treat it as a lower bound.

### Known gaps / deferred

- **Distribution:** Phase 1 skews zh (zh_only+zh_majority = 11/14) to front-load
  core coverage. Phase 2 rebalances toward the 60/25/15 zh/mixed/en target and
  the 30/30/15/15/10 decision/failure/pattern/fact/reverse kind split.
- **Deferred to Phase 2:** S5 duplicate / complement / supersede / S5 cross-topic
  pairs (≥4–5 each); meta-only reverse; more pure-zh / en cases.
- **Deferred to Phase 3:** long-thread / long-truncation (>48k-char segment),
  resume (multi-file, distinct UUIDs, created_at_offset), adversarial near-miss
  pairs, S4 lexical-only / semantic-only / RRF order cases, and ≥3 end-to-end
  composite workflows.
- **ToolMisc variants** (local_shell_call / web_search_call / …): 0/108 in real
  rollouts (anchors §5.1) — synthesize-only, edge-tier, not yet exercised.
