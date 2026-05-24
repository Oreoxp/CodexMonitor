// P6 Step 9 — Prompt assembly bridge for opencrab-eval.
//
// Run with `npx tsx` from the sidecar directory so module resolution sees
// sidecar/node_modules. The eval bin (Rust) writes this file to a tempdir,
// then spawns `npx tsx <tempfile>` with cwd=<sidecar root>.
//
// Stdout (single line, no trailing newline) = JSON
//   { developerInstructions: string,    // post-finalize (cache sentinel stripped)
//     kickoffPrompt:         string }   // composeFirstUserMessage output
//
// MUST mirror the real provisioning path bytewise:
//   composeDeveloperInstructions(team, agent, loader)
//     -> finalizeSystemPromptForCodex(...)    // strip cache sentinel
//   composeFirstUserMessage(agent.name, /* daily memory prelude */ "")
//
// The bridge deliberately uses the test seam composeDeveloperInstructionsForTests
// because composeDeveloperInstructions is module-private. The test seam is a
// thin re-export — same bytes.
//
// P6 Step 18 — `--tools-mode` post-processes the production developer
// instructions to swap the text-tag comm guide for a tool-call variant
// (matching the opencrab-team-mcp stub's two-tool surface). Only the
// `## Sending a message` and `## Proposing a plan for approval` sections
// change; opening intro and `## Receiving a message` stay aligned with
// production.

import { readFileSync } from "node:fs";
import { argv, stderr, stdout, exit } from "node:process";

function getArg(name: string): string {
  const idx = argv.findIndex((a) => a === name);
  if (idx < 0 || idx + 1 >= argv.length) {
    stderr.write(`compose.mts: missing required arg ${name}\n`);
    exit(2);
  }
  return argv[idx + 1]!;
}

const sidecarRoot = getArg("--sidecar-root");
const userDataDir = getArg("--user-data");
const projectDataDir = getArg("--project-data");
const agentId = getArg("--agent-id");
const toolsMode = argv.includes("--tools-mode");

// Dynamic imports — absolute paths resolved at runtime via tsx. Pointing
// at .ts source means tsx transpiles on the fly; we don't need a build step.
const teamMod = await import(`${sidecarRoot}/src/team/types.ts`);
const stateMod = await import(`${sidecarRoot}/src/runtime/state.ts`);
const promptMod = await import(`${sidecarRoot}/src/prompt/index.ts`);
const loaderMod = await import(`${sidecarRoot}/src/workspace/file-loader.ts`);
const aclMod = await import(`${sidecarRoot}/src/team/acl.ts`);

// Load team.json from the user layer (where bootstrap migrated it).
const teamPath = `${userDataDir}/team.json`;
const raw = readFileSync(teamPath, "utf-8");
const team = teamMod.parseTeamConfig(JSON.parse(raw));

const agent = team.agents.find((a: { id: string }) => a.id === agentId);
if (!agent) {
  stderr.write(`compose.mts: agent ${agentId} not in team.json\n`);
  exit(3);
}

const loader = new loaderMod.WorkspaceFileLoader(userDataDir, projectDataDir);

// Production path: internal compose returns the boundary-bearing string;
// finalize strips the sentinel on the way to Codex. Bytewise identical to
// what `provisionAndStartRouter` sends to thread/start.
const internal = stateMod.composeDeveloperInstructionsForTests(team, agent, loader);
let onWire = promptMod.finalizeSystemPromptForCodex(internal);

