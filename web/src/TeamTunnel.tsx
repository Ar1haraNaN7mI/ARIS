import { useLayoutEffect, useRef } from "react";
import Markdown from "react-markdown";
import type { Team, TeamTunnel } from "./teamTypes";

export const tunnelStatus = { open: "协商中", closed: "已结束", cancelled: "已取消", interrupted: "已中断" };
export const tunnelAction = { message: "发言", proposal: "提议", accept: "同意提议" };

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
      {tunnel.messages.map((message) => <article key={message.id} className="lp-tunnel-message" data-side={message.from === tunnel.participants[0] ? "first" : "second"} data-tunnel-from={message.from} data-tunnel-to={message.to}>
        <div className="lp-tunnel-byline"><button onClick={() => onSelectActor(message.from)}>{names.get(message.from) || message.from}</button>
          <span>→ {names.get(message.to) || message.to}</span><small>{tunnelAction[message.kind]}</small>
          <time dateTime={message.at}>{new Date(message.at).toLocaleTimeString()}</time></div>
        <div className="message-body"><Markdown components={{ a: (props) => <a {...props} target="_blank" rel="noreferrer noopener" /> }}>{message.text}</Markdown></div>
      </article>)}
      {tunnel.decision && <section className="lp-tunnel-decision" aria-label="双方确认的结论"><strong>双方已确认</strong><p>{tunnel.decision.text}</p></section>}
    </div>
  </section>;
}
