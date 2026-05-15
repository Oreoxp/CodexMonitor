// Sidecar session module — Phase 1 Spike A.
//
// Pairs with the Node-side sidecar at `<repo>/sidecar/`. Each workspace gets
// its own `npx tsx src/main.ts` child process; the manager keeps them in a
// HashMap keyed by workspace_id.

pub(crate) mod commands;
pub(crate) mod inbound_ops;
pub(crate) mod manager;
pub(crate) mod session;
pub(crate) mod status;
pub(crate) mod team_router;

pub(crate) use manager::SidecarSessionManager;
