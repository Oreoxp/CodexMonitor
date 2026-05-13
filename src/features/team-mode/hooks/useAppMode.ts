// useAppMode is now backed by AppModeContext so all callers share one
// state slot. The hook export stays at this path for backward compat with
// existing imports.
export { useAppMode, type AppMode } from "../context/AppModeContext";
