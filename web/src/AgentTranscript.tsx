import { useMemo } from "react";
import Markdown from "react-markdown";
import type { SessionEvent } from "./types";
import type { Team, TeamAgent } from "./teamTypes";
import { useAgentSession } from "./useAgentSession";

type TimelineItem =
  | { kind: "message" | "note"; event: SessionEvent }
  | { kind: "tool"; event: SessionEvent; result?: SessionEvent; turnEnded: boolean };

// Providers can reuse call IDs in later turns. Pair each result with a pending
// call in its own turn instead of replacing history with the latest result.
export function sessionTimeline(events: SessionEvent[]): TimelineItem[] {
  const items: TimelineItem[] = [];
  const pending = new Map<string, Extract<TimelineItem, { kind: "tool" }>>();
  const finishTurn = () => { for (const tool of pending.values()) tool.turnEnded = true; pending.clear(); };
  for (const event of events) {
    if (event.type === "turn_start" || event.type === "turn_end") { finishTurn(); continue; }
    if (event.type === "user_message" || event.type === "assistant_message") {
      items.push({ kind: "message", event });
    } else if (event.type === "tool_call") {
      const item: Extract<TimelineItem, { kind: "tool" }> = { kind: "tool", event, turnEnded: false };
      items.push(item);
      if (event.call_id) pending.set(event.call_id, item);
    } else if (event.type === "tool_result") {
      const item = event.call_id ? pending.get(event.call_id) : undefined;
      if (item) { item.result = event; pending.delete(event.call_id!); }
      else items.push({ kind: "tool", event, result: event, turnEnded: true });
    } else if (event.type === "system_note") items.push({ kind: "note", event });
  }
  return items;
}

function Timestamp({ at }: { at?: string }) {
  if (!at) return null;
  const date = new Date(at);
  if (!Number.isFinite(date.getTime())) return null;
  return <time dateTime={at} title={date.toLocaleString()}>{date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}</time>;
}

function contentText(value: unknown): string {
  if (typeof value !== "string") return value === undefined ? "" : JSON.stringify(value, null, 2);
  try { return JSON.stringify(JSON.parse(value), null, 2); } catch { return value; }
}

function MessageBody({ text }: { text: string }) {
  return <div className="message-body"><Markdown components={{ a: (props) => <a {...props} target="_blank" rel="noreferrer noopener" /> }}>{text}</Markdown></div>;
}

function ToolRecord({ item, running }: { item: Extract<TimelineItem, { kind: "tool" }>; running: boolean }) {
  const { event, result } = item;
  const state = result ? result.ok === true ? "completed" : result.ok === false ? "failed" : "unknown" : running && !item.turnEnded ? "running" : "unknown";
  const label = { completed: "已完成", failed: "失败", running: "执行中", unknown: result ? "已返回" : "未收到结果" }[state];
  const duration = result && event.type === "tool_call" ? new Date(result.at).getTime() - new Date(event.at).getTime() : NaN;
  return <details className={`agent-tool ${state}`}>
    <summary><span className={`team-status ${state}`} aria-hidden="true" /><strong>{event.name || result?.name || "工具调用"}</strong><span className="agent-tool-state">{label}</span><Timestamp at={event.at} />{Number.isFinite(duration) && duration >= 0 && <small>{duration < 1000 ? `${duration} ms` : `${(duration / 1000).toFixed(1)} s`}</small>}</summary>
    {event.call_id && <div className="agent-tool-id">调用标识 <code>{event.call_id}</code></div>}
    {event.type === "tool_call" && <details className="agent-tool-block" open><summary>输入参数</summary><pre>{contentText(event.arguments) || "无参数"}</pre></details>}
    {result ? <details className="agent-tool-block" open><summary>{result.ok === false ? "错误与返回结果" : "返回结果"}<Timestamp at={result.at} /></summary><pre>{contentText(result.content) || "工具返回了空内容。"}</pre></details> : <p className="agent-tool-pending">{state === "running" ? "等待工具返回实际结果。" : "会话未记录该调用的返回结果。"}</p>}
  </details>;
}

function instructionAt(event: SessionEvent, messages: Team["messages"], goal?: string): string {
  const eventTime = Date.parse(event.at);
  let latestTime = -Infinity;
  let text = goal || "";
  for (const message of messages || []) {
    const messageTime = Date.parse(message.at);
    if (Number.isFinite(messageTime) && messageTime <= eventTime && messageTime >= latestTime) {
      latestTime = messageTime;
      text = message.text;
    }
  }
  return text;
}

export function SessionTimeline({ events, agentName, running = false, messages, goal }: {
  events: SessionEvent[]; agentName: string; running?: boolean; messages?: Team["messages"]; goal?: string;
}) {
  const items = useMemo(() => sessionTimeline(events), [events]);
  return <>{items.map((item) => {
    const { event } = item;
    if (item.kind === "tool") return <ToolRecord key={event.id} item={item} running={running} />;
    if (item.kind === "note") return <details className="agent-tool agent-note" key={event.id}><summary>运行记录<Timestamp at={event.at} /></summary><pre>{event.text}</pre></details>;
    const user = event.type === "user_message";
    const instruction = user ? instructionAt(event, messages, goal) : "";
    return <article className={`agent-message ${event.type}`} key={event.id}>
      <header className="agent-message-header"><span>{user ? "你" : agentName}</span><Timestamp at={event.at} /></header>
      {instruction.trim() && <MessageBody text={instruction} />}
      {!user && event.reasoning?.trim() && <details className="reasoning agent-reasoning"><summary>模型推理记录</summary><pre>{event.reasoning}</pre></details>}
      {event.text?.trim() && (user ? <details className="agent-context"><summary>查看完整任务与用户上下文</summary><MessageBody text={event.text} /></details> : <MessageBody text={event.text} />)}
    </article>;
  })}</>;
}

export function AgentTranscript({ token, agent, messages, goal }: {
  token: string; agent: TeamAgent; messages?: Team["messages"]; goal?: string;
}) {
  const { snapshot, error, loading, refresh } = useAgentSession(token, agent.session_id);
  const events = snapshot?.session.events || [];
  const hasAssistant = events.some((event) => event.type === "assistant_message" && event.text?.trim());
  return <div className="agent-transcript" aria-busy={loading}>
    <details className="agent-assignment"><summary>任务说明 · {agent.role || agent.name}</summary><p>{agent.prompt}</p>{agent.files.length > 0 && <div className="agent-file-chips">{agent.files.map((file) => <code key={file}>{file}</code>)}</div>}</details>
    {error && <div className="aris-error" role="alert">读取会话失败：{error}<button type="button" onClick={refresh}>重试</button></div>}
    <SessionTimeline events={events} agentName={agent.name} running={agent.status === "running"} messages={messages} goal={goal} />
    {((!hasAssistant && agent.output) || (agent.status === "running" && agent.output)) && <article className="agent-message assistant_message" data-live-message="true"><header className="agent-message-header"><span>{agent.name}</span><small>{agent.status === "running" ? "正在回复" : "已保存的交付结果"}</small></header><MessageBody text={agent.output!} /></article>}
    {agent.error && <div className="aris-error" role="alert">{agent.error}</div>}
    {!events.length && !agent.output && !error && <div className="agent-empty"><span className={`team-status ${agent.status}`} /><p>{loading ? "正在读取会话记录…" : agent.status === "running" ? "智能体正在执行，已记录的工具调用与回复会显示在这里。" : "执行后，这里会显示这位智能体的会话与交付结果。"}</p></div>}
  </div>;
}
