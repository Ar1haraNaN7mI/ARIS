import type { ModelProfile, Team, TeamAgent } from "./teamTypes";
import { conversationAgents, progressAgents } from "./teamConversationModel";
import type { SessionEvent, SessionResult } from "./types";
import { sessionModelIdentities, type SessionModelIdentity } from "./sessionModelIdentity";

export type TraceKind = "start" | "message" | "reply" | "tool" | "artifact" | "checkpoint" | "merge" | "wait" | "error" | "complete" | "cancelled" | "model" | "note";
export type TraceNode = {
  id: string; revision: string; teamId: string; agentId?: string; at: string;
  kind: TraceKind; label: string; detail: string; source: "team" | "session" | "checkpoint";
  eventId?: string; anchorEventId?: string; ok?: boolean; running?: boolean; arguments?: unknown; result?: string;
  identity?: SessionModelIdentity;
  parentIds: string[]; branch: number; y: number; durationMs?: number;
};
export type TraceLane = {
  team: Team; color: string; nodes: TraceNode[]; width: number; height: number;
  currentAgent?: TeamAgent; model: string; completed: number; total: number;
};

// Same authored lane palette as Lancer. It identifies task lines, not success.
const LANE_COLORS = ["#3B82F6", "#8B5CF6", "#10B981", "#DB2777", "#0891b2", "#b58430"];
export const traceLabels: Record<TraceKind, string> = {
  start: "开始", message: "消息", reply: "回复", tool: "工具调用", artifact: "文件写入",
  checkpoint: "检查点", merge: "依赖汇合", wait: "等待确认", error: "错误", complete: "阶段完成",
  cancelled: "已停止", model: "模型切换", note: "运行记录",
};

function branchSlots(agents: TeamAgent[]): Map<string, number> {
  const slots = new Map<string, number>();
  const children = new Map(agents.map((agent) => [agent.id, agents.filter((other) => other.depends_on.includes(agent.id))]));
  let next = 0;
  for (let pass = 0; pass < agents.length; pass++) {
    let changed = false;
    for (const agent of agents) {
      if (slots.has(agent.id) || agent.depends_on.some((id) => !slots.has(id))) continue;
      let slot: number;
      if (!agent.depends_on.length) slot = next++;
      else if (agent.depends_on.length > 1) slot = Math.min(...agent.depends_on.map((id) => slots.get(id)!));
      else {
        const parent = agent.depends_on[0];
        slot = children.get(parent)?.[0]?.id === agent.id ? slots.get(parent)! : next++;
      }
      slots.set(agent.id, slot); changed = true;
    }
    if (!changed) break;
  }
  for (const agent of agents) if (!slots.has(agent.id)) slots.set(agent.id, next++);
  return slots;
}

function compact(text: string, limit = 42) { const value = text.replace(/\s+/g, " ").trim(); return value.length > limit ? `${value.slice(0, limit)}…` : value; }
function checkpointLabel(label: string) { return ({ "Before run": "运行前快照", "After run": "运行后快照", "Before restore (undo)": "恢复前备份" } as Record<string, string>)[label] || label; }
function time(at: string) { const value = Date.parse(at); return Number.isFinite(value) ? value : 0; }
function shortUserInput(team: Team, agentId: string, at: string) {
  return [...(team.messages || [])].filter((message) => (!message.agent_id || message.agent_id === agentId) && time(message.at) <= time(at))
    .sort((a, b) => time(b.at) - time(a.at))[0]?.text || team.goal;
}
function toolLabel(event: SessionEvent, ok?: boolean) { return `${event.name || "工具"}${ok === false ? " · 失败" : ""}`; }
function toolFile(argumentsValue: unknown): string | undefined {
  if (!argumentsValue || typeof argumentsValue !== "object" || Array.isArray(argumentsValue)) return;
  const value = argumentsValue as Record<string, unknown>;
  const path = value.path || value.file_path;
  return typeof path === "string" && path.trim() ? path : undefined;
}

