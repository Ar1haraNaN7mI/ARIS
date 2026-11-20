import type { ModelProfile, Team, TeamAgent } from "./teamTypes";
import { adminAgent, conversationAgents } from "./teamConversationModel";
import { mirrorsReceivedTunnelMessage, tunnelMessageLabel } from "./tunnelMessages";
import { actionLabels, messageAction, toolAction, type CollaborationAction } from "./collaborationActions";
import type { SessionEvent, SessionResult } from "./types";
import { sessionModelIdentities, type SessionModelIdentity } from "./sessionModelIdentity";

export type TraceKind = "start" | "message" | "reply" | "tool" | "artifact" | "checkpoint" | "merge" | "wait" | "error" | "complete" | "cancelled" | "model" | "note" | "tunnel";
export type TraceEdgeKind = "sequence" | "dependency" | "handoff" | "dispatch" | "tunnel";
export type TraceNode = {
  id: string; revision: string; teamId: string; laneId?: string; agentId?: string; at: string;
  kind: TraceKind; label: string; detail: string; source: "team" | "session" | "checkpoint" | "handoff" | "tunnel";
  tunnelId?: string; sentNodeId?: string; tunnelOrder?: number;
  action?: CollaborationAction;
  eventId?: string; anchorEventId?: string; ok?: boolean; running?: boolean; arguments?: unknown; result?: string;
  identity?: SessionModelIdentity;
  parentIds: string[]; parentKinds: Record<string, TraceEdgeKind>; y: number; durationMs?: number;
  teamEventKind?: string; handoffFrom?: string;
  checkpointId?: string; files?: Team["checkpoints"][number]["files"];
  summaryEvent?: Team["events"][number];
};
export type TraceLane = {
  id: string; kind: "agent" | "admin"; agent: TeamAgent; label: string;
  team: Team; color: string; nodes: TraceNode[]; width: number; height: number;
  model: string;
};
export type WorkspaceTrace = { lanes: TraceLane[]; teamEvents: Map<string, TraceNode[]> };
export type TraceHistory = { revisions: Set<string>; latest: number; baseline?: number; present?: boolean };

// The column identity belongs to the role, never to its current model.
const LANE_COLORS = ["#3B82F6", "#8B5CF6", "#10B981", "#DB2777", "#0891b2", "#b58430"];
export const traceLabels: Record<TraceKind, string> = {
  tunnel: "角色协商",
  start: "开始", message: "消息", reply: "回复", tool: "工具调用", artifact: "文件写入",
  checkpoint: "检查点", merge: "依赖汇合", wait: "等待确认", error: "错误", complete: "阶段完成",
  cancelled: "已停止", model: "模型切换", note: "运行记录",
};

