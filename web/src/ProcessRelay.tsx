import type { CSSProperties } from "react";
import { AgentGlyph } from "./AgentGlyph";
import { RunningBorder } from "./RunningBorder";
import type { ModelFamily, ModelProfile, Team, TeamAgent } from "./teamTypes";
import { statusLabels } from "./teamTypes";
import { adminAgent, workerAgents } from "./teamConversationModel";

export const modelColors: Record<ModelFamily, string> = { gpt: "#169785", gemini: "#7182d7", grok: "#516a7a", deepseek: "#4c89d8", glm: "#8b71bc", kimi: "#b38a32" };
export function ModelGlyph({ family, className = "" }: { family?: ModelFamily; className?: string }) {
  return <AgentGlyph family={family} className={`lp-model-glyph ${className}`} />;
}

export function agentStages(agents: TeamAgent[]) {
  const depths = new Map<string, number>();
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const depth = (agent: TeamAgent, visited = new Set<string>()): number => {
    if (depths.has(agent.id)) return depths.get(agent.id)!;
    if (visited.has(agent.id)) return 0;
    const next = new Set(visited).add(agent.id);
    const level = Math.max(0, ...agent.depends_on.filter((id) => byId.has(id)).map((id) => depth(byId.get(id)!, next) + 1));
    depths.set(agent.id, level); return level;
  };
  for (const agent of agents) depth(agent);
  return Array.from(new Set(depths.values())).sort((a, b) => a - b).map((level) => agents.filter((agent) => depths.get(agent.id) === level));
}

export function relayStages(team: Team) {
  const admin = adminAgent(team);
  return admin ? [[admin], ...agentStages(workerAgents(team))] : agentStages(team.agents);
}

export function ProcessRelay({ teams, profiles, selectedTeamId, selectedAgentId, onSelect, onOpenTunnel }: { teams: Team[]; profiles: ModelProfile[]; selectedTeamId?: string; selectedAgentId?: string; onSelect: (teamId: string, agentId?: string) => void; onOpenTunnel?: (teamId: string, tunnelId: string) => void }) {
  return <section className="lp-relay" aria-label="智能体泳道">{teams.map((team, index) => {
    const members = relayStages(team).flat();
    const names = new Map(members.map((agent) => [agent.id, agent.name]));
    return <div className="lp-relay-group" key={team.id}>
      <button className={`lp-relay-label ${team.id === selectedTeamId ? "selected" : ""}`} title={team.name} onClick={() => onSelect(team.id)}>
        <i style={{ background: ["#4c92d1", "#8976c4", "#38a38f", "#c29142"][index % 4] }} />{team.name}
      </button>
      <div className="lp-relay-lanes">{members.map((agent) => {
        const profile = profiles.find((item) => item.id === agent.profile_id);
        const isAdmin = !!team.admin && team.admin.session_id === agent.session_id;
        const model = (isAdmin && team.admin?.model_id) || profile?.model || agent.profile_id;
        const selected = team.id === selectedTeamId && agent.id === selectedAgentId;
        const predecessors = agent.depends_on.map((id) => names.get(id) || id).join("、");
        const color = modelColors[profile?.family || "gpt"];
        return <div key={agent.id} className="lp-relay-row" data-relay-session={agent.session_id} data-subagent-id={agent.subagent_id}>
          <button className={`lp-model-chip ${agent.status} ${selected ? "selected" : ""}`}
            style={{ "--model-color": color, "--light-color": color } as CSSProperties}
            title={`${agent.name} · ${model} · ${statusLabels[agent.status]}`} aria-pressed={selected}
            data-anchor-key={`chip:${team.id}:${agent.id}`} data-relay-team={team.id} data-relay-agent={agent.id}
            onClick={() => onSelect(team.id, agent.id)}>
            {agent.status === "running" && <RunningBorder />}<ModelGlyph family={profile?.family} />
            <span>{isAdmin ? `${agent.name} · 管理员` : agent.name}</span>
            {agent.status === "completed" && <small>✓</small>}{agent.status === "running" && <i className="lp-chip-live" />}
            {agent.status === "failed" && <small className="lp-failed">!</small>}
          </button>
          <small className="lp-relay-model" title={model}>{model}</small>
          {predecessors && <small className="lp-relay-inputs" title={predecessors}>接收 {predecessors} 的交付</small>}
        </div>;
      })}{team.tunnels?.filter((t) => t.status === "open").map((t) => <button key={t.id} className="lp-tunnel-relay" data-live-tunnel={t.id} onClick={() => onOpenTunnel?.(team.id, t.id)}>
        <span aria-hidden="true">⇄</span><strong>{t.participants.map((id) => names.get(id) || id).join(" · ")}</strong><span>协商中 · {t.topic}</span>
      </button>)}</div>
    </div>;
  })}{!teams.length && <div className="lp-relay-empty">会话开始后，智能体会在这里各自展开。</div>}</section>;
}

export function ModelReel({ profiles, selected, onSelect, allowAuto = false }: { profiles: ModelProfile[]; selected?: string; onSelect: (id: string) => void; allowAuto?: boolean }) {
  const options = allowAuto && profiles.length ? [{ id: "", name: "自动分配", model: "", family: undefined }, ...profiles] : profiles;
  const value = selected || "";
  const index = Math.max(0, options.findIndex((profile) => profile.id === value));
  return <div className="lp-model-reel" aria-label="选择模型"><div className="lp-reel-mask"><div className="lp-reel-track" style={{ transform: `translateX(${150 - 58 - index * 116}px)` }}>{options.map((profile, i) => <button key={profile.id} style={{ left: i * 116, opacity: Math.max(.2, 1 - Math.abs(i - index) * .38) }} className={profile.id === value ? "active" : ""} aria-pressed={profile.id === value} title={profile.model || "根据任务自动选择已连接模型"} onClick={() => onSelect(profile.id)}>{profile.id ? <ModelGlyph family={profile.family} /> : <span aria-hidden="true">⇄</span>}<span>{profile.name}</span></button>)}</div></div><div className="lp-reel-lens" />{profiles.length > 0 && <select className="lp-reel-select" aria-label="当前模型" title="切换模型" value={value} onChange={(event) => onSelect(event.target.value)}>{options.map((profile) => <option key={profile.id} value={profile.id}>{profile.name}{profile.model ? ` · ${profile.model}` : ""}</option>)}</select>}{!profiles.length && <button className="lp-reel-empty" onClick={() => onSelect("")}>连接模型</button>}</div>;
}
