import { useEffect, useState } from "react";
import { errorText, rpc } from "./api";
import type { Approval, SessionResult } from "./types";
import type { Team } from "./teamTypes";

export function useTeamActivity(token?: string, team?: Team) {
  const [sessions, setSessions] = useState<Record<string, SessionResult>>({});
  const [approvals, setApprovals] = useState<Approval[]>([]);
  const [error, setError] = useState("");
  const signature = team?.agents.map((agent) => `${agent.id}:${agent.session_id}`).join("|");
  useEffect(() => {
    setSessions({}); setApprovals([]); setError("");
    if (!token || !team) return;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    const current = team;
    async function load() {
      const next: Record<string, SessionResult> = {};
      try {
        const agents = current.agents.filter((agent) => agent.session_id);
        for (let i = 0; i < agents.length; i += 8) {
          const batch = agents.slice(i, i + 8);
          const results = await Promise.all(batch.map(async (agent) => [agent.id, await rpc<SessionResult>(token!, "sessions/get", { id: agent.session_id }, abort.signal)] as const));
          for (const [id, snapshot] of results) next[id] = snapshot;
        }
        const pending = await rpc<{ approvals: Approval[] }>(token!, "approvals/list", {}, abort.signal);
        if (abort.signal.aborted) return;
        const taskIds = new Set(Object.values(next).map((snapshot) => snapshot.task_id).filter(Boolean));
        setSessions(next); setApprovals(pending.approvals.filter((item) => !!item.task_id && taskIds.has(item.task_id))); setError("");
      } catch (failure) { if (!abort.signal.aborted) setError(errorText(failure)); }
      finally { if (!abort.signal.aborted) timer = setTimeout(() => void load(), 2200); }
    }
    void load(); return () => { abort.abort(); clearTimeout(timer); };
  }, [token, team?.id, signature]);
  async function resolve(request_id: string, allow: boolean) {
    try {
      await rpc(token!, "approvals/resolve", { request_id, allow });
      setApprovals((items) => items.filter((item) => item.request_id !== request_id));
    } catch (failure) { setError(errorText(failure)); }
  }
  return { sessions, approvals, error, resolve };
}
