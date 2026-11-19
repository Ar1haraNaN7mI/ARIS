import { useLayoutEffect, useRef } from "react";
import Markdown from "react-markdown";
import type { Team, TeamTunnel, TunnelMessage } from "./teamTypes";
import { tunnelMessageLabel, tunnelRequestState } from "./tunnelMessages";

const tunnelStatus = { open: "通道开放", closed: "已结束", cancelled: "已取消", interrupted: "已中断" };

export function TunnelMessageCard({ tunnel, message, names, onSelectActor, onOpenTunnel }: {
  tunnel: TeamTunnel; message: TunnelMessage; names: Map<string, string>;
  onSelectActor: (id: string) => void; onOpenTunnel?: (id: string) => void;
}) {
  const state = tunnelRequestState(tunnel, message);
  const parentId = message.reply_to || message.proposal_id;
  const parent = parentId ? tunnel.messages.find((item) => item.id === parentId) : undefined;
  return <article className="lp-tunnel-message" data-side={message.from === tunnel.participants[0] ? "first" : "second"} data-tunnel-from={message.from} data-tunnel-to={message.to} data-peer-kind={message.kind}>
    <div className="lp-tunnel-byline"><strong>{tunnelMessageLabel(message)}</strong>
      <button onClick={() => onSelectActor(message.from)}>{names.get(message.from) || message.from}</button>
      <span aria-hidden="true">→</span><button onClick={() => onSelectActor(message.to)}>{names.get(message.to) || message.to}</button>
      {state && <span className="lp-peer-state">{state}</span>}
      <time dateTime={message.at}>{new Date(message.at).toLocaleTimeString()}</time></div>
    {parent && <blockquote className="lp-peer-reference" title={parent.text}>{tunnelMessageLabel(parent)} · {parent.text}</blockquote>}
    <div className="message-body"><Markdown components={{ a: (props) => <a {...props} target="_blank" rel="noreferrer noopener" /> }}>{message.text}</Markdown></div>
    {onOpenTunnel && <button type="button" className="lp-peer-topic" onClick={() => onOpenTunnel(tunnel.id)} aria-label={`打开协作记录 · ${tunnel.topic}`}>{tunnel.topic} ↗</button>}
  </article>;
}

export function TeamTunnelThread({ team, tunnel, onSelectActor, onSelectTunnel }: {
  team: Team; tunnel: TeamTunnel; onSelectActor: (id: string) => void; onSelectTunnel: (id: string) => void;
}) {
  const names = new Map(team.agents.map((agent) => [agent.id, agent.name]));
  const list = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  useLayoutEffect(() => { follow.current = true; }, [tunnel.id]);
  useLayoutEffect(() => {
    if (list.current && follow.current) list.current.scrollTop = list.current.scrollHeight;
  }, [tunnel.id, tunnel.messages.length, tunnel.status]);
  return <section className="lp-tunnel-thread" aria-label="协商全文" data-tunnel-id={tunnel.id}>
    <header><div><small>{tunnel.participants.map((id) => names.get(id) || id).join(" ⇄ ")}</small><h2>{tunnel.topic}</h2></div>
      {(team.tunnels?.length || 0) > 1 && <select aria-label="切换协商通道" value={tunnel.id} onChange={(e) => onSelectTunnel(e.target.value)}>
        {team.tunnels!.map((item) => <option key={item.id} value={item.id}>{item.topic}</option>)}
      </select>}
      <span className="lp-tunnel-status" data-open={tunnel.status === "open"}>{tunnelStatus[tunnel.status]}</span>
    </header>
    <div className="lp-tunnel-scroll" ref={list} onScroll={(e) => {
      const area = e.currentTarget; follow.current = area.scrollHeight - area.scrollTop - area.clientHeight < 80;
    }}>
      {tunnel.messages.map((message) => <TunnelMessageCard key={message.id} tunnel={tunnel} message={message} names={names} onSelectActor={onSelectActor} />)}
      {tunnel.decision && <section className="lp-tunnel-decision" aria-label="双方确认的结论"><strong>双方已确认</strong><p>{tunnel.decision.text}</p></section>}
    </div>
  </section>;
}
