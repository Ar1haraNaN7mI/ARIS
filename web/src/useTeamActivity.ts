import { useEffect, useMemo, useState } from "react";
import { errorText, rpc } from "./api";
import { SessionCache } from "./sessionCache";
import { conversationAgents, teamIsActive } from "./teamConversationModel";
import type { Approval, SessionResult } from "./types";
import type { Team } from "./teamTypes";

export function activityRevisions(team: Team) {
  const latest = new Map<string, string>();
  for (const event of team.events || []) if (event.agent_id) latest.set(event.agent_id, `${event.at}:${event.kind}`);
  return conversationAgents(team).filter((agent) => agent.session_id).map((agent) => ({ agent,
    revision: `${agent.status}:${latest.get(agent.id) || ""}:${team.admin && agent.session_id === team.admin.session_id ? `${team.admin.request_id}:${team.admin.phase}` : ""}`,
  }));
}
const sameSnapshots = (a: Record<string, SessionResult>, b: Record<string, SessionResult>) => Object.keys(a).length === Object.keys(b).length && Object.keys(a).every((key) => a[key] === b[key]);

export function useTeamActivity(token?: string, team?: Team) {
  const [sessions, setSessions] = useState<Record<string, SessionResult>>({});
  const [approvals, setApprovals] = useState<Approval[]>([]);
  const [error, setError] = useState("");
  const cache = useMemo(() => new SessionCache(token || ""), [token]);
  useEffect(() => () => cache.dispose(), [cache]);
  const watched = team ? activityRevisions(team) : [];
  const signature = watched.map(({ agent, revision }) => `${agent.id}:${agent.session_id}:${revision}`).join("|");
  useEffect(() => {
    setApprovals([]); setError("");
    if (!token || !team) { setSessions({}); return; }
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout> | undefined;
    let loading = false;
    let first = true;
    const snapshot = () => Object.fromEntries(watched.flatMap(({ agent }) => { const value = cache.peek(agent.session_id!); return value ? [[agent.id, value]] : []; }));
    setSessions((previous) => { const next = snapshot(); return sameSnapshots(previous, next) ? previous : next; });
    async function load() {
      if (loading || abort.signal.aborted) return;
      loading = true;
      const errors: string[] = [];
      try {
        const due = watched.filter(({ agent, revision }) => agent.status === "running" || !cache.synced(agent.session_id!, revision));
        for (let i = 0; i < due.length && !abort.signal.aborted; i += 6) {
          const results = await Promise.allSettled(due.slice(i, i + 6).map(({ agent, revision }) => cache.load(agent.session_id!, revision)));
          for (const result of results) if (result.status === "rejected") errors.push(errorText(result.reason));
        }
        if (abort.signal.aborted) return;
        const next = snapshot();
        setSessions((previous) => sameSnapshots(previous, next) ? previous : next);
        if (teamIsActive(team) || first) {
          const pending = await rpc<{ approvals: Approval[] }>(token!, "approvals/list", {}, abort.signal);
          const taskIds = new Set(Object.values(next).map((value) => value.task_id).filter(Boolean));
          if (!abort.signal.aborted) setApprovals((previous) => {
            const relevant = pending.approvals.filter((item) => !!item.task_id && taskIds.has(item.task_id));
            return JSON.stringify(previous) === JSON.stringify(relevant) ? previous : relevant;
          });
        }
        first = false;
        if (!abort.signal.aborted) setError(errors.join("；"));
      } catch (failure) { errors.push(errorText(failure)); if (!abort.signal.aborted) setError(errorText(failure)); }
      finally {
        loading = false;
        if (!abort.signal.aborted && (teamIsActive(team) || errors.length)) timer = setTimeout(() => void load(), document.hidden ? 10_000 : 2200);
      }
    }
    const visible = () => { if (!document.hidden) { clearTimeout(timer); void load(); } };
    void load(); document.addEventListener("visibilitychange", visible);
    return () => { abort.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visible); };
  }, [token, team?.id, team?.status, signature, cache]);
  async function resolve(request_id: string, allow: boolean) {
    try {
      await rpc(token!, "approvals/resolve", { request_id, allow });
      setApprovals((items) => items.filter((item) => item.request_id !== request_id));
    } catch (failure) { setError(errorText(failure)); }
  }
  return { sessions, approvals, error, resolve };
}
