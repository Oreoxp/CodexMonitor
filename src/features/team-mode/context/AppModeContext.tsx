import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";

export type AppMode = "normal" | "team";

const STORAGE_KEY = "opencrab.appMode";
const DEFAULT_MODE: AppMode = "normal";

type AppModeContextValue = {
  mode: AppMode;
  setMode: (next: AppMode) => void;
};

const AppModeContext = createContext<AppModeContextValue | null>(null);

function readStored(): AppMode {
  if (typeof window === "undefined") return DEFAULT_MODE;
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    return raw === "team" || raw === "normal" ? raw : DEFAULT_MODE;
  } catch {
    return DEFAULT_MODE;
  }
}

export function AppModeProvider({ children }: { children: ReactNode }) {
  const [mode, setModeState] = useState<AppMode>(readStored);

  useEffect(() => {
    try {
      window.localStorage.setItem(STORAGE_KEY, mode);
    } catch {
      // localStorage unavailable (private mode, etc) — silent.
    }
  }, [mode]);

  const setMode = useCallback((next: AppMode) => {
    setModeState(next);
  }, []);

  const value = useMemo<AppModeContextValue>(() => ({ mode, setMode }), [mode, setMode]);

  return <AppModeContext.Provider value={value}>{children}</AppModeContext.Provider>;
}

export function useAppMode(): [AppMode, (next: AppMode) => void] {
  const ctx = useContext(AppModeContext);
  if (ctx === null) {
    throw new Error("useAppMode must be used within <AppModeProvider>");
  }
  return [ctx.mode, ctx.setMode];
}
