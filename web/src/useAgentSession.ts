import { useCallback, useEffect, useState } from "react";
import { errorText, rpc } from "./api";
import type { SessionResult } from "./types";

export function useAgentSession(token: string, sessionId?: string | null) {
  const [snapshot, setSnapshot] = useState<SessionResult>();
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const refresh = useCallback(() => setRevision((value) => value + 1), []);
  useEffect(() => {
    setError("");
    if (!sessionId) { setSnapshot(undefined); setLoading(false); return; }
    let stopped = false;
    let current: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setLoading(true);
    async function load() {
      const controller = new AbortController();
      current = controller;
      let timedOut = false;
      const deadline = setTimeout(() => { timedOut = true; controller.abort(); }, 10_000);
      try {
        const value = await rpc<SessionResult>(token, "sessions/get", { id: sessionId }, controller.signal);
        if (!stopped) { setSnapshot(value); setError(""); }
      } catch (failure) {
        if (!stopped) setError(timedOut ? "连接超时，将自动重试。" : errorText(failure));
      } finally {
        clearTimeout(deadline);
        if (!stopped) { setLoading(false); timer = setTimeout(() => void load(), 1500); }
      }
    }
    void load();
    return () => { stopped = true; current?.abort(); clearTimeout(timer); };
  }, [token, sessionId, revision]);
  return { snapshot: snapshot?.session.id === sessionId ? snapshot : undefined, error, loading, refresh };
}
