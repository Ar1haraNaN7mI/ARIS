# ARIS browser runtime

ARIS uses the public [OpenAI computer-use driver pattern](https://developers.openai.com/api/docs/guides/tools-computer-use-integration) with [Playwright browser contexts](https://playwright.dev/docs/browser-contexts). This is a local driver, not a private ChatGPT endpoint.

```sh
cd integrations/browser-runtime
npm ci
npm run install-browser
npm test
```

Enable browser tools with `aris computer enable`. ARIS launches this service automatically over private standard input/output. `ARIS_BROWSER_RUNTIME` can point to an installed `server.mjs`; `ARIS_NODE` can override the Node executable. Set `ARIS_BROWSER_HEADLESS=0` to show the controlled browser windows. Node 22+ is required.

Each agent gets a separate browser context with its own cookies, storage, pages and `page.mouse`. Requests for one agent are serialized, while different agents run concurrently. The runtime-assigned session ID supplies the model's identity. A model cannot select another agent's context. Pixel coordinates are CSS pixels within a 1440 × 900 viewport. Returned PNG screenshots are actual browser captures; DOM text is also available to models without vision.

The JSONL protocol accepts `{id, agent_id, action, ...}` and returns `{id, result}` or `{id, error}`. Supported actions: `open`, `navigate`, `observe`, `screenshot`, `tabs`, `new_page`, `select_page`, `click`, `double_click`, `move`, `type`, `keypress`, `scroll`, `drag`, `wait`, `close`. A controller can list contexts with `list` and interrupt one context with `cancel`. Closing stdin releases every context and the browser. Actions time out after 25 seconds and close their affected context. Model calls also follow the harness permission policy.

The old Windows UIA/OCR driver and system-wide pointer overlay are removed. This runtime controls browser pages; native desktop application control would require a separate desktop/VM driver. It does not create multiple OS physical pointers.
