import { chromium } from "playwright";
import { randomUUID } from "node:crypto";

// One browser, independent incognito contexts and page.mouse objects per agent.
// This implements the public Playwright driver pattern, not a private GPT API.
export class BrowserRuntime {
  constructor({ headless = true, timeoutMs = 25_000 } = {}) {
    this.headless = headless;
    this.timeoutMs = timeoutMs;
    this.sessions = new Map();
    this.queues = new Map();
    this.generations = new Map();
    this.browserPromise = null;
    this.closed = false;
  }

  async browser() {
    if (this.closed) throw new Error("Browser runtime is closed");
    if (!this.browserPromise) {
      // Chromium does not need the gateway credentials in its environment.
      const env = Object.fromEntries(Object.entries(process.env).filter(([key]) =>
        /^(PATH|SYSTEMROOT|WINDIR|TEMP|TMP|HOME|USERPROFILE|LOCALAPPDATA|DISPLAY|WAYLAND_DISPLAY|XDG_RUNTIME_DIR|LANG)$/i.test(key)));
      this.browserPromise = chromium.launch({ headless: this.headless, env,
        args: ["--disable-extensions", "--disable-file-system"] }).then(browser => {
        browser.on("disconnected", () => { this.browserPromise = null; this.sessions.clear(); });
        return browser;
      }).catch(error => {
        this.browserPromise = null;
        throw new Error(`Chromium could not start. Run npm ci and npm run install-browser in integrations/browser-runtime. ${error.message}`);
      });
    }
    return this.browserPromise;
  }

