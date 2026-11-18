import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

spec = importlib.util.spec_from_file_location("aris_crawl", Path(__file__).with_name("crawl.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Handler(BaseHTTPRequestHandler):
    hits = []

    def do_GET(self):
        type(self).hits.append(self.path)
        if self.path == "/slow":
            time.sleep(1.5)
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "http://example.invalid/offsite")
            self.end_headers()
            return
        if self.path == "/missing":
            self.send_response(404)
            self.end_headers()
            return
        content = {
            "/": '<title>中文来源</title><main><h1>Research root</h1><p>真实正文。</p><a href="/a">Next</a><a href="/missing">Missing</a><a href="http://example.invalid/offsite">External</a></main><script>hidden code</script>',
            "/a": '<title>Second</title><h1>Second page</h1><a href="/deep">Deep</a><a href="/#again">Cycle</a>',
            "/deep": '<h1>Deep page</h1>',
            "/large": '<h1>Large</h1><p>' + 'content ' * 1000 + '</p>',
            "/js": '<h1>Static shell</h1><p id="result"></p><script>document.getElementById("result").textContent="Rendered JavaScript evidence"</script>',
        }.get(self.path, '<h1>Fixture</h1>')
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        try:
            self.wfile.write(content.encode("utf-8"))
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass

    def log_message(self, *_):
        pass


class CrawlTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.worker = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.worker.start()
        cls.url = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.worker.join()

    def test_real_fetch_markdown_sources_scope_and_http_errors(self):
        result = module.crawl({"url": self.url, "max_pages": 3, "max_depth": 1})
        self.assertEqual(result["engine"], "scrapling")
        self.assertEqual(len(result["pages"]), 2)
        self.assertEqual(result["pages"][0]["title"], "中文来源")
        self.assertIn("真实正文", result["pages"][0]["markdown"])
        self.assertNotIn("hidden code", result["pages"][0]["markdown"])
        self.assertEqual(result["pages"][0]["source_url"], self.url + "/")
        self.assertTrue(result["pages"][0]["fetched_at"].endswith("+00:00"))
        self.assertEqual(result["failures"][0]["category"], "http_error")
        self.assertEqual(result["failures"][0]["status"], 404)
        self.assertEqual(result["attempted_pages"], 3)

    def test_page_depth_character_and_redirect_limits(self):
        result = module.crawl({"url": self.url, "max_pages": 1})
        self.assertEqual(len(result["pages"]), 1)
        self.assertEqual(result["stop_reason"], "page_limit")
        self.assertEqual(len(module.crawl({"url": self.url, "max_depth": 0})["pages"]), 1)
        large = module.crawl({"url": self.url + "/large", "max_chars": 1000})
        self.assertEqual(large["characters"], 1000)
        self.assertEqual(large["stop_reason"], "character_limit")
        redirect = module.crawl({"url": self.url + "/redirect"})
        self.assertEqual(redirect["failures"][0]["category"], "redirect_error")

    def test_json_process_protocol_and_invalid_input(self):
        process = subprocess.run([sys.executable, "-X", "utf8", str(Path(__file__).with_name("crawl.py"))],
                                 input=json.dumps({"url": self.url, "max_depth": 0}), text=True, encoding="utf-8", capture_output=True, timeout=15)
        self.assertEqual(process.returncode, 0)
        self.assertEqual(len(json.loads(process.stdout)["pages"]), 1)
        with self.assertRaises(ValueError):
            module.crawl({"url": "file:///C:/Windows/win.ini"})
        with self.assertRaises(ValueError):
            module.crawl({"url": self.url, "max_pages": 11})

    def test_request_timeout_is_reported_as_a_failure(self):
        started = time.monotonic()
        result = module.crawl({"url": self.url + "/slow", "timeout_secs": 1})
        self.assertEqual(result["pages"], [])
        self.assertEqual(result["failures"][0]["category"], "timeout")
        self.assertLess(time.monotonic() - started, 2.5)

    @unittest.skipUnless(os.environ.get("ARIS_TEST_DYNAMIC") == "1", "set ARIS_TEST_DYNAMIC=1 after installing Chromium")
    def test_dynamic_executes_javascript(self):
        static = module.crawl({"url": self.url + "/js", "max_depth": 0})
        self.assertNotIn("Rendered JavaScript evidence", static["pages"][0]["markdown"])
        dynamic = module.crawl({"url": self.url + "/js", "mode": "dynamic", "max_depth": 0, "timeout_secs": 30})
        self.assertEqual(dynamic["failures"], [], dynamic)
        self.assertIn("Rendered JavaScript evidence", dynamic["pages"][0]["markdown"])


if __name__ == "__main__":
    unittest.main()
