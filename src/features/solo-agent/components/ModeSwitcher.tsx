import Bot from "lucide-react/dist/esm/icons/bot";
import Code2 from "lucide-react/dist/esm/icons/code-2";
import type { AppMode } from "./types";

type ModeSwitcherProps = {
  mode: AppMode;
  onModeChange: (mode: AppMode) => void;
};

export function ModeSwitcher({ mode, onModeChange }: ModeSwitcherProps) {
  return (
    <div className="mode-switcher" role="tablist" aria-label="App mode">
      <button
        className={`mode-switcher-button${mode === "code" ? " is-active" : ""}`}
        type="button"
        role="tab"
        aria-selected={mode === "code"}
        onClick={() => onModeChange("code")}
      >
        <Code2 aria-hidden />
        <span>Code</span>
      </button>
      <button
        className={`mode-switcher-button${mode === "solo" ? " is-active" : ""}`}
        type="button"
        role="tab"
        aria-selected={mode === "solo"}
        onClick={() => onModeChange("solo")}
      >
        <Bot aria-hidden />
        <span>Solo</span>
      </button>
    </div>
  );
}
