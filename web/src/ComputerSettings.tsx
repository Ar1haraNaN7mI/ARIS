import { useEffect, useState } from "react";
import { ArrowRightIcon, ReloadIcon, Cross1Icon } from "@radix-ui/react-icons";
import { errorText, rpc } from "./api";

type BrowserSession = { agent_id: string; pages: { page_id: string; url: string; title: string; selected?: boolean }[] };
type Observation = { agent_id: string; page_id?: string; url?: string; title?: string; text?: string; screenshot?: { data_url?: string; path?: string }; closed?: boolean };
export function ComputerSettings({ token, agentId = "operator", onChanged }: { token: string; agentId?: string; onChanged?: () => void | Promise<void> }) {
  const [status, setStatus] = useState<{ enabled: boolean; backend?: string }>();
  const [sessions, setSessions] = useState<BrowserSession[]>([]);
  const [owner, setOwner] = useState(agentId);
  const [url, setUrl] = useState("");
  const [observation, setObservation] = useState<Observation>();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => { setOwner(agentId); setObservation(undefined); }, [agentId]);
  useEffect(() => {
    const abort = new AbortController();
    void rpc<{ enabled: boolean; backend?: string }>(token, "computer/status", {}, abort.signal).then(setStatus).catch((e) => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [token]);
  useEffect(() => {
    if (!status?.enabled) return;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try { const result = await rpc<{ sessions: BrowserSession[] }>(token, "computer/sessions", {}, abort.signal); if (!abort.signal.aborted) setSessions(result.sessions || []); }
      catch (e) { if (!abort.signal.aborted) setError(errorText(e)); }
      finally { if (!abort.signal.aborted) timer = setTimeout(() => void poll(), 2000); }
    };
    void poll(); return () => { abort.abort(); clearTimeout(timer); };
  }, [token, status?.enabled]);
  async function perform(action: () => Promise<void>) {
    if (busy) return; setBusy(true); setError("");
    try { await action(); } catch (e) { setError(errorText(e)); } finally { setBusy(false); }
  }
  async function action(name: string, params: Record<string, unknown> = {}) {
    await perform(async () => { const result = await rpc<Observation>(token, "computer/action", { agent_id: owner, action: name, ...params }); setObservation(result); });
  }
  const current = sessions.find((session) => session.agent_id === owner);
  return <div className="aris-browser">
    <div className="aris-section-heading"><span className="eyebrow">PER-AGENT BROWSER</span><h2>独立页面，独立鼠标。</h2><p>每位智能体拥有自己的浏览器上下文与页面光标，可同时工作。</p></div>
    <label className="browser-enable"><input type="checkbox" checked={status?.enabled || false} disabled={!status || busy} onChange={(e) => void perform(async () => { const next = await rpc<{ enabled: boolean; backend?: string }>(token, "computer/setEnabled", { enabled: e.target.checked }); setStatus(next); setObservation(undefined); await onChanged?.(); })} /><span>启用浏览器工具</span><small>Playwright</small></label>
    {error && <div className="aris-error" role="alert">{error}</div>}
    {status?.enabled && <>
      <label className="aris-field">智能体页面<select value={owner} onChange={(e) => { setOwner(e.target.value); setObservation(undefined); }}><option value={agentId}>{agentId === "operator" ? "操作员浏览器" : "当前智能体"}</option>{sessions.filter((s) => s.agent_id !== agentId).map((s) => <option key={s.agent_id} value={s.agent_id}>{s.agent_id}</option>)}</select></label>
      <form className="browser-url" onSubmit={(e) => { e.preventDefault(); void action(current?.pages.length ? "navigate" : "open", { url }); }}><input aria-label="浏览器网址" type="url" required placeholder="https://example.com" value={url} onChange={(e) => setUrl(e.target.value)} /><button className="solid-button" disabled={busy || !url.trim()} aria-label="打开网址"><ArrowRightIcon /></button></form>
      {!!current?.pages.length && <div className="browser-pages">{current.pages.map((page) => <button key={page.page_id} className={page.selected ? "selected" : ""} onClick={() => void action("select_page", { page_id: page.page_id })} title={page.url}>{page.title || page.url || "新页面"}</button>)}</div>}
      <div className="inline-actions browser-actions"><button className="outline-button" disabled={busy || !current?.pages.length} onClick={() => void action("observe")}><ReloadIcon />查看当前页面</button><button className="outline-button" disabled={busy || !current?.pages.length} onClick={() => void action("close")}><Cross1Icon />关闭此浏览器</button></div>
      {observation?.closed && <p className="aris-success" role="status">此智能体的浏览器已关闭。</p>}
      {observation?.screenshot?.data_url && <figure className="browser-preview"><img src={observation.screenshot.data_url} alt={observation.title || "智能体浏览器当前画面"} /><figcaption>{observation.url}</figcaption></figure>}
      {observation?.text && <details className="browser-text"><summary>页面文本</summary><pre>{observation.text}</pre></details>}
      {!current?.pages.length && !observation?.closed && <p className="subtle browser-hint">尚未打开页面。执行任务时，智能体可自行调用浏览器；也可以在此打开起始网址。</p>}
    </>}
  </div>;
}
