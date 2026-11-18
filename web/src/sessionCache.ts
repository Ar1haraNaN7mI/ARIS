import { rpc } from "./api";
import type { SessionResult } from "./types";

export const needsSessionReset = (previous: SessionResult | undefined, incoming: SessionResult) => incoming.sync?.reset === false &&
  (!previous || previous.session.id !== incoming.session.id || previous.session.events.length + incoming.session.events.length !== incoming.sync.event_count);

/** Session logs are append-only between explicit server resets. */
export function mergeSession(previous: SessionResult | undefined, incoming: SessionResult): SessionResult {
  const replace = !previous || previous.session.id !== incoming.session.id || incoming.sync?.reset !== false;
  if (needsSessionReset(previous, incoming)) throw new Error("会话增量边界不匹配。");
  // Start/end records may share an ID. The opaque server cursor, not event IDs,
  // defines exactly which records are new.
  const events = replace ? incoming.session.events : incoming.session.events.length ? [...previous.session.events, ...incoming.session.events] : previous.session.events;
  if (previous && events === previous.session.events && incoming.task_id === previous.task_id && incoming.state === previous.state &&
      incoming.session.name === previous.session.name && incoming.session.cwd === previous.session.cwd && incoming.sync?.cursor === previous.sync?.cursor) return previous;
  return { ...incoming, session: { ...incoming.session, events } };
}

type Entry = { value?: SessionResult; revision?: string; pending?: { revision: string; promise: Promise<SessionResult> }; controller?: AbortController };

/** One in-flight request per session; switching views retains completed history. */
export class SessionCache {
  private entries = new Map<string, Entry>();
  constructor(private token: string) {}
  peek(id: string) { return this.entries.get(id)?.value; }
  synced(id: string, revision: string) { return !!this.peek(id) && this.entries.get(id)?.revision === revision; }
  async load(id: string, revision: string): Promise<SessionResult> {
    let entry = this.entries.get(id);
    if (!entry) { entry = {}; this.entries.set(id, entry); }
    if (entry.pending) {
      if (entry.pending.revision === revision) return entry.pending.promise;
      // A final-state refresh must start after an older running-state read.
      await entry.pending.promise;
      return this.load(id, revision);
    }
    const current = entry;
    const controller = new AbortController();
    current.controller = controller;
    const deadline = setTimeout(() => controller.abort(), 10_000);
    const promise = rpc<SessionResult>(this.token, "sessions/get", { id, ...(current.value?.sync?.cursor ? { cursor: current.value.sync.cursor } : {}) }, controller.signal)
      .then(async (incoming) => {
        if (incoming.session.id !== id) throw new Error("会话响应与请求不匹配。");
        if (needsSessionReset(current.value, incoming)) incoming = await rpc<SessionResult>(this.token, "sessions/get", { id }, controller.signal);
        if (incoming.session.id !== id) throw new Error("会话响应与请求不匹配。");
        const value = mergeSession(current.value, incoming);
        current.value = value; current.revision = revision;
        return value;
      }).finally(() => { clearTimeout(deadline); current.pending = undefined; current.controller = undefined; });
    current.pending = { revision, promise };
    return promise;
  }
  dispose() { for (const entry of this.entries.values()) entry.controller?.abort(); this.entries.clear(); }
}
