import { useEffect, useState } from "react";
import { PlusIcon, TrashIcon, ReloadIcon } from "@radix-ui/react-icons";
import { errorText, rpc } from "./api";
import { familyLabels, type KeiConfig, type ModelFamily, type ModelProfile } from "./teamTypes";

type DesktopWindow = Window & { __ARIS_DESKTOP__?: boolean; ipc?: { postMessage?: (message: string) => void } };

export function KeiSettings({ token, onChanged }: { token: string; onChanged?: () => void | Promise<void> }) {
  const [config, setConfig] = useState<KeiConfig>();
  const [key, setKey] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [importing, setImporting] = useState(false);
  const [importUrl, setImportUrl] = useState("");
  useEffect(() => {
    const abort = new AbortController();
    void rpc<KeiConfig>(token, "kei/config", {}, abort.signal).then(async (loaded) => {
      if (abort.signal.aborted) return;
      setConfig(loaded);
      // An import can finish in another tab while this panel is closed.
      await onChanged?.();
    }).catch((e) => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [token]);
  useEffect(() => {
    if (!importing) return;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const result = await rpc<{ status: string; message?: string; imported_count?: number }>(token, "kei/import/status", {}, abort.signal);
        if (abort.signal.aborted) return;
        if (result.status === "completed") {
          setConfig(await rpc<KeiConfig>(token, "kei/config", {}, abort.signal)); setImporting(false); setImportUrl("");
          const desktop = window as DesktopWindow;
          if (desktop.__ARIS_DESKTOP__) desktop.ipc?.postMessage?.(JSON.stringify({ event: "aris-imported", version: 1 }));
          setNotice(result.message || `已导入 ${result.imported_count || 0} 个模型档案。`); await onChanged?.(); return;
        }
        if (result.status === "error") { setError(result.message || "导入未完成，请重试。"); setImporting(false); return; }
        timer = setTimeout(() => void poll(), 1500);
      } catch (e) { if (!abort.signal.aborted) { setError(errorText(e)); setImporting(false); } }
    }
    void poll(); return () => { abort.abort(); clearTimeout(timer); };
  }, [token, importing]);
  async function perform(action: () => Promise<void>) {
    if (busy) return;
    setBusy(true); setError(""); setNotice("");
    try { await action(); } catch (e) { setError(errorText(e)); } finally { setBusy(false); }
  }
  function patchProfile(index: number, patch: Partial<ModelProfile>) {
    setConfig((previous) => previous && { ...previous, profiles: previous.profiles.map((profile, i) => i === index ? { ...profile, ...patch } : profile) });
  }
  return <div className="kei-settings">
    <div className="aris-section-heading"><span className="eyebrow">ONE GATEWAY / MANY MODELS</span><h2>KEI API</h2><p>一个网关连接整个团队。每位智能体使用独立的模型档案。</p></div>
    <div className="kei-import"><div><strong>已有 KEI API 账号？</strong><p>在 KEI API 页面确认，将连接与模型导入 ARIS。</p></div><button type="button" className="solid-button" disabled={busy || importing} onClick={() => {
      const desktop = window as DesktopWindow;
      const tab = desktop.__ARIS_DESKTOP__ ? null : window.open("about:blank", "_blank"); if (tab) tab.opener = null;
      void perform(async () => {
        try {
          const result = await rpc<{ url: string }>(token, "kei/import/start");
          const target = new URL(result.url); if (!/^https?:$/.test(target.protocol)) throw new Error("导入地址无效。");
          setImportUrl(target.href);
          if (desktop.__ARIS_DESKTOP__) window.open(target.href, "_blank");
          else if (tab) tab.location.href = target.href;
          setImporting(true);
        } catch (e) { tab?.close(); throw e; }
      });
    }}>{importing ? "等待 KEI API 确认…" : "从 KEIAPI 一键导入"}</button></div>
    {importUrl && <p className="subtle">已打开导入页面。<a href={importUrl} target="_blank" rel="noreferrer noopener">未打开时点击这里</a></p>}
    {error && <div className="aris-error" role="alert">{error}</div>}
    {notice && <p className="aris-success" role="status">{notice}</p>}
    {!config ? <p className="subtle">正在读取连接配置…</p> : <>
      <label className="aris-field">API Base URL<input value={config.base_url} placeholder="https://your-gateway.example/v1" onChange={(e) => setConfig({ ...config, base_url: e.target.value })} /></label>
      <div className="kei-key-row"><label className="aris-field">API Key <small>{config.configured ? "已配置 · 不显示已保存密钥" : "尚未配置"}</small><input type="password" autoComplete="new-password" value={key} placeholder="输入新密钥" onChange={(e) => setKey(e.target.value)} /></label><button className="outline-button" disabled={busy || !key.trim()} onClick={() => void perform(async () => {
        await rpc(token, "kei/credential", { api_key: key }); setKey("");
        setConfig((c) => c && { ...c, configured: true }); setNotice("密钥已保存。"); await onChanged?.();
      })}>保存密钥</button></div>
      <div className="aris-split-heading"><h3>模型档案 <span>{config.profiles.length}</span></h3><button className="outline-button" disabled={busy} onClick={() => void perform(async () => {
        // Save the endpoint first so discovery uses precisely the visible gateway.
        const saved = await rpc<KeiConfig>(token, "kei/update", { base_url: config.base_url, profiles: config.profiles }); setConfig(saved);
        const result = await rpc<{ models: { id: string }[] }>(token, "kei/models"); setModels(result.models.map((model) => model.id));
        setNotice(`已读取 ${result.models.length} 个模型，可在模型 ID 中选择。`); await onChanged?.();
      })}><ReloadIcon />从网关获取模型</button></div>
      <datalist id="kei-model-catalog">{models.map((id) => <option key={id} value={id} />)}</datalist>
      <div className="kei-profile-list">{config.profiles.map((profile, index) => <fieldset className="kei-profile" key={profile.id}>
        <legend>{profile.name || `模型 ${index + 1}`}</legend>
        <div className="kei-profile-grid">
          <label className="aris-field">名称<input value={profile.name} onChange={(e) => patchProfile(index, { name: e.target.value })} /></label>
          <label className="aris-field">模型家族<select value={profile.family} onChange={(e) => patchProfile(index, { family: e.target.value as ModelFamily })}>{Object.entries(familyLabels).map(([id, label]) => <option key={id} value={id}>{label}</option>)}</select></label>
          <label className="aris-field">模型 ID<input list="kei-model-catalog" value={profile.model} placeholder="填写网关实际提供的 ID" onChange={(e) => patchProfile(index, { model: e.target.value })} /></label>
          <label className="aris-field">API 协议<select value={profile.protocol} onChange={(e) => patchProfile(index, { protocol: e.target.value as ModelProfile["protocol"] })}><option value="chat_completions">Chat Completions</option><option value="responses">Responses</option></select></label>
        </div>
        <details className="profile-advanced"><summary>高级配置</summary><div className="kei-profile-grid"><label className="aris-field">独立 Base URL <small>留空继承网关</small><input value={profile.base_url || ""} onChange={(e) => patchProfile(index, { base_url: e.target.value || undefined })} /></label><label className="aris-field">密钥环境变量 <small>可选</small><input value={profile.api_key_env || ""} onChange={(e) => patchProfile(index, { api_key_env: e.target.value || undefined })} /></label><label className="aris-field">最大输出 tokens<input type="number" min={1} value={profile.max_tokens || ""} onChange={(e) => patchProfile(index, { max_tokens: e.target.value ? Number(e.target.value) : undefined })} /></label><label className="aris-field">Temperature<input type="number" min={0} max={2} step={0.1} value={profile.temperature ?? ""} onChange={(e) => patchProfile(index, { temperature: e.target.value ? Number(e.target.value) : undefined })} /></label></div><label className="profile-thinking"><input type="checkbox" checked={profile.thinking || false} onChange={(e) => patchProfile(index, { thinking: e.target.checked })} />启用思考（需要上游模型支持）</label></details>
        <div className="kei-profile-footer"><code>{profile.id}</code><button className="icon-button" aria-label={`删除模型 ${profile.name}`} onClick={() => setConfig({ ...config, profiles: config.profiles.filter((_, i) => i !== index) })}><TrashIcon /></button></div>
      </fieldset>)}</div>
      <div className="inline-actions"><button className="outline-button" onClick={() => setConfig({ ...config, profiles: [...config.profiles, { id: `model-${crypto.randomUUID().slice(0, 8)}`, name: "新模型", family: "gpt", model: "", protocol: "chat_completions" }] })}><PlusIcon />添加模型</button>
      <button className="solid-button" disabled={busy || !config.base_url.trim() || config.profiles.some((p) => !p.name.trim())} onClick={() => void perform(async () => {
        setConfig(await rpc<KeiConfig>(token, "kei/update", { base_url: config.base_url, profiles: config.profiles })); setNotice("网关与模型档案已保存。"); await onChanged?.();
      })}>{busy ? "正在处理…" : "保存配置"}</button></div>
      <p className="subtle kei-hint">模型 ID 以 KEI API 实际返回为准。GPT / Codex 家族可按上游接口选择 Responses；其他档案使用网关支持的协议。</p>
    </>}
  </div>;
}
