# CodexMonitor — Frontend: UI & Workspace for 小螃蟹

## Role

This directory is the **Face & Workspace** of OpenCrab3. It is a **Tauri 2 + React 19** desktop application that provides the entire user-facing experience for interacting with the `codex-cli` backend (app-server).

- Communicates with `codex-app-server` via **WebSocket** (`ws://`) or **stdio** transport.
- Manages workspace isolation, agent sessions, diff review, and all visual tool-call rendering.
- Bundles as a native macOS/Windows desktop app via Tauri.

---

## Core Stack

| Layer | Technology | Notes |
|---|---|---|
| Framework | **React 19** | Functional components, hooks throughout |
| Language | **TypeScript 5.8** | Strict mode; `tsc --noEmit` for typecheck |
| Build | **Vite 7** | Dev server on `localhost:1420` |
| Native Shell | **Tauri 2** | `@tauri-apps/api` v2; transparent window, overlay titlebar |
| Styling | **CSS Modules + Design Token CSS** | No Tailwind; uses `ds-tokens.css` design system tokens (`--ds-*` vars) |
| State | **React hooks** (custom, feature-scoped) | State lives in `useThreads`, `useWorkspaceController`, etc. — no global store library |
| Terminal | **xterm.js** (`@xterm/xterm` v5) | Embedded PTY terminal view |
| Diff Rendering | **`@pierre/diffs`** | Used in strict Diff Approval panels (`ds-diff.css`) |
| Virtualization | **`@tanstack/react-virtual`** | Long message thread lists |
| Icons | **`lucide-react`** + `vscode-material-icons` | Synced via `scripts/sync-material-icons.mjs` |
| Markdown | **`react-markdown`** + `remark-gfm` | Agent message rendering |
| Testing | **Vitest** + `@testing-library/react` | `npm test` / `npm run test:watch` |

---

## Feature Architecture

Code is organized under `src/features/` by domain:

- `app/` — Root orchestration (`MainApp.tsx`, layout shell, modal controllers)
- `workspaces/` — Workspace and worktree lifecycle
- `git/` — Diff, PR composer, branch switching
- `composer/` — User input, shortcuts, editor state
- `threads/` — Agent conversation thread management
- `terminal/` — Embedded xterm terminal
- `collaboration/` — Collaboration mode selection
- `notifications/` — Toast notifications, error toasts, approval toasts
- `mobile/` — Mobile/remote workspace support

---

## Development Rules

### 1. Visual Cleanliness

> **RULE: The UI must maintain extreme visual cleanliness at all times.**

- Tool calls MUST be rendered as **collapsible components** — never dump raw tool output inline into the message thread.
- Approval flows (file writes, shell commands) use **toast-based approval UI** (`approval-toasts.css`), not modal interruptions wherever possible.
- Diff views MUST use the dedicated **Diff Approval panel** (`ds-diff.css`, `diff-viewer.css`). Do not render diffs as plain text.
- Use existing `--ds-*` CSS design tokens. Do not introduce ad-hoc colors or spacing values.

### 2. No Auth / Token Logic

> **RULE: Zero API token or authentication logic belongs in this layer.**

- All auth, API key management, and cost control is handled exclusively by `codex-cli` (the backend).
- `CodexMonitor` does not store, read, transmit, or display raw API tokens.
- If a feature requires auth state, it must be received from the backend via the app-server protocol — not computed or stored here.

### 3. Protocol Consumption

> **RULE: `CodexMonitor` is a consumer of the app-server protocol, never its source of truth.**

- Message types and WebSocket schemas are defined in `codex-cli/codex-rs/app-server-protocol/`.
- If a protocol change is needed, update the backend first, then update the TypeScript types here to match.

### 4. Commands

```bash
# Development (runs doctor check first)
npm run tauri:dev

# Type check only
npm run typecheck

# Lint
npm run lint

# Tests
npm test

# Production build
npm run tauri:build
```

---

关于本项目已实现的功能细节以及未来的开发缺口，请随时查阅同目录下的 `FEATURES.md`。
