import { afterEach, expect, it, vi } from "vitest";
import { mergeSession, SessionCache } from "./sessionCache";
import * as api from "./api";
import type { SessionResult } from "./types";
vi.mock("./api", async (original) => ({ ...(await original<typeof import("./api")>()), rpc: vi.fn() }));
afterEach(() => vi.resetAllMocks());
const snapshot = (ids: string[], cursor: string, reset = false, count = ids.length): SessionResult => ({ session: { id: "s", name: "Session", events: ids.map((id) => ({ id, type: "assistant_message", at: id, text: id })) }, task_id: "task", state: "running", sync: { cursor, reset, event_count: count } });
it("appends the exact cursor window and replaces history after server reset", () => {
  const first = snapshot(["a", "b"], "two", true);
  const next = mergeSession(first, snapshot(["c"], "three", false, 3));
  expect(next.session.events.map((event) => event.id)).toEqual(["a", "b", "c"]);
  expect(mergeSession(next, snapshot(["summary"], "compacted", true)).session.events.map((event) => event.id)).toEqual(["summary"]);
});
it("keeps unchanged history and result references without replaying old records", () => {
  const first = snapshot(["a"], "one", true);
  expect(mergeSession(first, snapshot([], "one", false, 1))).toBe(first);
  const finished = mergeSession(first, { ...snapshot([], "one", false, 1), state: "completed" });
  expect(finished.session.events).toBe(first.session.events);
  expect(finished.state).toBe("completed");
});
it("sends the server cursor and deduplicates simultaneous reads", async () => {
  let complete!: (result: SessionResult) => void;
  vi.mocked(api.rpc).mockImplementationOnce(() => new Promise<SessionResult>((resolve) => { complete = resolve; }));
  const cache = new SessionCache("token");
  const one = cache.load("s", "running"); const two = cache.load("s", "running");
  expect(api.rpc).toHaveBeenCalledTimes(1);
  complete(snapshot(["a"], "one", true));
  expect(await one).toBe(await two);
  vi.mocked(api.rpc).mockResolvedValueOnce(snapshot(["b"], "two", false, 2));
  await cache.load("s", "running");
  expect(api.rpc).toHaveBeenLastCalledWith("token", "sessions/get", { id: "s", cursor: "one" }, expect.any(AbortSignal));
  expect(cache.peek("s")?.session.events.map((event) => event.id)).toEqual(["a", "b"]);
  cache.dispose();
});
it("waits for an older running read before fetching the final state", async () => {
  let complete!: (result: SessionResult) => void;
  vi.mocked(api.rpc).mockImplementationOnce(() => new Promise<SessionResult>((resolve) => { complete = resolve; }));
  vi.mocked(api.rpc).mockResolvedValueOnce({ ...snapshot(["b"], "two", false, 2), state: "completed" });
  const cache = new SessionCache("token");
  const running = cache.load("s", "running"); const final = cache.load("s", "completed");
  expect(api.rpc).toHaveBeenCalledTimes(1);
  complete(snapshot(["a"], "one", true)); await running; await final;
  expect(api.rpc).toHaveBeenCalledTimes(2);
  expect(cache.synced("s", "completed")).toBe(true);
  expect(cache.peek("s")?.session.events.map((event) => event.id)).toEqual(["a", "b"]);
  cache.dispose();
});
it("preserves start and end events that share the same ID", () => {
  const first = snapshot(["turn"], "start", true);
  first.session.events[0].type = "turn_start";
  const end = snapshot(["turn"], "end", false, 2); end.session.events[0].type = "turn_end";
  expect(mergeSession(first, end).session.events.map((event) => event.type)).toEqual(["turn_start", "turn_end"]);
});
it("recovers a mismatched incremental boundary with one full read", async () => {
  vi.mocked(api.rpc).mockResolvedValueOnce(snapshot(["a"], "one", true))
    .mockResolvedValueOnce(snapshot(["missing-boundary"], "bad", false, 50))
    .mockResolvedValueOnce(snapshot(["compacted"], "reset", true));
  const cache = new SessionCache("token"); await cache.load("s", "running");
  expect((await cache.load("s", "running")).session.events.map((event) => event.id)).toEqual(["compacted"]);
  expect(api.rpc).toHaveBeenLastCalledWith("token", "sessions/get", { id: "s" }, expect.any(AbortSignal));
  cache.dispose();
});
it("rejects a wrong-session response without contaminating cached history", async () => {
  vi.mocked(api.rpc).mockResolvedValueOnce({ ...snapshot([], "bad", true), session: { id: "other", name: null, events: [] } });
  const cache = new SessionCache("token");
  await expect(cache.load("s", "running")).rejects.toThrow("不匹配");
  expect(cache.peek("s")).toBeUndefined(); cache.dispose();
});