const key = (...parts: string[]) => parts.map(encodeURIComponent).join("/");
export const traceLaneId = (teamId: string, agentId: string) => key("role", teamId, agentId);
const nodeId = (teamId: string, ...parts: string[]) => key("trace", teamId, ...parts);
function hash(text: string) { let value = 2166136261; for (const char of text) value = Math.imul(value ^ char.charCodeAt(0), 16777619); return value >>> 0; }
function compact(text: string, limit = 42) { const value = text.replace(/\s+/g, " ").trim(); return value.length > limit ? `${value.slice(0, limit)}…` : value; }
function checkpointLabel(label: string) { return ({ "Before run": "运行前快照", "After run": "运行后快照", "Before restore (undo)": "恢复前备份" } as Record<string, string>)[label] || label; }
function time(at: string) { const value = Date.parse(at); return Number.isFinite(value) ? value : 0; }
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
  const occurrences = new Map<string, number>();
  const isAdmin = !!team.admin && team.admin.session_id === agent.session_id;
  const closeTurn = () => { for (const node of pending.values()) node.running = false; pending.clear(); };
  for (const event of events) {
    if (event.type === "turn_start" || event.type === "turn_end") { closeTurn(); continue; }
    if (!["user_message", "assistant_message", "tool_call", "tool_result", "system_note"].includes(event.type)) continue;
    if (event.type === "system_note" && mirrorsReceivedTunnelMessage(team, agent.id, event.id)) continue;
    if (event.type === "tool_result" && event.call_id && pending.has(event.call_id)) {
      const node = pending.get(event.call_id)!;
      node.durationMs = Math.max(0, time(event.at) - time(node.at));
      node.at = event.at; node.eventId = event.id; node.revision = `${node.id}/result/${key(event.type, event.id, event.at)}`;
      node.ok = event.ok; node.running = false; node.result = event.content;
      node.label = `${node.action ? actionLabels[node.action] : event.name || node.label}${event.ok === false ? " · 失败" : ""}`;
      const path = toolFile(node.arguments);
      if (event.ok && path && ["write_file", "edit_file", "create_file"].includes(event.name || "")) { node.kind = "artifact"; node.label = path; }
      pending.delete(event.call_id);
      continue;
    }
    const kind: TraceKind = event.type === "user_message" ? isAdmin ? "message" : "note" : event.type === "assistant_message" ? "reply" : event.type.startsWith("tool_") ? "tool" : "note";
    const recordKey = key(event.type, event.id);
    const occurrence = occurrences.get(recordKey) || 0; occurrences.set(recordKey, occurrence + 1);
    const id = nodeId(team.id, "session", agent.id, agent.session_id || "", event.type, event.id, String(occurrence));
    const action = kind === "tool" ? toolAction(event.name, event.arguments) : undefined;
    const node: TraceNode = {
      id, revision: id, teamId: team.id, laneId: traceLaneId(team.id, agent.id), agentId: agent.id,
      at: event.at, kind, source: "session", eventId: event.id, anchorEventId: event.id, parentIds: [], parentKinds: {}, y: 0,
      identity: identities.get(event.id),
      label: event.type === "user_message" ? isAdmin ? compact(event.text || "消息") : "接收执行上下文" : kind === "reply" ? compact(event.text || "模型回复") : kind === "tool" ? toolLabel(event, event.ok) : compact(event.text || "运行记录"),
      action, detail: event.text || event.name || "", arguments: event.arguments, result: event.content, ok: event.ok,
      running: event.type === "tool_call" && agent.status === "running" && ["planning", "running", "cancelling"].includes(team.status),
    };
    nodes.push(node);
    if (action) node.label = actionLabels[action];
    if (event.type === "tool_call" && event.call_id) pending.set(event.call_id, node);
  }
  return nodes;
}

function teamNode(team: Team, event: Team["events"][number], occurrence: number, agent?: TeamAgent): TraceNode {
  let kind: TraceKind = "note", label = event.message;
  let ok: boolean | undefined;
  let action: CollaborationAction | undefined;
  const channel = event.kind.startsWith("tunnel_") ? team.tunnels?.find((t) => event.message.startsWith(t.id + ": ")) : undefined;
  switch (event.kind) {
    case "created": kind = "start"; label = "工作区已创建"; break;
    case "started": kind = "start"; label = "开始执行"; break;
    case "admin_started": kind = "start"; label = "ARIS · 正在处理"; break;
    case "admin_answered": kind = "complete"; label = "ARIS · 已回复"; break;
    case "admin_delegated": kind = "merge"; action = "dispatch"; label = "ARIS · 已安排协作"; break;
    case "admin_failed": kind = "error"; label = "ARIS · 未完成"; break;
    case "admin_cancelled": kind = "cancelled"; label = "ARIS · 已停止"; break;
    case "admin_stopped": kind = "note"; label = "ARIS · 处理已停止"; break;
    case "subagent_created": kind = "note"; action = "create"; label = `已创建 · ${agent?.name || event.message}`; break;
    case "agent_started": kind = "start"; label = agent ? `${agent.name} · 开始` : event.message; break;
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
    case "tunnel_resumed": kind = "start"; action = "resume"; label = "继续处理协作请求"; break;
    case "cowork_turn_started": kind = "start"; action = event.kind; label = actionLabels[action]; break;
    case "cowork_turn_finished": kind = "note"; action = event.kind; label = actionLabels[action]; break;
    case "tunnel_followup_ended": action = "end_followup"; label = "协作跟进已结束"; break;
    case "tunnel_opened": case "tunnel_closed": case "tunnel_decision": case "tunnel_message": {
      kind = "tunnel";
      action = ({ tunnel_opened: "open", tunnel_closed: "close", tunnel_decision: "accept", tunnel_message: "message" } as const)[event.kind];
      label = channel ? `${actionLabels[action]} · ${channel.topic}` : actionLabels[action];
      break;
    }
    case "agent_error": case "error": case "interrupted": kind = "error"; label = compact(event.message); break;
    case "cancelled": case "cancelling": kind = "cancelled"; label = event.kind === "cancelled" ? "执行已停止" : "正在停止"; break;
    case "finished": kind = event.message === "All agents completed" ? "complete" : "error"; label = kind === "complete" ? "流程完成" : "流程未完成"; break;
    case "restored": kind = "checkpoint"; label = "已恢复登记文件"; break;
    case "handoff": kind = "merge"; action = "handoff"; label = compact(event.message); break;
  }
  const id = nodeId(team.id, "team", event.at, event.kind, event.agent_id || "", String(occurrence));
  return { id, revision: `${id}/${hash(event.message)}`, teamId: team.id, agentId: event.agent_id || undefined,
    at: event.at, kind, action, tunnelId: channel?.id, label: compact(label), detail: event.message, source: "team", teamEventKind: event.kind, ok, parentIds: [], parentKinds: {}, y: 0 };
}

