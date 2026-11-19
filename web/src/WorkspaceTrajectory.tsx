import { useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import type { ModelProfile, Team } from "./teamTypes";
import type { SessionResult } from "./types";
import { statusLabels } from "./teamTypes";
import { AgentGlyph } from "./AgentGlyph";
import { newTraceNodes, projectWorkspaceTrace, traceLabels, type TraceHistory, type TraceKind, type TraceNode } from "./workspaceTrace";
import { createLightLayer, type LightLayer } from "./vendor/lancer-light-ui/src/index";
import "./vendor/lancer-light-ui/src/running-border.css";
import "./WorkspaceTrajectory.css";

const clock = (at?: string) => { const date = at ? new Date(at) : undefined; return date && Number.isFinite(date.getTime()) ? date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }) : "—"; };

function eventLabel(node: TraceNode) {
  if (node.teamEventKind === "admin_delegated") return "分工";
  if (node.source === "handoff" || node.teamEventKind === "handoff") return "交接";
  return traceLabels[node.kind];
}

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
  sessions?: Record<string, SessionResult>; onSelectAgent: (teamId: string, agentId?: string) => void;
}) {
  const [scope, setScope] = useState<"team" | "all">("team");
  const focused = scope === "team" && !!activeTeamId;
  const [showTeamEvents, setShowTeamEvents] = useState(false);
  const [detail, setDetail] = useState<TraceNode>();
  const [tip, setTip] = useState<{ node: TraceNode; x: number; y: number }>();
  const [width, setWidth] = useState(450);
  const scroll = useRef<HTMLDivElement>(null);
  const heads = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  const previousView = useRef("");
  const light = useRef<LightLayer | undefined>(undefined);
  const history = useRef(new Map<string, TraceHistory>());
  const flights = useRef(new Map<string, { teamId: string; cancel: () => void; cleanup: ReturnType<typeof setTimeout> }>());
  const { lanes, teamEvents } = useMemo(() => projectWorkspaceTrace(teams, profiles, activeTeamId, sessions), [teams, profiles, activeTeamId, sessions]);
  const visible = useMemo(() => focused ? lanes.filter((lane) => lane.team.id === activeTeamId) : lanes, [lanes, focused, activeTeamId]);
  const minimumWidth = visible.reduce((total, lane) => total + lane.width, 0);
  const canvasWidth = Math.max(width, minimumWidth);
  const { columns, positions } = useMemo(() => {
    const extra = visible.length ? (canvasWidth - minimumWidth) / visible.length : 0;
    const positions = new Map<string, { node: TraceNode; laneId: string; x: number }>();
    let left = 0;
    const columns = visible.map((lane) => {
      const column = { lane, left, width: lane.width + extra, rail: left + 15 };
      for (const node of lane.nodes) positions.set(node.id, { node, laneId: lane.id, x: column.rail });
      left += column.width;
      return column;
    });
    return { columns, positions };
  }, [visible, minimumWidth, canvasWidth]);
  const height = visible.reduce((bottom, lane) => Math.max(bottom, lane.height), 280);
  const currentDetail = detail && detail.teamId === activeTeamId && detail.agentId === selectedAgentId
    ? positions.get(detail.id)?.node : undefined;
  const visibleTeams = teams.filter((team) => !focused || team.id === activeTeamId);
  const recordCount = visibleTeams.reduce((count, team) => count + (teamEvents.get(team.id)?.length || 0), 0);
  const lastTimestamp = visibleTeams.reduce((latest, team) => {
    const at = teamEvents.get(team.id)?.at(-1)?.at || "";
    return at > latest ? at : latest;
  }, visible.reduce((latest, lane) => {
    const at = lane.nodes.at(-1)?.at || ""; return at > latest ? at : latest;
  }, ""));
  const grid = { gridTemplateColumns: columns.map((column) => `${column.width}px`).join(" "), width: canvasWidth };
  const followLanes = visible.filter((lane) => lane.team.id === activeTeamId && (!selectedAgentId || lane.agent.id === selectedAgentId));
  const followY = followLanes.reduce<number | undefined>((bottom, lane) => {
    const y = lane.nodes.at(-1)?.y;
    return y === undefined ? bottom : Math.max(bottom ?? 0, y);
  }, undefined);
  const targetColumn = columns.find(({ lane }) => lane.team.id === activeTeamId && (!selectedAgentId || lane.agent.id === selectedAgentId));

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
    const area = scroll.current;
    if (!area) return;
    const view = JSON.stringify([activeTeamId, selectedAgentId, focused, targetColumn?.lane.id]);
    if (previousView.current !== view) {
      previousView.current = view;
      following.current = true;
      if (targetColumn && (targetColumn.left < area.scrollLeft || targetColumn.left + targetColumn.width > area.scrollLeft + area.clientWidth)) {
        area.scrollLeft = Math.max(0, targetColumn.left - (area.clientWidth - targetColumn.width) / 2);
      }
      if (heads.current) heads.current.scrollLeft = area.scrollLeft;
    }
    if (!currentDetail && following.current && followY !== undefined && (followY < area.scrollTop || followY > area.scrollTop + area.clientHeight - 55)) {
      area.scrollTop = Math.max(0, followY - area.clientHeight + 90);
    }
  }, [activeTeamId, selectedAgentId, focused, targetColumn, followY, currentDetail]);

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
      const lane = visible.find((value) => value.id === node.laneId);
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
  const choose = (node: TraceNode) => { setShowTeamEvents(false); setDetail(node); if (node.agentId) onSelectAgent(node.teamId, node.agentId); };
  const legend: TraceKind[] = ["reply", "checkpoint", "start", "merge", "artifact", "tool", "model", "wait", "complete", "error", "cancelled", "message"];
  return <aside className="lp-trajectory wt-panel" aria-label="工作树与泳道">
    <header className="wt-heading"><strong>工作树 · 泳道</strong><span>{visible.length} 个角色 · {visible.filter((lane) => lane.agent.status === "running").length} 运行中</span></header>
    <div className="wt-heads-scroll" ref={heads} style={{ maxWidth: width || undefined }} onScroll={(event) => {
      if (scroll.current && scroll.current.scrollLeft !== event.currentTarget.scrollLeft) scroll.current.scrollLeft = event.currentTarget.scrollLeft;
    }}><div className="wt-lane-heads" style={grid}>
      {visible.map((lane) => {
        const selected = activeTeamId === lane.team.id && selectedAgentId === lane.agent.id;
        const name = lane.kind === "admin" ? `${lane.agent.name} · 管理员` : lane.agent.name;
        return <button type="button" key={lane.id} className="wt-lane-head" data-lane-id={lane.id} data-lane-team={lane.team.id} data-lane-agent={lane.agent.id} data-current={selected} data-status={lane.agent.status}
          data-subagent-id={lane.agent.subagent_id} aria-pressed={selected} title={`${lane.team.name} · ${name} · ${lane.model}`} style={{ "--lc": lane.color } as CSSProperties} onClick={() => { setDetail(undefined); setShowTeamEvents(false); onSelectAgent(lane.team.id, lane.agent.id); }}>
          {!focused && visibleTeams.length > 1 && <small className="wt-team-name">{lane.team.name}</small>}
          <span className="wt-lane-name"><i /><span>{name}</span></span>
          <span className="wt-lane-model"><AgentGlyph profileId={lane.agent.profile_id} profiles={profiles} size={11} /><span>{lane.model}</span></span>
          <span className="wt-lane-state">{statusLabels[lane.agent.status]}</span>
        </button>;
      })}
    </div></div>
    <div className="wt-filter">
      <button type="button" className={focused ? "selected" : ""} aria-pressed={focused} disabled={!activeTeamId} onClick={() => setScope("team")}>当前任务</button>
      <button type="button" className={!focused ? "selected" : ""} aria-pressed={!focused} onClick={() => setScope("all")}>全部泳道</button>
      <button type="button" className="wt-records-toggle" aria-expanded={showTeamEvents} disabled={!recordCount} onClick={() => { setShowTeamEvents(!showTeamEvents); setDetail(undefined); }}>团队记录</button>
      <time>{clock(lastTimestamp)}</time>
    </div>
    <div className="wt-scroll" ref={scroll} onScroll={(event) => {
      const area = event.currentTarget;
      if (heads.current && heads.current.scrollLeft !== area.scrollLeft) heads.current.scrollLeft = area.scrollLeft;
      following.current = (followY ?? area.scrollHeight) - area.clientHeight - area.scrollTop < 60;
      setTip(undefined);
    }}>
      {!visible.length ? <p className="wt-empty">创建角色后，会在这里显示各自的泳道。</p> : <div className="wt-tree-space" style={{ width: canvasWidth, height }}>
        <svg className="wt-rails" width={canvasWidth} height={height} aria-hidden="true">
          {columns.map(({ lane, left, width: columnWidth, rail }) => <g key={lane.id} stroke={lane.color} fill="none" strokeWidth="1.6">
            <path className="wt-lane-divider" d={`M${left + columnWidth} 0 V${height}`} />
            {lane.nodes.flatMap((node) => node.parentIds.map((parentId) => {
              const parent = positions.get(parentId);
              if (!parent) return null;
              const mid = (parent.node.y + node.y) / 2;
              const path = parent.x === rail
                ? `M${rail} ${parent.node.y} V${node.y}`
                : `M${parent.x} ${parent.node.y} C${parent.x} ${mid},${rail} ${mid},${rail} ${node.y}`;
              const kind = node.parentKinds[parentId];
              return <path key={JSON.stringify([parentId, node.id])} d={path} opacity={node.kind === "tool" ? .55 : .8}
                strokeDasharray={kind === "handoff" ? "5 3" : undefined} data-edge-kind={kind}
                data-trace-parent={parentId} data-trace-child={node.id} data-edge-from-lane={parent.laneId} data-edge-to-lane={lane.id} />;
            }))}
            <path d={`M${rail} ${(lane.nodes.at(-1)?.y ?? 0) + 9} V${height - 18}`} strokeDasharray="2 5" opacity=".25" />
          </g>)}
        </svg>
        {columns.flatMap(({ lane, left, width: columnWidth, rail }) => lane.nodes.map((node) =>
          <button type="button" key={node.id} className="wt-node" data-selected={currentDetail?.id === node.id}
            data-agent-current={activeTeamId === node.teamId && selectedAgentId === lane.agent.id} data-trace-kind={node.kind}
            style={{ left: rail - 9, top: node.y - 10, width: left + columnWidth - rail + 2, "--lc": lane.color } as CSSProperties}
            aria-label={`${lane.agent.name} · ${eventLabel(node)} · ${node.label}`} onClick={() => choose(node)} onMouseEnter={(event) => showTip(node, event.currentTarget)} onMouseLeave={() => setTip(undefined)} onFocus={(event) => showTip(node, event.currentTarget)} onBlur={() => setTip(undefined)}>
            <span className="wt-marker" data-anchor-key={`tree:${node.teamId}:${node.id}`} data-tree-anchor={node.id} data-event-lane={lane.id} data-event-type={node.kind}><TraceGlyph kind={node.kind} failed={node.ok === false} /></span>
            <span className="wt-node-label"><span>{node.label}</span><small>{node.identity?.modelId || node.identity?.profileId || lane.agent.name} · {clock(node.at)}</small></span>
          </button>))}
      </div>}
    </div>
    {showTeamEvents && <section className="wt-detail wt-records" aria-label="团队记录"><header><strong>团队记录</strong><button type="button" aria-label="关闭团队记录" onClick={() => setShowTeamEvents(false)}>×</button></header>
      {visibleTeams.map((team) => {
        const records = teamEvents.get(team.id);
        if (!records?.length) return null;
        return <div key={team.id}>{visibleTeams.length > 1 && <strong>{team.name}</strong>}
          {records.slice().reverse().map((node) => <details key={node.id}><summary>{clock(node.at)} · {node.label}</summary><pre>{node.detail}</pre></details>)}
        </div>;
      })}
    </section>}
    {currentDetail && <section className="wt-detail" aria-label="节点详情"><header><strong>{eventLabel(currentDetail)} · {currentDetail.label}</strong><button type="button" onClick={() => setDetail(undefined)} aria-label="关闭节点详情">×</button></header><p>{clock(currentDetail.at)}{currentDetail.durationMs !== undefined ? ` · ${(currentDetail.durationMs / 1000).toFixed(1)} 秒` : ""}{currentDetail.ok === false ? " · 工具返回失败" : ""}</p><pre>{currentDetail.detail}</pre>{currentDetail.arguments !== undefined && <details><summary>输入参数</summary><pre>{JSON.stringify(currentDetail.arguments, null, 2)}</pre></details>}{currentDetail.result !== undefined && <details><summary>实际返回</summary><pre>{currentDetail.result}</pre></details>}{currentDetail.agentId && <button type="button" className="wt-open-session" onClick={() => onSelectAgent(currentDetail.teamId, currentDetail.agentId!)}>打开这位智能体的会话 ↗</button>}</section>}
    <footer className="wt-legend"><strong>图例 · 真实事件</strong>{legend.map((kind) => <span key={kind}><span className="wt-legend-mark"><TraceGlyph kind={kind} /></span>{traceLabels[kind]}</span>)}</footer>
    {tip && <aside className="wt-tooltip" role="tooltip" style={{ left: tip.x, top: tip.y }}><strong>{eventLabel(tip.node)} · {tip.node.label}</strong><small>{clock(tip.node.at)} · {tip.node.source === "session" ? "会话记录" : tip.node.source === "checkpoint" ? "保存的检查点" : "工作流记录"}</small><p>{tip.node.detail.slice(0, 260)}</p></aside>}
  </aside>;
}
