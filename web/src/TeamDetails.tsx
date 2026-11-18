import { useState } from "react";
import { Cross1Icon, FileTextIcon, PlusIcon, ResetIcon } from "@radix-ui/react-icons";
import type { Team } from "./teamTypes";
import type { useTeams } from "./useTeams";

export function TeamDetails({ team, state, onClose }: { team: Team; state: ReturnType<typeof useTeams>; onClose: () => void }) {
  const [label, setLabel] = useState("");
  const [restore, setRestore] = useState<string>();
  const blocked = state.busy || team.status === "running" || team.status === "cancelling";
  return <aside className="aw-details" aria-label="交付与回滚"><header><div><strong>交付与回滚</strong><span>所有智能体共享同一个工作区</span></div><button className="aw-icon" aria-label="关闭详情" onClick={onClose}><Cross1Icon /></button></header><div className="aw-details-scroll">
    <section><h3><FileTextIcon />共享文件 <span>{team.files.length}</span></h3>{team.files.map((path) => <div className="aw-file" key={path}><FileTextIcon /><span title={path}>{path}</span></div>)}{!team.files.length && <p className="aw-muted">尚未声明共享文件。</p>}</section>
    <section><h3><ResetIcon />文件快照 <span>{team.checkpoints.length}</span></h3><p className="aw-muted">保存当前共享文件，随时恢复到一个明确的版本。</p><form className="aw-snapshot-form" onSubmit={(event) => { event.preventDefault(); void state.checkpoint(team.id, label.trim() || `手动快照 ${team.checkpoints.length + 1}`).then((result) => { if (result) setLabel(""); }); }}><input aria-label="快照名称" value={label} onChange={(event) => setLabel(event.target.value)} placeholder="为当前版本命名" /><button className="aw-icon" aria-label="保存文件快照" disabled={blocked}><PlusIcon /></button></form>{team.checkpoints.slice().reverse().map((point) => <div className="aw-snapshot" key={point.id}><div><strong>{point.label}</strong><small>{new Date(point.at).toLocaleString("zh-CN")} · {point.files.length} 个文件</small></div><button className="aw-icon" aria-label={`恢复 ${point.label}`} disabled={blocked} onClick={() => setRestore(point.id)}><ResetIcon /></button>{restore === point.id && <div className="aw-restore-confirm"><p>恢复这些共享文件，当前版本会先自动备份。</p><button className="aw-button primary" disabled={blocked} onClick={() => void state.restore(team.id, point.id).then((result) => { if (result) setRestore(undefined); })}>恢复文件</button><button className="aw-button" onClick={() => setRestore(undefined)}>取消</button></div>}</div>)}</section>
  </div></aside>;
}
