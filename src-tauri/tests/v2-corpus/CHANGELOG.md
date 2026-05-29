# Corpus CHANGELOG

## Phase 2 — 26 new cases (C015–C040), → 40 total

Main goal: S5 (consolidation) relation-pair material, plus ingest-boundary +
C011 hardening + v8 (summary-FTS) material. Built on the committed v7 harness
(bge-m3@1024, trigram log_fts, kind-advisory).

### New cases by S5 relation class (12 pairs)
- **contradiction** (co-temporal, same-scope present decision, NO switch verbs):
  C001↔C002 (session PG/Redis, existing), **C015↔C016** (auth JWT/session),
  **C017↔C018** (deploy blue-green/rolling).
- **supersede** (switch verbs + `created_at_offset_hours` gap; older.superseded_by newer):
  **C019→C020** (MQ RabbitMQ→Kafka, −72h/0), **C021→C022** (Redux→Zustand, −48h/0),
  **C023→C024** (REST→gRPC, −96h/0).
- **false_neighbor** (vector-near, no_action — the S5 precision gate; each pair
  carries a per-pair "why noise" reason): **C025↔C026** (PG pool vs PG FTS),
  **C027↔C028** (Redis cache vs lock), **C029↔C030** (Kafka consumer-offset vs
  broker-disk — orthogonal app/infra layers; replaced the weaker 限流/熔断 idea
  per review, which leaned complement).
- **duplicate** (near-synonym → merge): **C031↔C032** (retry jitter), **C033↔C034** (stdout logging).
- **complement** (same topic, both survive → cluster): **C035↔C036** (PG store
  decision + **PG role-security pattern** — C036 made a durable security
  convention-with-rationale, not config trivia, so No-Echo won't drop it),
  **C037↔C038** (Kafka bus decision + topic-naming pattern).

### Other
- **C011 hardened** — assistant now gives zero durable advice (only acknowledges +
  asks to resume), so the aborted thread has nothing to (wrongly) mine → stable `[]`.
- **C039 long-truncation** — rendered transcript ~78k chars (> 48k cap); HEAD
  GraphQL decoy at offset 143 (dropped), TAIL shadow-table KP at ~77.9k (kept).
  Layer B asserts tail KP present + `GraphQL` forbidden = keep-tail/drop-head proof.
- **C040 summary-fts (v8 material)** — "限流阈值按 P99" lesson; the Chinese
  discriminator lands in **summary**. v7: passes Layer C via vector; FTS-only = 0
  (the documented v8 gap). **Caveat:** its v8 value depends on the LLM placing
  "限流阈值" in the summary (temp-0.2 jitter) — a v8 FTS-recall test MUST first
  verify the summary actually contains the substring, then assert recall (a
  defensive/conditional assertion, like kind-advisory & C011-jitter).

### Harness changes (corpus_tests.rs)
- `ConsolidationRelation` + `relation` field ({duplicate|supersede|contradiction|
  complement|false_neighbor}) — action alone can't encode S5's verdict.
- `GroundTruth` + `group` (scenario co-location), `created_at_offset_hours`,
  `expected_distill_case_level_forbidden` (case-level anti-hallucination).
- Layer C **refactored to per-group DBs** (one DB per `group`) — a 40-case
  mega-DB would make top-K noisy; S5 is tested on controlled neighborhoods. The
  existing C001 queries (membership PG+Redis; day.js cross-topic) now run in the
  `session` group DB. Layer C also measures **bge-m3 KP-distance per relation
  pair** → the distribution S5-design uses for candidate threshold T.

### torn-line — SKIPPED
Already covered by backend unit tests `s2if_a1` / `s2if_a2` (two-pass write→ingest);
a static corpus file always has a trailing `\n` so it can't exercise it.

### Verification (live Layer B/C results + accidental-neighbor audit + distance
### distribution are in the Phase-2 2b report)
- Layer A + full schema suite: **145 passed / 0 failed** (incl all 40 cases).
- `corpus_tool.py verify`: 40/40 `expected_ingest` PASS; self-check (parse_line) clean.

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
