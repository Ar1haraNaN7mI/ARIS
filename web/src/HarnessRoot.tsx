import { useCallback, useEffect, useRef, useState } from "react";
import { App } from "./App";
import { AccessEntry } from "./AccessEntry";
import { accessStatus, bootstrap, errorText, isAbort } from "./api";
import { useInterfaceEffects } from "./uiPreferences";
import { acceptVisualProfile, useVisualStyleEffects } from "./visualStyle";
import type { AccessStatus, Bootstrap } from "./types";
import "./access.css";

/** Open the workbench directly after access verification; no boot animation. */
export function HarnessRoot() {
  useInterfaceEffects();
  useVisualStyleEffects();
  const [access, setAccess] = useState<AccessStatus>();
  const [boot, setBoot] = useState<Bootstrap>();
  const [error, setError] = useState("");
  const [checking, setChecking] = useState(false);
  const active = useRef<AbortController | undefined>(undefined);
  const connect = useCallback(async () => {
    active.current?.abort();
    const abort = new AbortController();
    active.current = abort;
    setChecking(true); setError("");
    try {
      const status = await accessStatus(abort.signal);
      if (abort.signal.aborted) return;
      acceptVisualProfile(status.profile); setAccess(status);
      if (!status.unlocked) { setBoot(undefined); return; }
      const data = await bootstrap(abort.signal);
      if (!abort.signal.aborted) { acceptVisualProfile(data.profile); setBoot(data); }
    } catch (failure) { if (!abort.signal.aborted && !isAbort(failure)) setError(errorText(failure)); }
    finally { if (!abort.signal.aborted) setChecking(false); }
  }, []);
  useEffect(() => {
    void connect();
    const locked = () => { setBoot(undefined); void connect(); };
    window.addEventListener("aris-access-locked", locked);
    return () => { active.current?.abort(); window.removeEventListener("aris-access-locked", locked); };
  }, [connect]);
  useEffect(() => {
    if (!access || access.unlocked) return;
    const desktop = window as Window & { __ARIS_DESKTOP__?: boolean; ipc?: { postMessage?: (message: string) => void } };
    if (desktop.__ARIS_DESKTOP__) desktop.ipc?.postMessage?.(JSON.stringify({ event: "aris-ready", version: 1 }));
  }, [access]);
  return access && !access.unlocked ? <AccessEntry status={access} onUnlocked={connect} connectionError={error} checking={checking} onRetry={connect} />
    : boot ? <App initialData={boot} latestProfile={access?.profile} />
      : <div className="initial-connection" role={error ? "alert" : "status"}><span className="eyebrow">ARIS / LOCAL WORKSPACE</span><p>{error || "正在连接本机 Harness…"}</p>{error && <button className="outline-button" disabled={checking} onClick={() => void connect()}>重新连接</button>}</div>;
}
