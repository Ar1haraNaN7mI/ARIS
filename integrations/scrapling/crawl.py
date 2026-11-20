"""Bounded Scrapling adapter. One JSON request on stdin, one JSON result on stdout."""
from __future__ import annotations

from collections import deque
from datetime import datetime, timezone
import importlib.metadata
import json
import logging
import os
import sys
import time
from urllib.parse import urldefrag, urljoin, urlsplit, urlunsplit


def timestamp():
    return datetime.now(timezone.utc).isoformat()


def normalized_url(value):
    if not isinstance(value, str) or len(value) > 8192:
        raise ValueError("url must be an HTTP(S) URL up to 8192 characters")
    parts = urlsplit(urldefrag(value.strip())[0])
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise ValueError("Only HTTP(S) URLs are supported")
    if parts.username is not None or parts.password is not None:
        raise ValueError("Credentials in URLs are not supported")
    return urlunsplit((parts.scheme.lower(), parts.netloc.lower(), parts.path or "/", parts.query, ""))


def origin(url):
    parts = urlsplit(url)
    return parts.scheme.lower(), parts.hostname.lower(), parts.port or (443 if parts.scheme == "https" else 80)


def integer(options, key, default, minimum, maximum):
    value = options.get(key, default)
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ValueError(f"{key} must be an integer between {minimum} and {maximum}")
    return value


def classify(error):
    message = str(error).lower()
    if isinstance(error, (ImportError, ModuleNotFoundError)):
        return "dependency_missing"
    if "timeout" in message or "timed out" in message:
        return "timeout"
    if "executable doesn't exist" in message or "browser" in message and ("install" in message or "launch" in message):
        return "browser_unavailable"
    if "certificate" in message or "ssl" in message:
        return "tls_error"
    if "resolve" in message or "dns" in message:
        return "dns_error"
    if "redirect" in message:
        return "redirect_error"
    return "fetch_error"


def fetch(url, mode, timeout, scope, selector):
    from scrapling.fetchers import Fetcher, StealthyFetcher

    if mode == "dynamic":
        # Block top-level redirects outside the crawl origin before navigation.
        blocked_redirect = []
        def setup(page):
            def route_request(route):
                request = route.request
                if request.is_navigation_request() and request.frame == page.main_frame and origin(request.url) != scope:
                    blocked_redirect.append(request.url)
                    route.abort()
                else:
                    route.continue_()
            page.route("**/*", route_request)

        kwargs = dict(headless=True, timeout=timeout * 1000, retries=1, load_dom=True,
                      google_search=False, page_setup=setup, network_idle=False,
                      wait=100, selector_config={"huge_tree": True})
        if executable := os.environ.get("ARIS_SCRAPLING_BROWSER"):
            kwargs["executable_path"] = executable
        if selector:
            kwargs["wait_selector"] = selector
        try:
            return StealthyFetcher.fetch(url, **kwargs)
        except Exception as error:
            if blocked_redirect:
                raise ValueError("Redirect leaves the requested origin") from error
            raise

    started = time.monotonic()
    current = url
    for _ in range(6):
        remaining = timeout - (time.monotonic() - started)
        if remaining <= 0:
            raise TimeoutError("Page redirect timeout")
        response = Fetcher.get(current, timeout=remaining, retries=1, follow_redirects=False,
                               stealthy_headers=False, headers={"User-Agent": "ARIS/0.1 (Scrapling)"},
                               selector_config={"huge_tree": True})
        if response.status not in (301, 302, 303, 307, 308):
            return response
        headers = {key.lower(): value for key, value in response.headers.items()}
        location = headers.get("location")
        if not location:
            raise ValueError("Redirect response has no Location")
        current = normalized_url(urljoin(current, location))
        if origin(current) != scope:
            raise ValueError("Redirect leaves the requested origin")
    raise ValueError("Too many redirects (maximum 5)")


