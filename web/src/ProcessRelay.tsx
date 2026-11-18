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

export function ProcessRelay({ teams, profiles, selectedTeamId, selectedAgentId, onSelect }: { teams: Team[]; profiles: ModelProfile[]; selectedTeamId?: string; selectedAgentId?: string; onSelect: (teamId: string, agentId?: string) => void }) {
  return <section className="lp-relay" aria-label="任务协作顺序">{teams.map((team, index) => <div className={`lp-relay-row ${team.id === selectedTeamId ? "selected" : ""}`} key={team.id}>
    <button className="lp-relay-label" title={team.name} onClick={() => onSelect(team.id)}><i style={{ background: ["#4c92d1", "#8976c4", "#38a38f", "#c29142"][index % 4] }} />{team.name}</button>
    <div className="lp-relay-stages">{relayStages(team).map((stage, stageIndex) => <span className="lp-relay-stage" key={stageIndex}>{stageIndex > 0 && <span className="lp-relay-arrow">→</span>}{stage.map((agent, agentIndex) => {
      const profile = profiles.find((item) => item.id === agent.profile_id);
      return <span className="lp-relay-step" key={agent.id}>{agentIndex > 0 && <span className="lp-relay-parallel" title="并行">+</span>}<button className={`lp-model-chip ${agent.status} ${team.id === selectedTeamId && agent.id === selectedAgentId ? "selected" : ""}`} style={{ "--model-color": modelColors[profile?.family || "gpt"], "--light-color": modelColors[profile?.family || "gpt"] } as CSSProperties} title={`${agent.name} · ${profile?.model || agent.profile_id} · ${statusLabels[agent.status]}`} data-anchor-key={`chip:${team.id}:${agent.id}`} data-relay-team={team.id} data-relay-agent={agent.id} onClick={() => onSelect(team.id, agent.id)}>{agent.status === "running" && <RunningBorder />}<ModelGlyph family={profile?.family} /><span>{profile?.name || agent.profile_id}</span>{agent.status === "completed" && <small>✓</small>}{agent.status === "running" && <i className="lp-chip-live" />}{agent.status === "failed" && <small className="lp-failed">!</small>}</button></span>;
    })}</span>)}</div>
  </div>)}{!teams.length && <div className="lp-relay-empty">会话开始后，模型与子任务将在这里同步展开。</div>}</section>;
}

export function ModelReel({ profiles, selected, onSelect, allowAuto = false }: { profiles: ModelProfile[]; selected?: string; onSelect: (id: string) => void; allowAuto?: boolean }) {
  const options = allowAuto && profiles.length ? [{ id: "", name: "自动分配", model: "", family: undefined }, ...profiles] : profiles;
  const value = selected || "";
  const index = Math.max(0, options.findIndex((profile) => profile.id === value));
  return <div className="lp-model-reel" aria-label="选择模型"><div className="lp-reel-mask"><div className="lp-reel-track" style={{ transform: `translateX(${150 - 58 - index * 116}px)` }}>{options.map((profile, i) => <button key={profile.id} style={{ left: i * 116, opacity: Math.max(.2, 1 - Math.abs(i - index) * .38) }} className={profile.id === value ? "active" : ""} aria-pressed={profile.id === value} title={profile.model || "根据任务自动选择已连接模型"} onClick={() => onSelect(profile.id)}>{profile.id ? <ModelGlyph family={profile.family} /> : <span aria-hidden="true">⇄</span>}<span>{profile.name}</span></button>)}</div></div><div className="lp-reel-lens" />{profiles.length > 0 && <select className="lp-reel-select" aria-label="当前模型" title="切换模型" value={value} onChange={(event) => onSelect(event.target.value)}>{options.map((profile) => <option key={profile.id} value={profile.id}>{profile.name}{profile.model ? ` · ${profile.model}` : ""}</option>)}</select>}{!profiles.length && <button className="lp-reel-empty" onClick={() => onSelect("")}>连接模型</button>}</div>;
}