// Compare sub-millisecond server timestamps without treating a newer summary
// as covered by an earlier session record that happens in the same millisecond.
function compareAt(a: string, b: string) {
  const millis = time(a) - time(b);
  if (millis) return millis;
  const remainder = (value: string) => (value.match(/\.(\d+)(?:Z|[+-]\d\d:\d\d)$/)?.[1] || "").padEnd(9, "0").slice(3, 9);
  return remainder(a).localeCompare(remainder(b));
}
const byTime = (a: TraceNode, b: TraceNode) => compareAt(a.at, b.at) || (a.tunnelOrder ?? -1) - (b.tunnelOrder ?? -1);
const gap = (node: TraceNode) => node.kind === "tool" ? 28 : 43;

function arrangeRoleLanes(lanes: TraceLane[]) {
  const byAgent = new Map(lanes.map((lane) => [lane.agent.id, lane]));
  const latest = new Map<string, TraceNode>();
  const deliveries = new Map<string, TraceNode>();
  const sent = new Map<string, TraceNode>();
  const consumed = new Map<string, Map<string, string>>();
  const lastStart = new Map<string, TraceNode>();
  let dispatch: TraceNode | undefined;
  const link = (node: TraceNode, parent: TraceNode | undefined, kind: TraceEdgeKind) => {
    if (!parent || parent.id === node.id || compareAt(parent.at, node.at) > 0 || node.parentIds.includes(parent.id)) return;
    node.parentIds.push(parent.id); node.parentKinds[parent.id] = kind;
    node.y = Math.max(node.y, parent.y + gap(node));
  };
  for (const node of lanes.flatMap((lane) => lane.nodes).sort(byTime)) {
    const lane = byAgent.get(node.agentId!);
    if (!lane) continue;
    const previous = latest.get(lane.agent.id);
    node.y = 24;
    link(node, previous, "sequence");
    if (node.sentNodeId) link(node, sent.get(node.sentNodeId), "tunnel");
    else if (node.source === "tunnel") sent.set(node.id, node);
    const starting = node.teamEventKind === "agent_started" || (!previous && node.source !== "tunnel" && !node.handoffFrom && node.teamEventKind !== "subagent_created");
    if (starting && lane.kind !== "admin") {
      const priorInputs = consumed.get(lane.agent.id) || new Map<string, string>();
      let newInputs = 0;
      for (const dependency of lane.agent.depends_on) {
        const delivery = deliveries.get(dependency);
        if (delivery && priorInputs.get(dependency) !== delivery.id) {
          link(node, delivery, "dependency"); priorInputs.set(dependency, delivery.id); newInputs++;
        }
      }
      consumed.set(lane.agent.id, priorInputs);
      if (node.teamEventKind === "agent_started" && newInputs > 1) node.kind = "merge";
      // A later direct continuation is not a fresh dispatch from an old admin turn.
      const priorStart = lastStart.get(lane.agent.id);
      if (!lane.agent.depends_on.length && dispatch && (!priorStart || compareAt(priorStart.at, dispatch.at) < 0)) link(node, dispatch, "dispatch");
      lastStart.set(lane.agent.id, node);
    }
    if (node.handoffFrom) {
      const source = latest.get(node.handoffFrom);
      link(node, source, "handoff");
      const delivered = deliveries.get(node.handoffFrom);
      if (source && delivered?.id === source.id && lane.agent.depends_on.includes(node.handoffFrom)) {
        const inputs = consumed.get(lane.agent.id) || new Map<string, string>();
        inputs.set(node.handoffFrom, source.id); consumed.set(lane.agent.id, inputs);
      }
      lastStart.set(lane.agent.id, node);
    }
    if (node.teamEventKind === "admin_delegated") dispatch = node;
    latest.set(lane.agent.id, node);
    if (node.kind === "reply" || node.kind === "artifact" || (node.teamEventKind === "agent_finished" && node.kind === "complete")) deliveries.set(lane.agent.id, node);
  }
  for (const lane of lanes) {
    lane.nodes.sort(byTime);
    lane.height = lane.nodes.reduce((height, node) => Math.max(height, node.y + 72), 280);
  }
}

