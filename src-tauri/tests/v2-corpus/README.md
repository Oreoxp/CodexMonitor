# V2 Memory Test Corpus

A versionable, replayable, ground-truth-annotated corpus of **synthetic codex
rollouts** for end-to-end validation of the V2 memory pipeline:

```
S2 ingest  ->  S3 distill  ->  S4 search  ->  S5 consolidate
```

Each case is one directory holding a synthetic `rollout-<iso>-<uuid>.jsonl`
(real codex rollout shape) plus a machine-verifiable `ground_truth.yaml`.

> **Phase 1 (this batch): 14 cases, C001–C014.** Scope = the S2/S3 core
> positive + reverse cases, one S2 compaction/boundary case, and one S5
> contradiction pair, enough to close the loop before scaling to 50–80.
> See [CHANGELOG.md](CHANGELOG.md) for what's deferred to Phases 2/3.

The source of truth for the rollout schema + classifier semantics is
[`_anchors/anchors.md`](_anchors/anchors.md) (a self-contained packaging of
`backend.rs`'s `parse_line` / `segment_thread` / `EXTRACTION_PROMPT`). Read it
before adding cases.

---

## Layout

```
v2-corpus/
├── README.md                # this file
├── matrix.yaml              # dimension -> covered cases
├── CHANGELOG.md             # per-phase delta
├── _anchors/anchors.md      # technical handoff (parse_line, prompt, samples)
├── _tools/
│   ├── corpus_tool.py       # generator + offline verifier (parse_line port)
│   └── README.md            # tool contract + how to add a case
└── cases/
    ├── C001-pg-session-store/
    │   ├── rollout-2026-05-10T09-00-00-0000c001-…-00000000c001.jsonl
    │   └── ground_truth.yaml
    └── … C014
```

## Case index

| Case | Title | KPs | Lang | Key tags |
|---|---|---|---|---|
| C001 | session store → **Postgres** (canonical / §7 sanity) | 1 decision | zh+en | tool-pairs, cross-topic, **contradicts C002** |
| C002 | session store → **Redis** | 1 decision | zh+en | **contradicts C001** |
| C003 | flaky tests from shared `/tmp` path | 1 failure | zh+en | noisy-tool-output, tool-pairs |
| C004 | CORS preflight strips Authorization | 1 failure | **en** | pure-en, tool-pairs |
| C005 | adding a Tauri command (codebase idiom) | 1 pattern | zh+en | tool-pairs |
| C006 | `/healthz`=readiness, `/livez`=liveness (+emoji) | 1 fact | mixed | utf8-multibyte |
| C007 | DB DSN comes from `ACME_DB_URL` (buried in dump) | 1 fact | zh+en | noisy-tool-output |
| C008 | greeting-only (`thanks!` / `glad it helped`) | **0** | **en** | reverse: No-Echo |
| C009 | rote bulk-rename, no lesson | **0** | zh+en | reverse: Skip-trivial |
| C010 | chitchat / contentless help request | **0** | **zh** | reverse: pure-zh |
| C011 | aborted mid-exploration (no conclusion) | **0** | **zh** | reverse: aborted, pure-zh |
| C012 | compaction boundary + prior-summary (backoff decision) | seg1=0, seg2=1 | zh+en | **multi-segment**, with-prior-summary |
| C013 | all-Dropped file (no Kept line) | **0** | — | all-dropped |
| C014 | day.js over moment (deprecated/bundle) | 1 decision | zh+en | cross-topic target of C001 |

## Harness contract — how to run a case through the pipeline

The case directory is **storage only**. Attribution
(`parse_rollout_attribution`, anchors §0.2) derives `thread_id`/`team_id`/
`project_hash` from the **path** and `agent_id` from a CLI arg — so before
ingest the runner must copy each `rollout-*.jsonl` into a scan tree:

```
<tmp>/team_sessions/<team_id>/<project_hash>/rollout-<iso>-<uuid>.jsonl
   └──────── scan_root ───────┘ └ from yaml ┘ └ from yaml ┘ └── as-is ──┘
```

Hard gates (miss any → the file is **silently skipped**):
- a literal `team_sessions` path component, with `<team_id>` and
  `<project_hash>` as the next two components, and the file exactly one level
  below that (no deeper);
- the filename's **last 36 chars** are a hex UUID (8-4-4-4-12) — already true
  for every corpus file; the `rollout-<iso>-` prefix is ignored;
- ingest with `--agent-id <agent_id from yaml>` (the path's `agents/<id>/`
  prefix is **not** parsed — the arg is authoritative).

All Phase-1 cases share `agent_id: test-agent-001`, `team_id: team-fixtures`,
`project_hash: hash-fixtures` so their distilled KPs land in **one** `log`
scope — required for the S4 cross-case search and the C001/C002 S5
contradiction to be seen together. Each case is its own thread (distinct UUID).
`parent_thread_id` is always NULL (anchors §0.7) — never assert on it.

Sanity loop for the first case (anchors §7): place C001, run the `s3val`-style
ignored test pointed at the temp `scan_root`, inspect the distilled rows, and
tune C001's semantic assertions to the live output before scaling.

## `ground_truth.yaml` schema

```yaml
case_id, title, description        # human metadata; description notes the "point" of the case
tags: [...]                        # coverage dims (aggregated in matrix.yaml)
multilingual: zh_only|en_only|zh_majority|en_majority|mixed
thread_count, segment_count
agent_id, team_id, project_hash    # attribution the harness must apply (see contract above)
reverse_rule: "..."                # (reverse cases only) which prompt rule should fire []

# --- S2: machine-derived, do NOT hand-edit (regenerated by `verify --fix`) ---
expected_ingest:                   # lives between the two sentinel comment lines
  raw_event_count: N               # == physical lines in the .jsonl
  kept_line_nos:     [...]         # 1-based; parse_line -> Kept
  dropped_line_nos:  [...]         # parse_line -> Dropped
  boundary_line_nos: [...]         # top-level type:"compacted"
                                   # the three lists partition [1..raw_event_count]

# --- S3: single-segment cases ---
expected_distill:
  expected_kp_count: N             # 0 for reverse; best-estimate for positives (tune live)
  knowledge_points:
    - kind: decision|failure|pattern|fact
      summary_must_contain_any: [...]      # OR
      summary_must_not_contain: [...]      # anti-hallucination
      detail_must_mention_all:             # AND of ORs — each inner list is OR
        - [...]
        - [...]
      detail_must_not_mention: [...]

# --- S3: multi-segment cases (e.g. C012) ---
expected_distill_segments:         # one entry per segment_thread() output, in order
  - {segment_index, expected_kp_count, knowledge_points, prior_summary_note?}

# --- S4 (optional, per-case) ---
expected_search:
  - query, limit, expected_top1_summary_must_match_any | expected_top1_summary_must_not_match

# --- S5 (optional, per-case) ---
consolidation_relations:
  - with_case: Cxxx
    expected_action: duplicate|complement|contradiction|supersede|no_action
    reason: "..."
```

Matching is **substring/set containment** — never LLM-judged — so the
verification stays deterministic. The `expected_ingest` block is fully
offline-verifiable today (`corpus_tool.py verify`); the S3/S4/S5 assertions
need a live LLM/embedder and are checked by the runner harness. Per spec §7,
positive-case S3 assertions are intentionally broad (OR sets) and meant to be
tuned against live distiller output — keep them broad enough that a paraphrase
still passes, tight enough to catch a wrong/hallucinated KP.

## Running the tool

```bash
cd _tools
python3 corpus_tool.py gen            # (re)write every rollout jsonl + self-check vs parse_line
python3 corpus_tool.py verify         # check each yaml's expected_ingest against its jsonl
python3 corpus_tool.py verify --fix   # rewrite the expected_ingest region from jsonl truth
python3 corpus_tool.py transcript C012  # dump the rendered transcript + segments the LLM sees
```

**Ownership:** `rollout-*.jsonl` is generated from the case spec in
`corpus_tool.py` — edit the spec, not the jsonl. `ground_truth.yaml` is
hand-authored; the tool only ever rewrites the single `expected_ingest` region
between its sentinel comments, leaving all semantic blocks untouched. Both are
committed artifacts. stdlib only — no third-party deps.
