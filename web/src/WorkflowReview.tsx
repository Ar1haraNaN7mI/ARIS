import { useEffect, useState } from "react";
import { ChevronRightIcon, ReaderIcon } from "@radix-ui/react-icons";
import { errorText, rpc } from "./api";
import "./workflow-review.css";

type ReviewChange = {
  target: string;
  description: string;
  status: "applied" | "retained" | "proposed" | "retired";
  baseline?: string | null;
  candidate?: string | null;
  version?: number | null;
};

type Review = {
  id: string;
  status: "queued" | "running" | "completed" | "failed";
  completed_at?: string;
  model?: string | null;
  summary?: string | null;
  evidence?: Record<string, unknown>;
  findings: { evidence: string; lesson: string }[];
  changes: ReviewChange[];
  error?: string | null;
};

type Reviews = { review: Review | null; history?: Review[] };
type ReviewProps = { token: string; subject: "team_id" | "session_id" | "task_id"; id: string; startedAt?: string };

const reviewStates: Record<Review["status"], string> = {
  queued: "等待复盘", running: "正在复盘", completed: "已完成", failed: "复盘未完成",
};
const changeStates: Record<ReviewChange["status"], string> = {
  applied: "后续适用任务采用", retained: "沿用现行策略", proposed: "待验证", retired: "已停用",
};
const changeTargets: Record<string, string> = {
  scheduler: "泳道调度", harness: "内核建议", "edit_file.missing_anchor.read_file": "文件编辑恢复 · 找不到原文", "edit_file.ambiguous_anchor.read_file": "文件编辑恢复 · 匹配到多处原文",
};

function FindingEvidence({ name, records }: { name: string; records?: Record<string, unknown> }) {
  const [open, setOpen] = useState(false);
  if (!records || !Object.hasOwn(records, name)) return <p>{name}</p>;
  return <details className="wr-evidence" onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>查看执行证据 · {name}</summary>
    {open && <pre>{JSON.stringify(records[name], null, 2)}</pre>}
  </details>;
}

export function WorkflowReview(props: ReviewProps) {
  // A session can be switched before an outstanding read returns.
  return <ReviewForSubject key={`${props.subject}:${props.id}:${props.startedAt}:${props.token}`} {...props} />;
}

function ReviewForSubject({ token, subject, id, startedAt }: ReviewProps) {
  const [result, setResult] = useState<Reviews>();
  const [error, setError] = useState("");
  const [selected, setSelected] = useState("");
  useEffect(() => {
    const abort = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    let pending = false;
    let nullPolls = 0;
    async function load() {
      if (pending || abort.signal.aborted) return;
      pending = true;
      clearTimeout(timer);
      try {
        const next = await rpc<Reviews>(token, "reviewer/get", { [subject]: id }, abort.signal);
        if (abort.signal.aborted) return;
        // A terminal task event can reach the browser just before its review is enqueued.
        const previousRun = startedAt && next.review?.completed_at && Date.parse(next.review.completed_at) < Date.parse(startedAt);
        const awaitingEnqueue = !next.review || !!previousRun;
        if (awaitingEnqueue) nullPolls++;
        setResult(previousRun && nullPolls < 4 ? undefined : next); setError("");
        const working = next.review?.status === "running" || next.review?.status === "queued";
        if ((working || awaitingEnqueue) && !document.hidden) {
          // Large workflows can still be collecting execution evidence when the
          // answer appears. Older sessions without a review taper to one read per minute.
          const delay = working ? 1500 : Math.min(60_000, 2000 * Math.max(1, nullPolls - 2) ** 2);
          timer = setTimeout(() => void load(), delay);
        }
      } catch (failure) {
        if (!abort.signal.aborted) {
          setError(errorText(failure));
          timer = setTimeout(() => void load(), document.hidden ? 15_000 : 5000);
        }
      } finally { pending = false; }
    }
    void load();
    const onVisible = () => { if (!document.hidden) void load(); };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      abort.abort(); clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [token, subject, id, startedAt]);

  const latest = result?.review;
  if (!latest) return error ? <p className="wr-read-error" role="status">复盘记录读取失败 · {error}</p> : null;
  const history = result.history?.length ? result.history : [latest];
  const review = history.find((item) => item.id === selected) || latest;
  const showingPriorRun = review.id === latest.id && startedAt && review.completed_at && Date.parse(review.completed_at) < Date.parse(startedAt);
  const adopted = review.changes?.filter((change) => change.status === "applied").length || 0;
  return <details className="wr-panel">
    <summary>
      <ReaderIcon aria-hidden="true" /><strong>工作流复盘</strong>
      <span className={`wr-status wr-${review.status}`} aria-live="polite">
        {showingPriorRun && "上次复盘 · "}{reviewStates[review.status]}{adopted > 0 && ` · ${adopted} 项改进已生效`}
      </span>
      <ChevronRightIcon className="wr-chevron" aria-hidden="true" />
    </summary>
    <div className="wr-body">
      {history.length > 1 && <label className="wr-history">复盘记录
        <select value={review.id} onChange={(event) => setSelected(event.target.value)}>
          {history.map((item, index) => <option key={item.id} value={item.id}>
            第 {history.length - index} 次{item.id === latest.id && " · 最新"} · {reviewStates[item.status]}
          </option>)}
        </select>
      </label>}
      {review.model && <p className="wr-model">复盘模型 <code>{review.model}</code></p>}
      {review.summary && <p className="wr-summary">{review.summary}</p>}
      {review.status === "queued" && <p className="wr-hint">等待审阅本次执行记录。</p>}
      {review.status === "running" && <p className="wr-hint">正在核对执行记录和改进方案。</p>}
      {review.error && <p className="wr-read-error">{review.error}</p>}
      {!!review.findings?.length && <section aria-label="复盘发现">
        <h3>复盘发现</h3>
        <ul>{review.findings.map((item, index) => <li key={index}>
          <strong>{item.lesson}</strong><FindingEvidence name={item.evidence} records={review.evidence} />
        </li>)}</ul>
      </section>}
      {!!review.changes?.length && <section aria-label="优化评测">
        <h3>优化评测</h3>
        <ul>{review.changes.map((change, index) => <li key={index}>
          <div className="wr-change"><strong title={change.target}>{changeTargets[change.target] || change.target}</strong><span className={`wr-decision wr-${change.status}`}>
            {changeStates[change.status]}{change.version != null && ` · v${change.version}`}
          </span></div>
          <p>{change.description}</p>
          {(change.baseline || change.candidate) && <dl className="wr-comparison" aria-label={change.target === "scheduler" ? "工作流回放" : "执行记录对比"}>
            {change.baseline && <><dt>{change.target === "scheduler" ? "基线回放" : "基线记录"}</dt><dd>{change.baseline}</dd></>}
            {change.candidate && <><dt>{change.target === "scheduler" ? "候选回放" : "验证记录"}</dt><dd>{change.candidate}</dd></>}
          </dl>}
        </li>)}</ul>
      </section>}
      {error && <p className="wr-read-error" role="status">复盘更新暂时中断 · {error}</p>}
    </div>
  </details>;
}
