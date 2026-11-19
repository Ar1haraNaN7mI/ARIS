import { useEffect, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { errorText, rpc } from "./api";
import type { Team } from "./teamTypes";

type Artifact = { path: string; text: string; sha256: string; size_bytes: number };
type Delivery = {
  agent_id: string; run_id?: string; status: string; stale: boolean; text: string; artifacts: Artifact[];
  sources: { agent_id: string; artifacts?: Artifact[] }[]; tunnel_ids: string[];
};

export function TeamDelivery({ team, token, onOpenTunnel }: { team: Team; token: string; onOpenTunnel: (id: string) => void }) {
  const [delivery, setDelivery] = useState<Delivery | null>();
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(0);
  const [path, setPath] = useState("");
  const [copied, setCopied] = useState("");
  const author = team.agents.find((agent) => agent.delivery === "final");
  useEffect(() => {
    const abort = new AbortController();
    setDelivery(undefined); setError(""); setCopied(""); setPath("");
    void rpc<{ delivery: Delivery | null }>(token, "teams/deliverable", { id: team.id }, abort.signal).then((value) => {
      if (!abort.signal.aborted) setDelivery(value.delivery);
    }).catch((err) => { if (!abort.signal.aborted) setError(errorText(err)); });
    return () => abort.abort();
  }, [team.id, team.status, author?.run_id, author?.status, token, retry]);
  if (error) return <p className="lp-delivery-notice" role="alert">{error} <button onClick={() => setRetry((n) => n + 1)}>重试</button></p>;
  if (!delivery || delivery.stale || delivery.status !== "completed") {
    const status = delivery?.status || author?.status;
    return <p className="lp-delivery-notice" role="status">{delivery === undefined ? "正在读取最终交付…" : delivery?.stale ? "依据已更新，请在汇总角色的会话中更新最终稿。" : status === "failed" || status === "cancelled" ? "这次汇总没有完成。可以打开汇总角色的会话继续。" : "等待汇总角色完成本轮交付。"}</p>;
  }
  const actors = new Map(team.agents.map((a) => [a.id, a.name]));
  const artifacts = new Map<string, Artifact & { author: string }>();
  for (const file of delivery.artifacts) artifacts.set(file.path, { ...file, author: delivery.agent_id });
  for (const source of delivery.sources) {
    for (const file of source.artifacts || []) {
      if (!artifacts.has(file.path)) artifacts.set(file.path, { ...file, author: source.agent_id });
    }
  }
  const chatDraft = !delivery.artifacts.length && !!delivery.text.trim();
  const selected = artifacts.get(path) || (chatDraft ? undefined : artifacts.values().next().value);
  const content = selected?.text ?? delivery.text;
  const byline = selected?.author || delivery.agent_id;
  const copy = async () => {
    try { await navigator.clipboard.writeText(content); setCopied("已复制"); }
    catch { setCopied("复制失败，请选中正文复制"); }
  };
  return <section className="lp-final" aria-label="最终交付" data-delivery-run={delivery.run_id}>
    <header><div><small>{actors.get(byline) || byline} · {byline === delivery.agent_id ? "最终交付" : "研究依据"}</small><h2>{selected?.path.split("/").at(-1) || "最终稿"}</h2></div>
      <button className="aw-button" onClick={() => void copy()}>{copied || "复制正文"}</button></header>
    {artifacts.size + Number(chatDraft) > 1 && <nav aria-label="交付文件">
      {chatDraft && <button aria-pressed={!selected} onClick={() => { setPath(""); setCopied(""); }}>最终稿</button>}
      {Array.from(artifacts.values(), (file) =>
      <button key={file.path} title={file.path} aria-pressed={file.path === selected?.path} onClick={() => { setPath(file.path); setCopied(""); }}>{file.path.split("/").at(-1)}</button>
    )}</nav>}
    <article className="message-body"><Markdown remarkPlugins={[remarkGfm]} components={{ a: (props) => <a {...props} target="_blank" rel="noreferrer noopener" /> }}>{content}</Markdown></article>
    {!!delivery.tunnel_ids.length && <footer>{delivery.tunnel_ids.map((id) => {
      const tunnel = team.tunnels?.find((t) => t.id === id);
      return tunnel && <button className="aw-button" key={id} onClick={() => onOpenTunnel(id)}>查看协商 · {tunnel.topic}</button>;
    })}</footer>}
  </section>;
}
