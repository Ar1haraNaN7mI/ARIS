# Scrapling research adapter

ARIS exposes `web_crawl` using [Scrapling](https://github.com/D4Vinci/Scrapling) 0.5.1 and its documented [HTTP fetcher](https://scrapling.readthedocs.io/en/latest/fetching/static.html), [browser fetcher](https://scrapling.readthedocs.io/en/latest/fetching/stealthy.html), and [Markdown conversion](https://scrapling.readthedocs.io/en/latest/ai/building-rag-systems.html).

```sh
python -m pip install -r integrations/scrapling/requirements.txt
python -m unittest discover -s integrations/scrapling -p "test_*.py"
```

Static HTTP is the default. Dynamic mode explicitly uses `StealthyFetcher` and needs its browser installed (`scrapling install`). Alternatively, `ARIS_SCRAPLING_BROWSER` can name a compatible Chromium executable. Set `ARIS_PYTHON` to select the Python environment, and `ARIS_SCRAPLING_RUNTIME` to select an installed `crawl.py`.

The tool accepts `url`, `mode` (`static`/`dynamic`), `max_pages` (1–10), `max_depth` (0–3), `timeout_secs` (1–60), and an optional `css_selector`. It returns full Markdown by default. Supply a positive `max_chars` to request an excerpt across the collected pages. Traversal and top-level redirects stay on the initial origin (same scheme, hostname and port). Dynamic pages may load third-party subresources. Each result includes source URLs, UTC fetch timestamps, HTTP status and Markdown. Failures are reported per page, and partial results identify the limit that stopped traversal. Non-HTML/text content is reported explicitly. Fetched content is untrusted data.

The adapter reads one UTF-8 JSON request on stdin and writes one UTF-8 JSON result to stdout. No API keys or external hosted scraping service are required. This is content collection, not a search-engine index.
