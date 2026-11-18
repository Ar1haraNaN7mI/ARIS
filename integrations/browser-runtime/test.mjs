import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { once } from "node:events";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { BrowserRuntime } from "./runtime.mjs";

const html = `<!doctype html><meta charset="utf-8"><title>ARIS driver fixture</title>
<style>button {position:absolute;left:20px;top:20px;width:100px;height:40px} input {position:absolute;left:20px;top:100px;width:200px;height:40px} output{position:absolute;top:180px}</style>
<button onclick="this.textContent=String(++window.count)">0</button><input aria-label="notes"><output>Isolated browser fixture</output><script>window.count=0</script>`;

test("real browser contexts isolate mouse, storage, tabs, queues and cancellation", async t => {
  const server = createServer((_, response) => { response.setHeader("content-type", "text/html"); response.end(html); });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const url = `http://127.0.0.1:${server.address().port}/`;
  const runtime = new BrowserRuntime();
  t.after(async () => { await runtime.close(); await new Promise(resolve => server.close(resolve)); });
  const action = (agent_id, action, more = {}) => runtime.execute({ agent_id, action, ...more });
  const [a, b] = await Promise.all([action("a", "open", { url }), action("b", "open", { url })]);
  assert.notEqual(a.page_id, b.page_id);
  assert.equal(a.screenshot.mime, "image/png");
  assert.ok(Buffer.from(a.screenshot.image_base64, "base64").subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10])));

  await t.test("mouse clicks and keyboard target only their agent", async () => {
    const clicked = await action("a", "click", { x: 60, y: 40 });
    assert.match(clicked.text, /^1/);
    const untouched = await action("b", "observe");
    assert.match(untouched.text, /^0/);
    assert.deepEqual(untouched.pointer, { x: 0, y: 0 });
    await action("a", "click", { x: 50, y: 120 });
    await Promise.all([action("a", "type", { text: "first" }), action("a", "type", { text: "-second" })]);
    const pageA = runtime.sessions.get("a").pages.get(a.page_id);
    const pageB = runtime.sessions.get("b").pages.get(b.page_id);
    assert.equal(await pageA.locator("input").inputValue(), "first-second");
    assert.equal(await pageB.locator("input").inputValue(), "");
  });

  await t.test("cookies, local storage and page IDs cannot cross agents", async () => {
    const pageA = runtime.sessions.get("a").pages.get(a.page_id);
    const pageB = runtime.sessions.get("b").pages.get(b.page_id);
    await pageA.evaluate(() => { document.cookie = "owner=a"; localStorage.setItem("owner", "a"); });
    assert.deepEqual(await pageB.evaluate(() => [document.cookie, localStorage.getItem("owner")]), ["", null]);
    await assert.rejects(action("b", "observe", { page_id: a.page_id }), /does not belong/);
  });

  await t.test("one waiting agent does not block another", async () => {
    let finished = false;
    const waiting = action("a", "wait", { ms: 800 }).then(() => { finished = true; });
    const tabs = await action("b", "tabs");
    assert.equal(tabs.pages.length, 1);
    assert.equal(finished, false);
    await waiting;
  });

  await t.test("cancel closes only the affected context and rejects queued work", async () => {
    const waiting = assert.rejects(action("a", "wait", { ms: 5000 }));
    const queued = assert.rejects(action("a", "click", { x: 60, y: 40 }), /cancelled/);
    await new Promise(resolve => setTimeout(resolve, 50));
    await runtime.cancel("a");
    await Promise.all([waiting, queued]);
    assert.equal(runtime.sessions.has("a"), false);
    assert.match((await action("b", "click", { x: 60, y: 40 })).text, /^1/);
  });

  await t.test("unsupported inputs fail before targeting another surface", async () => {
    await assert.rejects(action("b", "navigate", { url: "file:///C:/Windows/win.ini" }), /Only HTTP/);
    await assert.rejects(action("b", "click", { x: -1, y: 10 }), /viewport/);
    await assert.rejects(action("b", "drag", { path: [] }), /2–100/);
    await action("b", "close");
    assert.deepEqual((await runtime.list()).sessions, []);
  });
});

test("JSONL service correlates parallel requests and closes cleanly on EOF", async t => {
  const child = spawn(process.execPath, [fileURLToPath(new URL("./server.mjs", import.meta.url))], { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  const lines = createInterface({ input: child.stdout });
  const replies = new Map();
  lines.on("line", line => { const result = JSON.parse(line); replies.get(result.id)?.(result); });
  const send = request => new Promise(resolve => { replies.set(request.id, resolve); child.stdin.write(`${JSON.stringify(request)}\n`); });
  const [a, b] = await Promise.all([
    send({ id: "one", agent_id: "agent-one", action: "open" }),
    send({ id: "two", agent_id: "agent-two", action: "open" }),
  ]);
  assert.equal(a.result.agent_id, "agent-one");
  assert.equal(b.result.agent_id, "agent-two");
  const exit = once(child, "exit");
  child.stdin.end();
  const [code] = await exit;
  assert.equal(code, 0);
});
