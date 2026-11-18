import { useEffect, useRef, type CSSProperties } from "react";
import Markdown from "react-markdown";
import { FileTextIcon, PersonIcon } from "@radix-ui/react-icons";
import { sessionTimeline } from "./AgentTranscript";
import { modelColors } from "./ProcessRelay";
import { AgentGlyph } from "./AgentGlyph";
import { sessionModelIdentities, type SessionModelIdentity } from "./sessionModelIdentity";
import { formatTime } from "./TeamConversation";
import { RunningBorder } from "./RunningBorder";
import type { ModelProfile, Team, TeamAgent } from "./teamTypes";
import type { SessionEvent, SessionResult } from "./types";

type Entry = { id: string; at: string; user?: string; queued?: boolean; agent?: TeamAgent; identity?: SessionModelIdentity; event?: SessionEvent; tool?: Extract<ReturnType<typeof sessionTimeline>[number], { kind: "tool" }>; note?: string; partial?: boolean };
export function conversationEntries(team: Team, sessions: Record<string, SessionResult>, agentId?: string, profiles: ModelProfile[] = []): Entry[] {
  const entries: Entry[] = [{ id: "goal", at: team.created_at, user: team.goal }];
  for (const message of team.messages || []) if (!agentId || !message.agent_id || message.agent_id === agentId) entries.push({ id: message.id, at: message.at, user: message.text, queued: message.queued });
  for (const agent of team.agents.filter((item) => !agentId || item.id === agentId)) {
    const events = sessions[agent.id]?.session.events || [];
    const identities = sessionModelIdentities(events, profiles);
    for (const item of sessionTimeline(events)) {
      if (item.kind === "message" && item.event.type === "assistant_message") entries.push({ id: `${agent.id}:${item.event.id}`, at: item.event.at, agent, identity: identities.get(item.event.id), event: item.event });
      if (item.kind === "tool") entries.push({ id: `${agent.id}:${item.event.id}`, at: item.event.at, agent, identity: identities.get(item.event.id), event: item.event, tool: item });
      if (item.kind === "note") entries.push({ id: `${agent.id}:${item.event.id}`, at: item.event.at, agent, identity: identities.get(item.event.id), note: item.event.text, event: item.event });
    }
    const latestStart = team.events.filter((event) => event.agent_id === agent.id && event.kind === "agent_started").at(-1)?.at;
    const latestFinish = team.events.filter((event) => event.agent_id === agent.id && event.kind === "agent_finished").at(-1)?.at;
    const hasAssistant = events.some((event) => event.type === "assistant_message" && event.text?.trim());
    const lastAssistant = events.filter((event) => event.type === "assistant_message").at(-1);
    let partial = agent.output || "";
    if (agent.status === "running" && latestStart) {
      for (const saved of events.filter((event) => event.type === "assistant_message" && event.at >= latestStart && event.text?.trim())) {
        if (partial.trimStart().startsWith(saved.text!.trim())) partial = partial.trimStart().slice(saved.text!.trim().length).trimStart();
      }
    }
    const partialAt = events.at(-1)?.at && events.at(-1)!.at > (latestStart || "") ? events.at(-1)!.at : latestStart || team.created_at;
    const currentTurn = events.filter((event) => event.type === "turn_start" && (!latestStart || event.at >= latestStart)).at(-1);
    const liveIdentity = agent.status === "running" && currentTurn ? identities.get(currentTurn.id) : undefined;
    if (partial.trim() && (agent.status === "running" ? partial !== lastAssistant?.text : !hasAssistant)) entries.push({ id: `${agent.id}:output`, at: agent.status === "running" ? partialAt : latestFinish || team.created_at, agent, identity: liveIdentity, event: { id: "output", type: "assistant_message", text: partial, at: partialAt }, partial: agent.status === "running" });
    if (agent.error) entries.push({ id: `${agent.id}:error`, at: latestFinish || latestStart || team.created_at, agent, note: agent.error });
  }
  return entries.sort((a, b) => a.at.localeCompare(b.at));
}
function Body({ text }: { text: string }) { return <div className="message-body"><Markdown components={{ a: (props) => <a {...props} target="_blank" rel="noreferrer noopener" /> }}>{text}</Markdown></div>; }
function pretty(value: unknown) { if (typeof value !== "string") return JSON.stringify(value, null, 2) || ""; try { return JSON.stringify(JSON.parse(value), null, 2); } catch { return value; } }
function writtenFile(event?: SessionEvent) {
  if (!event || !/(write|save|edit|patch)/i.test(event.name || "")) return;
  let args = event.arguments;
  if (typeof args === "string") { try { args = JSON.parse(args); } catch { return; } }
  if (!args || typeof args !== "object") return;
  const record = args as Record<string, unknown>;
  return typeof record.path === "string" ? record.path : typeof record.file_path === "string" ? record.file_path : undefined;
}
export function ProcessChat({ team, sessions, profiles, agentId, onSelectAgent }: { team: Team; sessions: Record<string, SessionResult>; profiles: ModelProfile[]; agentId?: string; onSelectAgent: (id: string) => void }) {
  const entries = conversationEntries(team, sessions, agentId, profiles);
  const scroll = useRef<HTMLDivElement>(null);
  const nearBottom = useRef(true);
  useEffect(() => { nearBottom.current = true; }, [team.id, agentId]);
  const version = entries.map((entry) => `${entry.id}:${entry.event?.text?.length}:${entry.tool?.result?.id}:${entry.queued}`).join("|");
  useEffect(() => { if (nearBottom.current && scroll.current) scroll.current.scrollTop = scroll.current.scrollHeight; }, [version, team.id, agentId]);
  return <div className="lp-chat-scroll" ref={scroll} onScroll={(event) => { const node = event.currentTarget; nearBottom.current = node.scrollHeight - node.scrollTop - node.clientHeight < 100; }}><div className="lp-chat-content">{entries.map((entry) => {
    const agent = entry.agent;
    const color = entry.identity?.family ? modelColors[entry.identity.family] : "#8ba1ad";
    const attrs = agent ? { "data-anchor-key": `message:${team.id}:${agent.id}:${entry.event?.id || entry.id}`, "data-process-team": team.id, "data-process-agent": agent.id, "data-process-event": entry.event?.id } : {};
    if (entry.user) return <article className="lp-user-message" key={entry.id}><div className="lp-message-who"><PersonIcon /><span>你</span><time>{formatTime(entry.at)}</time>{entry.queued && <small>等待下一轮</small>}</div><div className="lp-user-bubble">{entry.user}</div></article>;
    if (entry.tool) {
      const { event, result } = entry.tool;
      const live = !result && !entry.tool.turnEnded && agent?.status === "running";
      const duration = result ? Date.parse(result.at) - Date.parse(event.at) : NaN;
      const file = result?.ok === true ? writtenFile(event) : undefined;
      return <div key={entry.id} {...attrs} className="lp-process-row" style={{ "--model-color": color, "--light-color": color } as CSSProperties}><details className={`lp-tool-step ${live ? "running" : ""} ${result?.ok === false ? "failed" : ""}`}><summary>{live && <RunningBorder />}<span className={`lp-step-marker ${result?.ok === false ? "failed" : ""}`} /><strong>{event.name || result?.name || "工具调用"}</strong><span className="lp-tool-owner">{agent?.name}</span><small>{live ? "执行中" : result ? result.ok === false ? "失败" : "已完成" : "未返回"}{Number.isFinite(duration) && duration >= 0 && ` · ${duration < 1000 ? `${duration} ms` : `${(duration / 1000).toFixed(1)} s`}`}</small></summary><div className="lp-tool-body">{event.type === "tool_call" && <details open><summary>输入参数</summary><pre>{pretty(event.arguments)}</pre></details>}{result && <details open><summary>工具返回</summary><pre>{pretty(result.content)}</pre></details>}</div></details>{file && <div className="lp-delivery"><FileTextIcon /><span>产物 · <code>{file}</code></span></div>}</div>;
    }
    if (entry.note) return <details className={`lp-note ${agent?.error === entry.note ? "failed" : ""}`} key={entry.id} {...attrs}><summary>{agent?.name} · {agent?.error === entry.note ? "执行失败" : "运行记录"}<time>{formatTime(entry.at)}</time></summary><pre>{entry.note}</pre></details>;
    return <article className={`lp-assistant-message ${entry.partial ? "running" : ""}`} key={entry.id} {...attrs} style={{ "--model-color": color, "--light-color": color } as CSSProperties}>{entry.partial && <RunningBorder />}<div className="lp-message-who"><AgentGlyph family={entry.identity?.family || "unknown"} /><button title={entry.identity?.modelId || "历史模型未记录"} onClick={() => agent && onSelectAgent(agent.id)}>{agent?.name}</button>{entry.identity?.modelId && <small className="lp-message-model">{entry.identity.modelId}</small>}<time>{formatTime(entry.at)}</time>{entry.partial && <span className="lp-writing">正在回复</span>}</div>{entry.event?.reasoning && <details className="lp-reasoning"><summary>思考过程</summary><pre>{entry.event.reasoning}</pre></details>}{entry.event?.text && <Body text={entry.event.text} />}</article>;
  })}{team.agents.some((agent) => agent.status === "running" && (!agentId || agent.id === agentId)) && <div className="lp-active-line"><span />{team.agents.filter((agent) => agent.status === "running" && (!agentId || agent.id === agentId)).map((agent) => agent.name).join("、")} 正在执行</div>}</div></div>;
}