if (toolsMode) {
  // Spike: replace the production comm guide chunk with the tool-call
  // variant. The production bytes are computed by calling composeCommGuide
  // directly with the same args — that string appears verbatim once
  // inside `onWire` (separated by `\n\n` boundaries).
  const prodGuide = stateMod.composeCommGuide(team, agent.id);
  const spikeGuide = buildToolModeCommGuide(team, agent.id);
  if (!onWire.includes(prodGuide)) {
    stderr.write(
      "compose.mts: --tools-mode could not find the production comm guide " +
        "inside developer instructions (anchor drift?)\n",
    );
    exit(4);
  }
  onWire = onWire.replace(prodGuide, spikeGuide);
  // Step 19 fix: ROLE.md (PM charter, in particular) STILL references the
  // `<propose_plan>` / `<send_message>` text tags after step 18's
  // comm-guide-only swap. The model honored the more detailed PM charter
  // and emitted a (broken) text tag instead of calling the MCP tool. Now
  // ALSO sweep the workspace-file blocks so every text-tag reference is
  // rewritten as the corresponding tool-call wording — no more conflicting
  // instructions inside one prompt.
  // The "X block" / "X tag" patterns must tolerate the source text's line
  // wrapping (PM ROLE.md wraps "`<propose_plan>`\nblock" across lines —
  // a literal-space replaceAll missed it in the previous take, and the
  // fallback collapsed `<propose_plan>` into `\`propose_plan\` tool` with
  // ugly nested backticks).
  onWire = onWire
    .replace(/`<propose_plan>`\s+block/g, "`propose_plan` tool call")
    .replace(/`<send_message>`\s+tag/g, "`send_message` tool call")
    // Any remaining text-tag forms (e.g. naked `<send_message>` mentions
    // in prose) — collapse the angle brackets so the reference reads as a
    // tool name. The two-step (with-backticks then without) handles both
    // ``<X>`` and bare `<X>` shapes without producing nested backticks.
    .replace(/`<(propose_plan|send_message)>`/g, "`$1`")
    .replace(/<(propose_plan|send_message)>/g, "`$1`");
}

// First user message = kickoff (with the daily-memory prelude empty — eval
// fixtures don't seed project-memory, so the production path also yields
// the kickoff verbatim here).
const kickoff = stateMod.composeFirstUserMessage(agent.name, "");

stdout.write(JSON.stringify({ developerInstructions: onWire, kickoffPrompt: kickoff }));

// -- Spike comm guide ----------------------------------------------------
//
// Step 18 tool-mode variant. Same shape as the production composeCommGuide
// (heading + 3 sections + dynamic targets line). The two sections that
// differ (`## Sending a message`, `## Proposing a plan for approval`) tell
// the agent to call the MCP tools instead of emitting text tags.
function buildToolModeCommGuide(t: unknown, aid: string): string {
  const targets = aclMod.publishableTargets(t, aid);
  const targetsLine =
    targets.length === 0
      ? "You currently have no recipients you can send messages to."
      : "You can send messages to: " +
        targets
          .map(
            (x: { to: string; channels: string[] }) =>
              `${x.to} (channel: ${x.channels.join(", ")})`,
          )
          .join("; ");
  return [
    "# Team communication",
    "",
    "You are one agent in a multi-agent team. The user speaks only with the PM; every other",
    "agent reaches the user — and reaches each other — only through the tools described here.",
    "This section is the team's wiring: it is how work actually moves between agents.",
    "",
    "## Sending a message",
    "Use the `send_message` tool to send a message to another agent. It is the only way your",
    "message reaches anyone — prose in your reply alone is not delivered. Required arguments:",
    "  • `to` — recipient agent id, from the list below",
    "  • `channel` — the subscription channel for that recipient (typically `chat`)",
    "  • `body` — the message text",
    "The recipient sees your message wrapped as `[From <you>]` at the start of their next",
    "incoming turn.",
    "",
    targetsLine,
    "",
    "## Receiving a message",
    "Messages from others arrive wrapped as `[From <sender>]` at the start of the incoming",
    "text. `[From system]` marks an automated notice from OpenCrab itself — for example, the",
    "result of a plan approval — not a person. Treat everything inside such a wrapper as",
    "information to act on: it is a report, not a script to answer line by line.",
    "",
    "The `[From <sender>]` wrapper is something you receive, never something you write — it",
    "is the inbound format only. To send, use the `send_message` tool.",
    "",
    "## Proposing a plan for approval",
    "Use the `propose_plan` tool to put a plan to the user for sign-off before any work",
    "begins. It takes a `tasks` array; each task has:",
    "  • `title` — short imperative (e.g. 'Install django-auditlog')",
    "  • `assignee` — agent id to assign the task to (optional; omit if undecided)",
    "  • `body` — longer description of what the task involves",
    "The user reviews and approves, edits, or rejects each task; results arrive in your",
    "thread as `[From system]` messages. Do not start or delegate work on a task until its",
    "approval result has arrived. A plan written only as prose or a Markdown list is not a",
    "plan the system can act on — it must be a `propose_plan` tool call.",
  ].join("\n");
}
