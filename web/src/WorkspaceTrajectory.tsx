import { useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import type { ModelProfile, Team } from "./teamTypes";
import type { SessionResult } from "./types";
import { statusLabels } from "./teamTypes";
import { conversationAgents, teamIsActive } from "./teamConversationModel";
import { AgentGlyph } from "./AgentGlyph";
import { newTraceNodes, projectWorkspaceTrace, traceLabels, type TraceKind, type TraceNode } from "./workspaceTrace";
import { createLightLayer, type LightLayer } from "./vendor/lancer-light-ui/src/index";
import "./vendor/lancer-light-ui/src/running-border.css";
import "./WorkspaceTrajectory.css";

const clock = (at?: string) => { const date = at ? new Date(at) : undefined; return date && Number.isFinite(date.getTime()) ? date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }) : "—"; };

function TraceGlyph({ kind, failed = false }: { kind: TraceKind; failed?: boolean }) {
  if (failed || kind === "error") return <span className="wt-glyph wt-error">×</span>;
  if (kind === "complete") return <span className="wt-glyph wt-complete">✓</span>;
  if (kind === "model") return <span className="wt-glyph wt-model">◇</span>;
  if (kind === "cancelled") return <span className="wt-glyph wt-cancelled">−</span>;
  return <span className={`wt-glyph wt-${kind}`} />;
}

function anchor(key: string): Element | null {
  const element = document.querySelector(`[data-anchor-key="${CSS.escape(key)}"]`);
  if (!element) return null;
  const bounds = element.getBoundingClientRect();
  if (!bounds.width || !bounds.height || getComputedStyle(element).visibility === "hidden") return null;
  const x = bounds.left + bounds.width / 2, y = bounds.top + bounds.height / 2;
  if (x < 0 || x > innerWidth || y < 0 || y > innerHeight) return null;
  for (let parent = element.parentElement; parent; parent = parent.parentElement) {
    const style = getComputedStyle(parent), rect = parent.getBoundingClientRect();
    if (/(hidden|auto|scroll|clip)/.test(style.overflowX) && (x < rect.left || x > rect.right)) return null;
    if (/(hidden|auto|scroll|clip)/.test(style.overflowY) && (y < rect.top || y > rect.bottom)) return null;
  }
  return element;
}