def crawl(options):
    if not isinstance(options, dict):
        raise ValueError("Expected a JSON object")
    url = normalized_url(options.get("url"))
    mode = options.get("mode", "static")
    if mode not in ("static", "dynamic"):
        raise ValueError("mode must be static or dynamic")
    max_pages = integer(options, "max_pages", 3, 1, 10)
    depth_limit = integer(options, "max_depth", 1, 0, 3)
    max_chars = options.get("max_chars")
    if max_chars is not None and (isinstance(max_chars, bool) or not isinstance(max_chars, int) or max_chars < 1):
        raise ValueError("max_chars must be a positive integer")
    timeout = integer(options, "timeout_secs", 45, 1, 60)
    selector = options.get("css_selector")
    if selector is not None and (not isinstance(selector, str) or not selector or len(selector) > 512):
        raise ValueError("css_selector must contain 1–512 characters")
    scope = origin(url)
    queue = deque([(url, 0)])
    scheduled = {url}
    attempted = set()
    pages, failures = [], []
    deadline = time.monotonic() + timeout
    chars = 0
    reason = None
    while queue and len(attempted) < max_pages:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            reason = "timeout"
            break
        current, depth = queue.popleft()
        if current in attempted:
            continue
        attempted.add(current)
        fetched_at = timestamp()
        try:
            response = fetch(current, mode, min(20, remaining), scope, selector)
            source_url = normalized_url(response.url)
            if origin(source_url) != scope:
                raise ValueError("Redirect leaves the requested origin")
            headers = {key.lower(): value for key, value in response.headers.items()}
            content_type = headers.get("content-type", "")
            if response.status >= 400:
                failures.append({"url": current, "source_url": source_url, "fetched_at": fetched_at,
                                 "category": "http_error", "status": response.status, "message": f"HTTP {response.status}"})
                continue
            if content_type and not any(kind in content_type.lower() for kind in ("text/html", "application/xhtml", "text/plain")):
                failures.append({"url": current, "fetched_at": fetched_at, "category": "unsupported_content", "message": content_type[:160]})
                continue
            markdown = response.markdown(main_content_only=True, css_selector=selector)
            truncated = max_chars is not None and len(markdown) > max_chars - chars
            if truncated:
                markdown = markdown[:max_chars - chars]
            chars += len(markdown)
            pages.append({"url": current, "source_url": source_url, "fetched_at": fetched_at,
                          "depth": depth, "status": response.status,
                          "title": response.css("title::text").get() or "",
                          "markdown": markdown, "truncated": truncated})
            if truncated:
                reason = "character_limit"
                break
            if depth < depth_limit:
                for href in response.css("a::attr(href)").getall()[:500]:
                    try:
                        link = normalized_url(urljoin(source_url, href))
                    except (ValueError, TypeError):
                        continue
                    if origin(link) == scope and link not in scheduled and len(scheduled) < 1000:
                        scheduled.add(link)
                        queue.append((link, depth + 1))
            if max_chars is not None and chars >= max_chars and queue:
                reason = "character_limit"
                break
        except Exception as error:
            failures.append({"url": current, "fetched_at": fetched_at,
                             "category": classify(error), "message": str(error)[:800]})
    if reason is None and queue:
        reason = "page_limit"
    return {"engine": "scrapling", "version": importlib.metadata.version("scrapling"), "mode": mode,
            "content_kind": "untrusted_web_content", "started_url": url, "finished_at": timestamp(),
            "scope": "same_origin", "pages": pages, "failures": failures,
            "attempted_pages": len(attempted), "characters": chars,
            "truncated": reason is not None, "stop_reason": reason,
            "limits": {"max_pages": max_pages, "max_depth": depth_limit, "max_chars": max_chars, "timeout_secs": timeout}}


def main():
    logging.disable(logging.CRITICAL)
    try:
        raw = sys.stdin.buffer.read(65537)
        if len(raw) > 65536:
            raise ValueError("Request exceeds 64 KiB")
        options = json.loads(raw.decode("utf-8"))
        result = crawl(options)
    except Exception as error:
        result = {"engine": "scrapling", "error": {"category": "invalid_input" if isinstance(error, (ValueError, TypeError)) else classify(error), "message": str(error)[:800]}}
    sys.stdout.buffer.write((json.dumps(result, ensure_ascii=False) + "\n").encode("utf-8"))


if __name__ == "__main__":
    main()
