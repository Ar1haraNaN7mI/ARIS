import { useCallback, useEffect, useRef, useState } from "react";
import { errorText, isAbort, rpc } from "./api";
import type { KeiConfig, Team, TeamDraft } from "./teamTypes";
import { teamIsActive } from "./teamConversationModel";

export function stableTeams(previous: Team[], incoming: Team[]): Team[] {
  const byId = new Map(previous.map((team) => [team.id, team]));
  const next = incoming.map((team) => { const old = byId.get(team.id); return old && JSON.stringify(old) === JSON.stringify(team) ? old : team; });
  return previous.length === next.length && previous.every((team, index) => team === next[index]) ? previous : next;
}

export function useTeams(token?: string) {
  const [teams, setTeams] = useState<Team[]>([]);
  const [selectedId, select] = useState<string>();
  const [config, setConfig] = useState<KeiConfig>();
  const [error, setError] = useState("");
  const [pollError, setPollError] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [revision, setRevision] = useState(0);
  const alive = useRef(true);
  const operation = useRef(false);
  const generation = useRef(0);
  const reconcile = useCallback((team: Team) => {
    setTeams((items) => stableTeams(items, items.some((item) => item.id === team.id) ? items.map((item) => item.id === team.id ? team : item) : [team, ...items]));
  }, []);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  useEffect(() => {
    if (!token) return;
    const abort = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    let polling = false;
    let active = false;
    async function poll() {
      if (polling || abort.signal.aborted) return;
      polling = true;
      const version = generation.current;
      try {
        const result = await rpc<{ teams: Team[] }>(token!, "teams/list", {}, abort.signal);
        if (!abort.signal.aborted && version === generation.current) {
          setTeams((previous) => stableTeams(previous, result.teams));
          active = result.teams.some(teamIsActive);
          setPollError("");
        }
      } catch (failure) {
        if (!abort.signal.aborted && !isAbort(failure)) setPollError(errorText(failure));
      } finally {
        polling = false;
        if (!abort.signal.aborted) { setLoading(false); timer = setTimeout(() => void poll(), document.hidden ? 15_000 : active ? 1500 : 5000); }
      }
    }
    void poll();
    const visible = () => { if (!document.hidden) { clearTimeout(timer); void poll(); } };
    document.addEventListener("visibilitychange", visible);
    return () => { abort.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visible); };
  }, [token, revision]);
  const refreshConfig = useCallback(async () => {
    if (!token) return;
    try {
      const next = await rpc<KeiConfig>(token, "kei/config");
      if (alive.current) setConfig(next);
    } catch (failure) { if (alive.current) setError(errorText(failure)); }
  }, [token]);
  useEffect(() => { void refreshConfig(); }, [refreshConfig]);
  async function act(method: string, params: Record<string, unknown>) {
    if (!token || operation.current) return;
    operation.current = true;
    generation.current++;
    setBusy(true); setError("");
    try {
      const result = await rpc<{ team: Team }>(token, method, params);
      if (!alive.current) return;
      reconcile(result.team); select(result.team.id);
      return result.team;
    } catch (failure) { if (alive.current) setError(errorText(failure)); }
    finally { operation.current = false; generation.current++; if (alive.current) { setBusy(false); setRevision((value) => value + 1); } }
  }
  return {
    teams, selectedId, select, config, refreshConfig, busy, loading, error: error || pollError,
    team: teams.find((item) => item.id === selectedId),
    retry: () => { setError(""); setRevision((value) => value + 1); },
    create: (draft: TeamDraft) => act("teams/create", draft),
    plan: (goal: string, profile_id?: string, template_id?: string) => act("teams/plan", { goal, profile_id, template_id }),
    admin: (message: string, id?: string, profile_id?: string) => act("teams/admin", { message, id, profile_id }),
    model: (id: string, agent_id: string, profile_id: string) => act("teams/model", { id, agent_id, profile_id }),
    run: (id: string) => act("teams/run", { id }),
    cancel: (id: string) => act("teams/cancel", { id }),
    checkpoint: (id: string, label: string) => act("teams/checkpoint", { id, label }),
    restore: (id: string, checkpoint_id: string) => act("teams/restore", { id, checkpoint_id }),
    message: (id: string, message: string, agent_id?: string) => act("teams/message", { id, message, agent_id }),
  };
}