export function WorkspaceTrajectory({ teams, activeTeamId, selectedAgentId, profiles = [], sessions = {}, onSelectAgent }: {
  teams: Team[]; activeTeamId?: string; selectedAgentId?: string; profiles?: ModelProfile[];
  sessions?: Record<string, SessionResult>; onSelectAgent: (teamId: string, agentId: string) => void;
}) {
  const [focused, setFocused] = useState(false);
  const [detail, setDetail] = useState<TraceNode>();
  const [tip, setTip] = useState<{ node: TraceNode; x: number; y: number }>();
  const [width, setWidth] = useState(450);
  const scroll = useRef<HTMLDivElement>(null);
  const heads = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  const previousTeam = useRef(activeTeamId);
  const light = useRef<LightLayer | undefined>(undefined);
  const history = useRef(new Map<string, { revisions: Set<string>; latest: number; baseline?: number }>());
  const flights = useRef(new Map<string, { teamId: string; cancel: () => void; cleanup: ReturnType<typeof setTimeout> }>());
  const lanes = useMemo(() => projectWorkspaceTrace(teams, profiles, activeTeamId, sessions), [teams, profiles, activeTeamId, sessions]);
  const nodeIndexes = useMemo(() => new Map(lanes.map((lane) => [lane.team.id, new Map(lane.nodes.map((node) => [node.id, node]))])), [lanes]);
  const visible = focused && activeTeamId ? lanes.filter((lane) => lane.team.id === activeTeamId) : lanes;
  const minimumWidth = visible.reduce((total, lane) => total + lane.width, 0);
  const canvasWidth = Math.max(width, minimumWidth);
  const slack = visible.length ? (canvasWidth - minimumWidth) / visible.length : 0;
  const columns = visible.map((lane) => lane.width + slack);
  const offsets = columns.map((_, index) => columns.slice(0, index).reduce((sum, value) => sum + value, 0));
  const height = Math.max(280, ...visible.map((lane) => lane.height));
  const currentDetail = detail && (nodeIndexes.get(detail.teamId)?.get(detail.id) || detail);
  const lastTimestamp = lanes.reduce((latest, lane) => lane.nodes.reduce((value, node) => node.at > value ? node.at : value, latest), "");
  const grid = { gridTemplateColumns: columns.map((value) => `${value}px`).join(" "), width: canvasWidth };

  useLayoutEffect(() => {
    const element = scroll.current;
    if (!element) return;
    const measure = () => setWidth(element.clientWidth);
    measure();
    const observer = new ResizeObserver(measure); observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    light.current = createLightLayer();
    return () => {
      for (const flight of flights.current.values()) { clearTimeout(flight.cleanup); flight.cancel(); }
      flights.current.clear(); light.current?.destroy(); light.current = undefined;
    };
  }, []);

  useLayoutEffect(() => {
    if (previousTeam.current !== activeTeamId) following.current = true;
    previousTeam.current = activeTeamId;
    const area = scroll.current;
    const lane = visible.find((value) => value.team.id === activeTeamId);
    const last = lane?.nodes.at(-1);
    if (!area || !following.current || !last) return;
    if (last.y < area.scrollTop || last.y > area.scrollTop + area.clientHeight - 55) area.scrollTop = Math.max(0, last.y - area.clientHeight + 90);
    const laneIndex = visible.findIndex((value) => value.team.id === activeTeamId);
    if (laneIndex >= 0 && (offsets[laneIndex] < area.scrollLeft || offsets[laneIndex] + columns[laneIndex] > area.scrollLeft + area.clientWidth)) {
      area.scrollLeft = Math.max(0, offsets[laneIndex] - (area.clientWidth - columns[laneIndex]) / 2);
      if (heads.current) heads.current.scrollLeft = area.scrollLeft;
    }
  }, [activeTeamId, focused, lanes, canvasWidth]);

  useLayoutEffect(() => {
    for (const [id, flight] of flights.current) {
      if (teams.some((team) => team.id === flight.teamId && ["failed", "cancelled"].includes(team.status))) {
        flight.cancel(); clearTimeout(flight.cleanup); flights.current.delete(id);
      }
    }
    const added = newTraceNodes(lanes, history.current);
    const layer = light.current;
    if (!layer || document.hidden) return;
    for (const node of added.slice(-20)) {
      // Do not replay older records when a user opens a different task after
      // its sessions load. Fresh delayed polls still get their real emission.
      if (Date.now() - Date.parse(node.at) > 10_000) continue;
      const lane = visible.find((value) => value.team.id === node.teamId);
      if (!lane || ["failed", "cancelled"].includes(lane.team.status)) continue;
      const targetKey = `tree:${node.teamId}:${node.id}`;
      if (!anchor(targetKey)) continue;
      const sources: { key: string; kind: "model" | "message" }[] = [{ key: `chip:${node.teamId}:${node.agentId}`, kind: "model" }];
      const messageEventId = node.anchorEventId || node.eventId;
      if (messageEventId) sources.push({ key: `message:${node.teamId}:${node.agentId}:${messageEventId}`, kind: "message" });
      for (const source of sources) {
        if (!anchor(source.key) || layer.activeCount >= 120) continue;
        const id = `${node.teamId}:${node.revision}:${source.kind}`;
        if (flights.current.has(id)) continue;
        const flight = layer.emit({ id, source: () => anchor(source.key), target: () => anchor(targetKey), color: lane.color, sourceKind: source.kind });
        const cleanup = setTimeout(() => flights.current.delete(id), 2200);
        flights.current.set(id, { teamId: node.teamId, cancel: flight.cancel, cleanup });
      }
    }
  }, [lanes, teams, focused, canvasWidth]);

  const showTip = (node: TraceNode, element: HTMLElement) => {
    const bounds = element.getBoundingClientRect();
    const panel = scroll.current?.getBoundingClientRect();
    setTip({ node, x: Math.max(8, (panel?.left || bounds.left) - 248), y: Math.max(48, Math.min(innerHeight - 170, bounds.top - 12)) });
  };
  const choose = (node: TraceNode) => { setDetail(node); if (node.agentId) onSelectAgent(node.teamId, node.agentId); };
  const legend: TraceKind[] = ["reply", "checkpoint", "start", "merge", "artifact", "tool", "model", "wait", "complete", "error", "cancelled", "message"];
  return <aside className="lp-trajectory wt-panel" aria-label="工作树与泳道">
    <header className="wt-heading"><strong>工作树 · 泳道</strong><span>{teams.length} 条任务线 · {teams.filter(teamIsActive).length} 运行中</span></header>
    <div className="wt-heads-scroll" ref={heads}><div className="wt-lane-heads" style={grid}>
      {visible.map((lane) => <button type="button" key={lane.team.id} className="wt-lane-head" data-current={activeTeamId === lane.team.id} style={{ "--lc": lane.color } as CSSProperties} onClick={() => lane.currentAgent && onSelectAgent(lane.team.id, lane.currentAgent.id)} title={lane.team.name}>
        <span className="wt-lane-name"><i />{lane.team.name}</span>
        <span className="wt-lane-model"><AgentGlyph profileId={lane.currentAgent?.profile_id} profiles={profiles} size={11} /><span>{lane.model}</span></span>
        <span className="wt-progress" aria-label={`${lane.completed} / ${lane.total} 阶段已完成`}><i style={{ width: `${lane.total ? lane.completed / lane.total * 100 : 0}%` }} /></span>
        <span className="wt-lane-state">{lane.total ? `${lane.completed}/${lane.total} · ` : "对话 · "}{statusLabels[lane.team.status]}</span>
      </button>)}
    </div></div>
    <div className="wt-filter"><button type="button" className={focused ? "selected" : ""} aria-pressed={focused} disabled={!activeTeamId} onClick={() => setFocused(true)}>聚焦</button><button type="button" className={!focused ? "selected" : ""} aria-pressed={!focused} onClick={() => setFocused(false)}>全部泳道</button><time>{clock(lastTimestamp)}</time></div>
    <div className="wt-scroll" ref={scroll} onScroll={(event) => { const area = event.currentTarget; if (heads.current) heads.current.scrollLeft = area.scrollLeft; const activeLast = visible.find((lane) => lane.team.id === activeTeamId)?.nodes.at(-1)?.y; following.current = (activeLast ?? area.scrollHeight) - area.clientHeight - area.scrollTop < 60; setTip(undefined); }}>
      {!visible.length ? <p className="wt-empty">创建任务后，实际执行记录会沿各自泳道展开。</p> : <div className="wt-tree-space" style={{ width: canvasWidth, height }}>
        <svg className="wt-rails" width={canvasWidth} height={height} aria-hidden="true">
          {visible.map((lane, index) => <g key={lane.team.id} stroke={lane.color} fill="none" strokeWidth="1.6">
            <path className="wt-lane-divider" d={`M${offsets[index] + columns[index]} 0 V${height}`} />
            {lane.nodes.flatMap((node) => node.parentIds.map((parentId) => {
              const parent = nodeIndexes.get(lane.team.id)?.get(parentId); if (!parent) return null;
              const x1 = offsets[index] + 17 + parent.branch * 20, x2 = offsets[index] + 17 + node.branch * 20;
              const mid = (parent.y + node.y) / 2;
              return <path key={`${node.id}:${parent.id}`} d={x1 === x2 ? `M${x1} ${parent.y} V${node.y}` : `M${x1} ${parent.y} C${x1} ${mid},${x2} ${mid},${x2} ${node.y}`} opacity={node.kind === "tool" ? .55 : .8} data-trace-parent={parentId} />;
            }))}
            {lane.nodes.length > 0 && <path d={`M${offsets[index] + 17 + lane.nodes.at(-1)!.branch * 20} ${lane.nodes.at(-1)!.y + 9} V${height - 18}`} strokeDasharray="2 5" opacity=".25" />}
          </g>)}
        </svg>
        {visible.flatMap((lane, index) => lane.nodes.map((node) => {
          const x = offsets[index] + 17 + node.branch * 20;
          const agent = conversationAgents(lane.team).find((value) => value.id === node.agentId);
          return <button type="button" key={`${lane.team.id}:${node.id}`} className="wt-node" data-selected={currentDetail?.teamId === node.teamId && currentDetail.id === node.id} data-agent-current={activeTeamId === lane.team.id && selectedAgentId === node.agentId} data-trace-kind={node.kind} style={{ left: x - 9, top: node.y - 10, width: offsets[index] + columns[index] - x + 2, "--lc": lane.color } as CSSProperties} aria-label={`${traceLabels[node.kind]} · ${node.label}`} onClick={() => choose(node)} onMouseEnter={(event) => showTip(node, event.currentTarget)} onMouseLeave={() => setTip(undefined)} onFocus={(event) => showTip(node, event.currentTarget)} onBlur={() => setTip(undefined)}>
            <span className="wt-marker" data-anchor-key={`tree:${node.teamId}:${node.id}`} data-tree-anchor={node.id} data-event-lane={lane.team.id} data-event-type={node.kind}><TraceGlyph kind={node.kind} failed={node.ok === false} /></span>
            <span className="wt-node-label"><span>{node.label}</span><small>{node.identity?.modelId || node.identity?.profileId || agent?.name || "工作区"} · {clock(node.at)}</small></span>
          </button>;
        }))}
      </div>}
    </div>
    {currentDetail && <section className="wt-detail" aria-label="节点详情"><header><strong>{traceLabels[currentDetail.kind]} · {currentDetail.label}</strong><button type="button" onClick={() => setDetail(undefined)} aria-label="关闭节点详情">×</button></header><p>{clock(currentDetail.at)}{currentDetail.durationMs !== undefined ? ` · ${(currentDetail.durationMs / 1000).toFixed(1)} 秒` : ""}{currentDetail.ok === false ? " · 工具返回失败" : ""}</p><pre>{currentDetail.detail}</pre>{currentDetail.arguments !== undefined && <details><summary>输入参数</summary><pre>{JSON.stringify(currentDetail.arguments, null, 2)}</pre></details>}{currentDetail.result !== undefined && <details><summary>实际返回</summary><pre>{currentDetail.result}</pre></details>}{currentDetail.agentId && <button type="button" className="wt-open-session" onClick={() => onSelectAgent(currentDetail.teamId, currentDetail.agentId!)}>打开这位智能体的会话 ↗</button>}</section>}
    <footer className="wt-legend"><strong>图例 · 真实事件</strong>{legend.map((kind) => <span key={kind}><span className="wt-legend-mark"><TraceGlyph kind={kind} /></span>{traceLabels[kind]}</span>)}</footer>
    {tip && <aside className="wt-tooltip" role="tooltip" style={{ left: tip.x, top: tip.y }}><strong>{traceLabels[tip.node.kind]} · {tip.node.label}</strong><small>{clock(tip.node.at)} · {tip.node.source === "session" ? "会话记录" : tip.node.source === "checkpoint" ? "保存的检查点" : "工作流记录"}</small><p>{tip.node.detail.slice(0, 260)}</p></aside>}
  </aside>;
}