function sessionNodes(team: Team, agent: TeamAgent, events: SessionEvent[], profiles: ModelProfile[]): TraceNode[] {
  const nodes: TraceNode[] = [];
  const identities = sessionModelIdentities(events, profiles);
  const pending = new Map<string, TraceNode>();
  const closeTurn = () => { for (const node of pending.values()) node.running = false; pending.clear(); };
  for (const event of events) {
    if (event.type === "turn_start" || event.type === "turn_end") { closeTurn(); continue; }
    if (!["user_message", "assistant_message", "tool_call", "tool_result", "system_note"].includes(event.type)) continue;
    if (event.type === "tool_result" && event.call_id && pending.has(event.call_id)) {
      const node = pending.get(event.call_id)!;
      node.durationMs = Math.max(0, time(event.at) - time(node.at));
      node.at = event.at; node.eventId = event.id; node.revision = `session:${agent.id}:${event.id}`;
      node.ok = event.ok; node.running = false; node.result = event.content;
      node.label = toolLabel(event, event.ok);
      const path = toolFile(node.arguments);
      if (event.ok && path && ["write_file", "edit_file", "create_file"].includes(event.name || "")) { node.kind = "artifact"; node.label = path; }
      pending.delete(event.call_id);
      continue;
    }
    const kind: TraceKind = event.type === "user_message" ? "message" : event.type === "assistant_message" ? "reply" : event.type.startsWith("tool_") ? "tool" : "note";
    const node: TraceNode = {
      id: `session:${agent.id}:${event.id}`, revision: `session:${agent.id}:${event.id}`, teamId: team.id, agentId: agent.id,
      at: event.at, kind, source: "session", eventId: event.id, anchorEventId: event.id, parentIds: [], branch: 0, y: 0,
      identity: identities.get(event.id),
      label: kind === "message" ? compact(shortUserInput(team, agent.id, event.at)) : kind === "reply" ? compact(event.text || "模型回复") : kind === "tool" ? toolLabel(event, event.ok) : compact(event.text || "运行记录"),
      detail: event.text || event.name || "", arguments: event.arguments, result: event.content, ok: event.ok,
      running: event.type === "tool_call" && agent.status === "running" && ["planning", "running", "cancelling"].includes(team.status),
    };
    nodes.push(node);
    if (event.type === "tool_call" && event.call_id) pending.set(event.call_id, node);
  }
  return nodes;
}

function teamNode(team: Team, event: Team["events"][number], index: number): TraceNode {
  const agent = team.agents.find((value) => value.id === event.agent_id);
  let kind: TraceKind = "note", label = event.message;
  let ok: boolean | undefined;
  switch (event.kind) {
    case "created": kind = "start"; label = "工作区已创建"; break;
    case "started": kind = "start"; label = "开始执行"; break;
    case "admin_started": kind = "start"; label = "ARIS · 正在处理"; break;
    case "admin_answered": kind = "complete"; label = "ARIS · 已回复"; break;
    case "admin_delegated": kind = "merge"; label = "ARIS · 已安排协作"; break;
    case "admin_failed": kind = "error"; label = "ARIS · 未完成"; break;
    case "admin_cancelled": kind = "cancelled"; label = "ARIS · 已停止"; break;
    case "agent_started": kind = agent && agent.depends_on.length > 1 ? "merge" : "start"; label = agent ? `${agent.name} · 开始` : event.message; break;
    case "agent_finished":
      kind = /:\s*failed$/.test(event.message) ? "error" : /:\s*cancelled$/.test(event.message) ? "cancelled" : /:\s*completed$/.test(event.message) ? "complete" : "note";
      label = `${agent?.name || "智能体"} · ${kind === "complete" ? "完成" : kind === "error" ? "失败" : kind === "cancelled" ? "停止" : "结束"}`;
      break;
    case "tool": kind = "tool"; break;
    case "tool_finished": kind = "tool"; ok = /:\s*completed$/.test(event.message) ? true : /:\s*failed$/.test(event.message) ? false : undefined; label = event.message.replace(/:\s*completed$/, " · 返回").replace(/:\s*failed$/, " · 失败"); break;
    case "approval_needed": kind = "wait"; label = "等待确认"; break;
    case "user_message": case "message_queued": kind = "message"; label = compact(event.message); break;
    case "messages_applied": kind = "message"; label = "已接收补充要求"; break;
    case "model_changed": kind = "model"; label = compact(event.message); break;
    case "checkpoint": kind = "checkpoint"; label = checkpointLabel(event.message); break;
    case "agent_error": case "error": case "interrupted": kind = "error"; label = compact(event.message); break;
    case "cancelled": case "cancelling": kind = "cancelled"; label = event.kind === "cancelled" ? "执行已停止" : "正在停止"; break;
    case "finished": kind = event.message === "All agents completed" ? "complete" : "error"; label = kind === "complete" ? "流程完成" : "流程未完成"; break;
    case "restored": kind = "checkpoint"; label = "恢复检查点"; break;
  }
  return { id: `team-event:${index}`, revision: `team-event:${index}:${event.at}`, teamId: team.id, agentId: event.agent_id || undefined,
    at: event.at, kind, label: compact(label), detail: event.message, source: "team", ok, parentIds: [], branch: 0, y: 0 };
}

