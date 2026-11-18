import { useRef, useState, type ReactNode } from "react";
import { ArrowRightIcon, ArrowUpIcon, ChatBubbleIcon, CheckIcon, CodeIcon } from "@radix-ui/react-icons";
import { type Team, type TeamAgent } from "./teamTypes";
import type { SessionResult } from "./types";

export function MessageComposer({ agent, agents, files = [], running, busy, onSend, onTargetChange, toolbar, onDraftChange, newChat = false, initialText = "" }: { agent?: TeamAgent; agents?: TeamAgent[]; files?: string[]; running: boolean; busy: boolean; onSend: (text: string) => Promise<unknown>; onTargetChange?: (id?: string) => void; toolbar?: ReactNode; onDraftChange?: (text: string) => void; newChat?: boolean; initialText?: string }) {
  const [text, setText] = useState(initialText);
  const [filesOpen, setFilesOpen] = useState(false);
  const [sending, setSending] = useState(false);
  const input = useRef<HTMLTextAreaElement>(null);
  const changeText = (next: string) => { setText(next); onDraftChange?.(next); };
  const submit = async () => {
    if (!text.trim() || busy || sending) return;
    setFilesOpen(false);
    setSending(true);
    try { if (await onSend(text.trim())) changeText(""); } finally { setSending(false); }
  };
  const insertFile = (path: string) => {
    const start = input.current?.selectionStart ?? text.length;
    const end = input.current?.selectionEnd ?? start;
    const before = text.slice(0, start).replace(/@[^\s@]*$/, "");
    changeText(`${before}@${path} ${text.slice(end)}`); setFilesOpen(false); input.current?.focus();
  };
  return <form className="aw-compose" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
    <textarea ref={input} aria-label={newChat ? "新消息" : agent ? `发送给 ${agent.name}` : "发送给整个团队"} value={text} onChange={(event) => { changeText(event.target.value); setFilesOpen(/@[^\s@]*$/.test(event.target.value.slice(0, event.target.selectionStart))); }} onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); void submit(); } if (event.key === "Escape") setFilesOpen(false); }} placeholder={newChat ? "说说你想完成什么…" : agent ? `继续和 ${agent.name} 讨论…` : "继续讨论，或提出新的要求…"} rows={2} disabled={busy || sending} />
    {filesOpen && <div className="lp-file-picker" role="listbox" aria-label="引用共享文件">{files.length ? files.map((path) => <button type="button" role="option" aria-selected={false} key={path} onClick={() => insertFile(path)}>{path}</button>) : <span>当前会话暂无共享文件</span>}</div>}
    <div className="aw-compose-footer"><span className="lp-key-hint">↵ 发送</span><button type="button" className="lp-compose-action" onClick={() => setFilesOpen((value) => !value)}>@ 引用文件</button>{agents && onTargetChange && <select aria-label="消息接收者" value={agent?.id || ""} onChange={(event) => onTargetChange(event.target.value || undefined)}><option value="">整个团队</option>{agents.map((member) => <option key={member.id} value={member.id}>{member.name}</option>)}</select>}{toolbar}{running && <span className="lp-queue-hint">发送后排队</span>}<button type="submit" className="lp-send" aria-label="发送并继续执行" disabled={busy || sending || !text.trim()}><ArrowUpIcon /></button></div>
  </form>;
}

export function TeamTimeline({ team, sessions, onSelectAgent }: { team: Team; sessions: Record<string, SessionResult>; onSelectAgent: (id: string) => void }) {
  const [filter, setFilter] = useState("all");
  const entries = [
    ...team.events.map((event, i) => ({ key: `team-${i}`, at: event.at, agentId: event.agent_id, type: "flow", title: event.message, body: "", failed: event.kind.includes("error"), kind: event.kind })),
    ...team.agents.flatMap((agent) => (sessions[agent.id]?.session.events || []).filter((event) => ["user_message", "assistant_message", "tool_call", "tool_result", "reasoning"].includes(event.type)).map((event) => ({ key: `${agent.id}-${event.id}`, at: event.at, agentId: agent.id, type: event.type.startsWith("tool_") ? "tool" : "message", title: event.type === "tool_call" ? `调用 ${event.name}` : event.type === "tool_result" ? `工具返回${event.ok === false ? " · 失败" : ""}` : event.type === "user_message" ? "接收指令与上下文" : event.type === "reasoning" ? "推理记录" : "智能体回复", body: event.text || event.content || event.reasoning || (event.arguments ? JSON.stringify(event.arguments, null, 2) : ""), failed: event.ok === false, kind: event.type }))),
  ].sort((a, b) => a.at.localeCompare(b.at));
  const visible = entries.filter((event) => filter === "all" || event.type === filter);
  return <section className="aw-timeline-view"><div className="aw-timeline-heading"><div><h2>完整执行记录</h2><p>团队调度、每位智能体的消息与工具结果，按实际发生时间展示。</p></div><span>{entries.length} 条记录</span></div><div className="aw-filter-tabs" role="tablist" aria-label="记录类型">{[["all", "全部"], ["flow", "流程"], ["message", "消息"], ["tool", "工具"]].map(([id, name]) => <button key={id} role="tab" aria-selected={filter === id} onClick={() => setFilter(id)}>{name}</button>)}</div><ol className="aw-timeline">{visible.map((entry) => <li key={entry.key} className={entry.failed ? "failed" : ""}><span className="aw-timeline-icon">{entry.type === "tool" ? <CodeIcon /> : entry.type === "message" ? <ChatBubbleIcon /> : <CheckIcon />}</span><div className="aw-timeline-item"><div><strong>{entry.title}</strong><time>{formatTime(entry.at)}</time></div>{entry.agentId && <button className="aw-text-link" onClick={() => onSelectAgent(entry.agentId!)}>{team.agents.find((agent) => agent.id === entry.agentId)?.name || entry.agentId}<ArrowRightIcon /></button>}{entry.body && <details><summary>查看{entry.type === "tool" ? "参数与结果" : "消息内容"}</summary><pre>{entry.body}</pre></details>}</div></li>)}</ol>{!visible.length && <p className="aw-empty">此类型尚无记录。</p>}</section>;
}
export function formatTime(at: string) { return new Date(at).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit" }); }
export function flowEventLabel(event: Team["events"][number]) {
  if (event.kind === "created") return "建立共享工作区";
  if (event.kind === "started") return "开始一轮协作，按依赖调度子流程";
  if (event.kind === "agent_started") return "开始执行";
  if (event.kind === "agent_finished") return event.message.includes("completed") ? "执行完成，已交付结果" : event.message.includes("cancelled") ? "已停止执行" : "执行失败";
  if (event.kind === "finished") return event.message.includes("All agents completed") ? "所有子流程已完成" : "流程因错误停止";
  if (event.kind === "cancelling") return "正在停止，等待当前工具结束";
  if (event.kind === "cancelled") return "所有运行中的智能体已停止";
  if (event.kind === "checkpoint") return ({ "Before run": "保存执行前快照", "After run": "保存执行后快照", "Before restore (undo)": "保存恢复前备份" } as Record<string, string>)[event.message] || `保存快照 · ${event.message}`;
  return event.message;
}
