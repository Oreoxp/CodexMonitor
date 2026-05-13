import { useAppMode, type AppMode } from "../hooks/useAppMode";

const TABS: { value: AppMode; label: string }[] = [
  { value: "normal", label: "Normal" },
  { value: "team", label: "Team" },
];

export function ModeToggle() {
  const [mode, setMode] = useAppMode();
  return (
    <div className="mode-toggle-tabs" role="tablist" aria-label="App mode">
      {TABS.map((tab) => {
        const active = tab.value === mode;
        return (
          <button
            key={tab.value}
            type="button"
            role="tab"
            aria-selected={active}
            data-active={active}
            className="mode-toggle-tab"
            onClick={active ? undefined : () => setMode(tab.value)}
          >
            {tab.label}
          </button>
        );
      })}
    </div>
  );
}
