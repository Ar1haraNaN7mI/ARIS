import { createInterface } from "node:readline";
import { BrowserRuntime } from "./runtime.mjs";

const runtime = new BrowserRuntime({ headless: process.env.ARIS_BROWSER_HEADLESS !== "0" });
const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
const reply = value => process.stdout.write(`${JSON.stringify(value)}\n`);
input.on("line", line => {
  let request;
  try {
    if (line.length > 64 * 1024) throw new Error("Browser request too large");
    request = JSON.parse(line);
    if (typeof request.id !== "string") throw new Error("Request id is required");
  } catch (error) { reply({ id: request?.id ?? null, error: error.message }); return; }
  runtime.execute(request).then(result => reply({ id: request.id, result }), error => reply({ id: request.id, error: error.message }));
});
let closing = false;
async function close() {
  if (closing) return;
  closing = true;
  const deadline = setTimeout(() => process.exit(1), 3000);
  try { await runtime.close(); } finally { clearTimeout(deadline); process.exit(0); }
}
input.on("close", close);
process.on("SIGINT", close);
process.on("SIGTERM", close);