export function projectWorkspaceTrace(teams: Team[], profiles: ModelProfile[] = [], activeTeamId?: string, sessions: Record<string, SessionResult> = {}): WorkspaceTrace {
  const lanes: TraceLane[] = [];
  const teamEvents = new Map<string, TraceNode[]>();
  for (const team of teams) {
    const admin = adminAgent(team);
    const agents = conversationAgents(team).map((agent) => {
      const original = team.agents.find((value) => value.id === agent.id);
      return original ? { ...agent, name: original.name } : agent;
    });
    const roles = new Map<string, TraceLane>();
    const records: TraceNode[] = [];
    teamEvents.set(team.id, records);
    const toolCoverage = new Map<string, string>();
    for (const [roleIndex, agent] of agents.entries()) {
      const id = traceLaneId(team.id, agent.id);
      const profile = profiles.find((value) => value.id === agent.profile_id);
      const kind = agent.session_id && agent.session_id === team.admin?.session_id ? "admin" : "agent";
      const colorIndex = hash(team.id) + roleIndex;
      const lane: TraceLane = { id, kind, agent, label: agent.name, team, color: LANE_COLORS[colorIndex % LANE_COLORS.length],
        model: kind === "admin" ? team.admin!.model_id || profile?.model || agent.profile_id : profile?.model || profile?.name || agent.profile_id,
        nodes: [], width: 124, height: 280 };
      roles.set(agent.id, lane); lanes.push(lane);
      const snapshot = team.id === activeTeamId ? sessions[agent.id] : undefined;
      if (snapshot && agent.session_id && snapshot.session.id === agent.session_id) {
        lane.nodes = sessionNodes(team, agent, snapshot.session.events, profiles);
        if (snapshot.session.events.some((event) => event.type === "tool_call" || event.type === "tool_result")) {
          toolCoverage.set(agent.id, snapshot.session.events.reduce((at, event) => compareAt(event.at, at) > 0 ? event.at : at, ""));
        }
      }
    }
    const occurrences = new Map<string, number>();
    for (const event of team.events) {
      const recordKey = key(event.at, event.kind, event.agent_id || "");
      const occurrence = occurrences.get(recordKey) || 0; occurrences.set(recordKey, occurrence + 1);
      const node = teamNode(team, event, occurrence, roles.get(event.agent_id || "")?.agent);
      // Only an explicit role ID or an explicit admin event has a role owner.
      const owner = event.agent_id || (event.kind.startsWith("admin_") ? admin?.id : undefined);
      const lane = owner ? roles.get(owner) : undefined;
      const coveredThrough = owner ? toolCoverage.get(owner) : undefined;
      const coveredSummary = ["tool", "tool_finished"].includes(event.kind) && coveredThrough && compareAt(event.at, coveredThrough) <= 0;
      const durableHandoff = event.kind === "handoff" && team.handoffs.some((handoff) => handoff.to === event.agent_id && event.message === `${handoff.from} → ${handoff.to}: ${handoff.instruction}`);
      const tunnelSummary = node.tunnelId && ["tunnel_message", "tunnel_decision"].includes(event.kind);
      if (!lane || coveredSummary || durableHandoff || tunnelSummary) {
        // Detailed tool records cover an older prefix of this role's session.
        // Preserve its summaries in the record drawer, without guessing call IDs.
        records.push(node);
      } else {
        node.agentId = lane.agent.id; node.laneId = lane.id; lane.nodes.push(node);
      }
    }
    for (const channel of team.tunnels || []) for (const message of channel.messages) {
      const from = roles.get(message.from), to = roles.get(message.to);
      if (!from || !to || from === to || !channel.participants.includes(message.from) || !channel.participants.includes(message.to)) continue;
      const sentId = nodeId(team.id, "tunnel", channel.id, message.id, "send");
      const receivedId = nodeId(team.id, "tunnel", channel.id, message.id, "receive");
      const verb = tunnelMessageLabel(message);
      const base = { teamId: team.id, tunnelId: channel.id, at: message.at, kind: "tunnel" as const, action: messageAction(message), source: "tunnel" as const, detail: message.text, y: 0 };
      from.nodes.push({ ...base, id: sentId, revision: sentId, laneId: from.id, agentId: from.agent.id, tunnelOrder: 2 * message.seq,
        label: `${verb} → ${to.label}`, parentIds: [], parentKinds: {} });
      to.nodes.push({ ...base, id: receivedId, revision: receivedId, laneId: to.id, agentId: to.agent.id, tunnelOrder: 2 * message.seq + 1,
        sentNodeId: sentId, label: `${verb} ← ${from.label}`, parentIds: [], parentKinds: {} });
    }
    for (const handoff of team.handoffs) {
      const id = nodeId(team.id, "handoff", handoff.id);
      const from = roles.get(handoff.from)?.agent.name || handoff.from;
      const to = roles.get(handoff.to)?.agent.name || handoff.to;
      const node: TraceNode = { id, revision: id, teamId: team.id, agentId: handoff.to, at: handoff.at, kind: "merge", action: "handoff", source: "handoff",
        label: `交接 · ${from} → ${to}`, detail: `${handoff.instruction}\n\n来源交付：\n${handoff.summary}`, handoffFrom: handoff.from,
        parentIds: [], parentKinds: {}, y: 0 };
      const target = roles.get(handoff.to);
      if (target) { node.laneId = target.id; target.nodes.push(node); } else records.push(node);
    }
    for (const checkpoint of team.checkpoints) {
      const id = nodeId(team.id, "checkpoint", checkpoint.id);
      // capture() persists a timestamped checkpoint then its summary. Match each
      // nearby summary once, never merge distinct checkpoints by label alone.
      const summary = records.filter((node) => node.teamEventKind === "checkpoint" && node.detail === checkpoint.label &&
        compareAt(node.at, checkpoint.at) >= 0 && time(node.at) - time(checkpoint.at) < 1000).sort(byTime)[0];
      if (summary) records.splice(records.indexOf(summary), 1);
      const original = summary && team.events.find((event) => event.kind === "checkpoint" && event.at === summary.at && event.message === checkpoint.label);
      records.push({ id, revision: id, teamId: team.id, at: checkpoint.at, kind: "checkpoint", source: "checkpoint", checkpointId: checkpoint.id, files: checkpoint.files, summaryEvent: original,
        label: checkpointLabel(checkpoint.label), detail: checkpoint.files.map((file) => `${file.path} · ${file.exists ? `${file.size} bytes` : "未创建"}`).join("\n"),
        parentIds: [], parentKinds: {}, y: 0 });
    }
    arrangeRoleLanes([...roles.values()]);
    records.sort(byTime);
  }
  return { lanes, teamEvents };
}