  async open(agentId) {
    const generation = this.generations.get(agentId) ?? 0;
    let session = this.sessions.get(agentId);
    if (session) return session;
    if (this.sessions.size >= 32) throw new Error("Close an agent browser before opening more than 32 contexts");
    const browser = await this.browser();
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 },
      acceptDownloads: false, serviceWorkers: "block" });
    await context.route("file://**", route => route.abort());
    if (this.closed || generation !== (this.generations.get(agentId) ?? 0)) {
      await context.close();
      throw new Error("Browser creation cancelled");
    }
    session = { context, pages: new Map(), current: null, pointer: { x: 0, y: 0 }, agent_id: agentId };
    this.sessions.set(agentId, session);
    context.on("page", page => this.registerPage(session, page));
    const page = await context.newPage();
    this.registerPage(session, page);
    return session;
  }

  registerPage(session, page) {
    if ([...session.pages.values()].includes(page)) return;
    const id = randomUUID();
    session.pages.set(id, page);
    session.current = id;
    page.setDefaultTimeout(10_000);
    page.setDefaultNavigationTimeout(20_000);
    page.on("dialog", dialog => dialog.dismiss().catch(() => {}));
    page.on("close", () => {
      session.pages.delete(id);
      if (session.current === id) session.current = session.pages.keys().next().value ?? null;
    });
  }

  async cancel(agentId) {
    this.generations.set(agentId, (this.generations.get(agentId) ?? 0) + 1);
    const session = this.sessions.get(agentId);
    this.sessions.delete(agentId);
    if (session) await session.context.close().catch(() => {});
    return { agent_id: agentId, closed: true };
  }

  async execute(request) {
    if (this.closed) throw new Error("Browser runtime is closed");
    const agentId = request.agent_id;
    if (typeof agentId !== "string" || !agentId.trim() || agentId.length > 160) throw new Error("agent_id is required (1–160 characters)");
    if (request.action === "cancel") return this.cancel(agentId);
    if (request.action === "list") return this.list();
    const generation = this.generations.get(agentId) ?? 0;
    const previous = this.queues.get(agentId) ?? Promise.resolve();
    const operation = previous.catch(() => {}).then(async () => {
      if ((this.generations.get(agentId) ?? 0) !== generation) throw new Error("Browser action cancelled");
      let timer;
      const timeout = new Promise((_, reject) => {
        timer = setTimeout(() => {
          this.cancel(agentId).catch(() => {});
          reject(new Error("Browser action timed out; its context was closed. Reopen and inspect before retrying."));
        }, this.timeoutMs);
      });
      try { return await Promise.race([this.perform(request), timeout]); }
      finally { clearTimeout(timer); }
    });
    this.queues.set(agentId, operation);
    try { return await operation; }
    finally { if (this.queues.get(agentId) === operation) this.queues.delete(agentId); }
  }

  async list() {
    return { sessions: await Promise.all([...this.sessions.values()].map(async session => ({
      agent_id: session.agent_id, pages: await this.tabs(session), pointer: session.pointer,
    }))) };
  }

  async tabs(session) {
    return Promise.all([...session.pages].map(async ([page_id, page]) => ({
      page_id, url: page.url(), title: await page.title().catch(() => ""), selected: page_id === session.current,
    })));
  }

  async perform(request) {
    const { agent_id: agentId, action } = request;
    if (action === "close") return this.cancel(agentId);
    let session = this.sessions.get(agentId);
    if (action === "open") session = await this.open(agentId);
    if (!session) throw new Error("This agent has no browser. Call computer_open first.");
    if (action === "tabs") return { agent_id: agentId, pages: await this.tabs(session) };
    if (action === "new_page") this.registerPage(session, await session.context.newPage());
    if (request.page_id) {
      if (!session.pages.has(request.page_id)) throw new Error("page_id does not belong to this agent");
      session.current = request.page_id;
    }
    const page = session.pages.get(session.current);
    if (!page) throw new Error("No open page; use new_page or reopen the agent browser");
    switch (action) {
      case "open": case "navigate": case "new_page": {
        if (request.url) {
          const url = new URL(request.url);
          if (!["https:", "http:"].includes(url.protocol) && request.url !== "about:blank") throw new Error("Only HTTP(S) URLs and about:blank are supported");
          await page.goto(request.url, { waitUntil: "domcontentloaded" });
        } else if (action === "navigate") throw new Error("navigate requires url");
        break;
      }
      case "click": case "double_click": case "move": {
        this.coordinates(request.x, request.y);
        session.pointer = { x: request.x, y: request.y };
        const button = request.button ?? "left";
        if (!["left", "right", "middle"].includes(button)) throw new Error("Invalid mouse button");
        if (action === "move") await page.mouse.move(request.x, request.y);
        else if (action === "double_click") await page.mouse.dblclick(request.x, request.y, { button });
        else await page.mouse.click(request.x, request.y, { button });
        break;
      }
      case "type":
        if (typeof request.text !== "string" || request.text.length > 16_384) throw new Error("type requires text up to 16384 characters");
        await page.keyboard.insertText(request.text);
        break;
      case "keypress":
        if (typeof request.key !== "string" || !request.key || request.key.length > 128) throw new Error("keypress requires a Playwright key combination");
        await page.keyboard.press(request.key);
        break;
      case "scroll": {
        const x = request.delta_x ?? 0, y = request.delta_y ?? 0;
        if (![x, y].every(v => Number.isFinite(v) && Math.abs(v) <= 10_000)) throw new Error("Scroll deltas must be finite CSS pixels within ±10000");
        await page.mouse.wheel(x, y);
        await page.waitForTimeout(80);
        break;
      }
      case "drag": {
        const path = request.path;
        if (!Array.isArray(path) || path.length < 2 || path.length > 100) throw new Error("drag requires 2–100 points");
        for (const point of path) this.coordinates(point.x, point.y);
        await page.mouse.move(path[0].x, path[0].y);
        await page.mouse.down();
        try { for (const point of path.slice(1)) await page.mouse.move(point.x, point.y); }
        finally { await page.mouse.up(); }
        session.pointer = path.at(-1);
        break;
      }
      case "wait":
        if (!Number.isInteger(request.ms) || request.ms < 0 || request.ms > 5000) throw new Error("wait requires ms between 0 and 5000");
        await page.waitForTimeout(request.ms);
        break;
      case "observe": case "screenshot": case "select_page": break;
      default: throw new Error(`Unsupported browser action: ${action}`);
    }
    return this.observe(session, page);
  }

  coordinates(x, y) {
    if (![x, y].every(Number.isFinite) || x < 0 || y < 0 || x >= 1440 || y >= 900) throw new Error("Mouse coordinates must be inside the 1440×900 CSS-pixel viewport");
  }

  async observe(session, page) {
    // Text works for non-vision models; actual pixels are separately returned.
    const text = await page.locator("body").innerText({ timeout: 1500 }).catch(() => "");
    const nodes = await page.locator("a,button,input,textarea,select,[role=button],[contenteditable=true]").evaluateAll(elements =>
      elements.slice(0, 200).map(element => {
        const rect = element.getBoundingClientRect();
        return { role: element.getAttribute("role") ?? element.tagName.toLowerCase(),
          name: (element.getAttribute("aria-label") ?? element.textContent ?? "").trim().slice(0, 160),
          input_type: element.getAttribute("type"), placeholder: element.getAttribute("placeholder"),
          bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
          disabled: element.matches(":disabled") };
      }).filter(node => node.bounds.width > 0 && node.bounds.height > 0 && node.bounds.x < 1440 && node.bounds.y < 900 && node.bounds.x + node.bounds.width > 0 && node.bounds.y + node.bounds.height > 0)
    ).catch(() => []);
    const screenshot = await page.screenshot({ type: "png", fullPage: false });
    if (screenshot.length > 8 * 1024 * 1024) throw new Error("Screenshot exceeds 8 MiB");
    return { agent_id: session.agent_id, page_id: session.current, url: page.url(), title: await page.title(),
      content_kind: "untrusted_browser_observation", text: text.slice(0, 12_000), nodes,
      coordinate_space: "viewport_css_pixels", viewport: { width: 1440, height: 900 },
      pointer: session.pointer, screenshot: { mime: "image/png", width: 1440, height: 900, image_base64: screenshot.toString("base64") } };
  }

  async close() {
    this.closed = true;
    await Promise.allSettled([...this.sessions.keys()].map(id => this.cancel(id)));
    if (this.browserPromise) await (await this.browserPromise).close();
    this.browserPromise = null;
  }
}