export function projectWorkspaceTrace(teams: Team[], profiles: ModelProfile[] = [], activeTeamId?: string, sessions: Record<string, SessionResult> = {}): TraceLane[] {
  const order = [...teams].sort((a, b) => a.created_at.localeCompare(b.created_at) || a.id.localeCompare(b.id));
  return teams.map((team) => {
    const agents = conversationAgents(team);
    const workers = progressAgents(team);
    const actualSessions = new Map<string, SessionResult>();
    if (team.id === activeTeamId) for (const agent of agents) {
      const session = sessions[agent.id];
      if (session && agent.session_id && session.session.id === agent.session_id) actualSessions.set(agent.id, session);
    }
    const nodes: TraceNode[] = team.events.flatMap((event, index) => {
      // Team tool events are summaries without call IDs. Prefer their actual
      // session records when available; never guess pairings from tool names.
      if (event.agent_id && actualSessions.has(event.agent_id) && ["tool", "tool_finished"].includes(event.kind)) return [];
      return [teamNode(team, event, index)];
    });
    for (const agent of agents) {
      const session = actualSessions.get(agent.id);
      if (session) nodes.push(...sessionNodes(team, agent, session.session.events, profiles));
    }
    // Persisted checkpoints can outlive event truncation. Show the real record,
    // and do not duplicate the matching checkpoint event when it is present.
    for (const checkpoint of team.checkpoints) if (!team.events.some((event) => event.kind === "checkpoint" && event.message === checkpoint.label && Math.abs(time(event.at) - time(checkpoint.at)) < 1000)) {
      nodes.push({ id: `checkpoint:${checkpoint.id}`, revision: `checkpoint:${checkpoint.id}`, teamId: team.id, at: checkpoint.at,
        kind: "checkpoint", label: checkpointLabel(checkpoint.label), detail: checkpoint.files.map((file) => `${file.path} · ${file.exists ? `${file.size} bytes` : "未创建"}`).join("\n"), source: "checkpoint", parentIds: [], branch: 0, y: 0 });
    }
    nodes.sort((a, b) => time(a.at) - time(b.at));
    const slots = branchSlots(agents);
    const latest = new Map<string, TraceNode>();
    let trunk: TraceNode | undefined;
    let cursor = 24;
    for (const node of nodes) {
      node.branch = node.agentId ? slots.get(node.agentId) || 0 : 0;
      node.y = cursor;
      cursor += node.kind === "tool" ? 26 : 43;
      if (node.agentId) {
        const previous = latest.get(node.agentId);
        if (previous) node.parentIds.push(previous.id);
        const agent = agents.find((value) => value.id === node.agentId);
        if (!previous || (node.source === "team" && ["start", "merge"].includes(node.kind))) {
          for (const parentId of agent?.depends_on || []) { const parent = latest.get(parentId); if (parent && !node.parentIds.includes(parent.id)) node.parentIds.push(parent.id); }
          if (!node.parentIds.length && trunk) node.parentIds.push(trunk.id);
        }
        latest.set(node.agentId, node);
      } else {
        if (trunk) node.parentIds.push(trunk.id);
        if (["complete", "checkpoint", "cancelled"].includes(node.kind)) for (const previous of latest.values()) if (!node.parentIds.includes(previous.id)) node.parentIds.push(previous.id);
        trunk = node;
      }
    }
    const currentAgent = agents.find((agent) => agent.status === "running") || [...agents].reverse().find((agent) => agent.status !== "pending") || agents[0];
    const profile = profiles.find((value) => value.id === currentAgent?.profile_id);
    return { team, color: LANE_COLORS[order.findIndex((value) => value.id === team.id) % LANE_COLORS.length], nodes,
      width: Math.max(112, 112 + Math.max(0, ...slots.values()) * 20), height: Math.max(280, cursor + 72), currentAgent,
      model: profile?.model || profile?.name || currentAgent?.profile_id || "—",
      completed: workers.filter((agent) => agent.status === "completed").length, total: workers.length };
  });
}

/** New server records only. Initial history and later-loaded old session data do
 * not become live emissions just because the user selected another task. */
export function newTraceNodes(lanes: TraceLane[], previous: Map<string, { revisions: Set<string>; latest: number; baseline?: number }>): TraceNode[] {
  const added: TraceNode[] = [];
  for (const lane of lanes) {
    const before = previous.get(lane.team.id);
    const latest = Math.max(0, ...lane.nodes.map((node) => time(node.at)));
    // Sessions and team summaries poll independently. A newly received session
    // result may precede the latest team summary without being old history.
    // Keep the first visible baseline and all seen IDs across detail toggles.
    const baseline = before?.baseline ?? before?.latest ?? latest;
    if (before) for (const node of lane.nodes) if (!before.revisions.has(node.revision) && time(node.at) > baseline && node.agentId) added.push(node);
    previous.set(lane.team.id, { revisions: new Set([...(before?.revisions || []), ...lane.nodes.map((node) => node.revision)]), latest: Math.max(latest, before?.latest || 0), baseline });
  }
  for (const id of previous.keys()) if (!lanes.some((lane) => lane.team.id === id)) previous.delete(id);
  return added;
}
