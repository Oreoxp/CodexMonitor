import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";

export function AboutView() {
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    const fetchVersion = async () => {
      try {
        const value = await getVersion();
        if (active) {
          setVersion(value);
        }
      } catch {
        if (active) {
          setVersion(null);
        }
      }
    };

    void fetchVersion();
    return () => {
      active = false;
    };
  }, []);

  return (
    <div className="about">
      <div className="about-card">
        <div className="about-header">
          <img
            className="about-icon"
            src="/app-icon.png"
            alt="OpenCrab icon"
          />
          <div className="about-title">小螃蟹 (OpenCrab)</div>
        </div>
        <div className="about-version">
          {version ? `Version ${version}` : "Version —"}
        </div>
        <div className="about-tagline">
          Monitor the situation of your Codex agents
        </div>
      </div>
    </div>
  );
}