const historyScopes = new WeakMap<Map<string, TraceHistory>, { baseline: number }>();
/** Observe all lanes when possible. A filtered-out lane returning to view gets
 * a fresh baseline; a genuinely new role can emit records after the first frame. */
export function newTraceNodes(lanes: TraceLane[], previous: Map<string, TraceHistory>): TraceNode[] {
  const added: TraceNode[] = [];
  const scope = historyScopes.get(previous);
  const visibleIds = new Set(lanes.map((lane) => lane.id));
  for (const [id, before] of previous) if (!visibleIds.has(id)) before.present = false;
  const shared = scope || { baseline: lanes.reduce((latest, lane) => lane.nodes.reduce((value, node) => Math.max(value, time(node.at)), latest), 0) || Date.now() };
  if (!scope) historyScopes.set(previous, shared);
  for (const lane of lanes) {
    const before = previous.get(lane.id);
    const latest = lane.nodes.reduce((value, node) => Math.max(value, time(node.at)), 0);
    const reappearing = before?.present === false;
    const baseline = reappearing ? Math.max(latest, before?.baseline || 0) : before?.baseline ?? before?.latest ?? shared.baseline;
    if (scope && !reappearing) for (const node of lane.nodes) if (!before?.revisions.has(node.revision) && time(node.at) > baseline && node.agentId) added.push(node);
    const revisions = before?.revisions || new Set<string>();
    for (const node of lane.nodes) revisions.add(node.revision);
    previous.set(lane.id, { revisions, latest: Math.max(latest, before?.latest || 0), baseline, present: true });
  }
  return added;
}
