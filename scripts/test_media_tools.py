"""Real local media + isolated provider protocol tests (no desktop or paid calls)."""
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("aris_media", Path(__file__).resolve().parents[1] / "integrations/media/media.py")
media = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(media)
try:
    from PIL import Image
except ImportError:
    Image = None


@unittest.skipIf(Image is None, "Install integrations/media/requirements.txt")
class MediaTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="aris-media-tests-")
        self.root = Path(self.temporary.name).resolve()

    def tearDown(self):
        self.temporary.cleanup()

    def call(self, operation, args, provider=None):
        return media.dispatch({"operation": operation, "args": args, "workspace": str(self.root), "provider": provider or {}})

    def test_poster_is_real_png_and_refuses_overwrite_and_text_overflow(self):
        args = {"output": "deliverables/poster.png", "width": 640, "height": 800, "background": "#e8faff",
                "elements": [{"type": "rectangle", "x": 30, "y": 30, "width": 580, "height": 4, "color": "#16b1d1"},
                             {"type": "text", "x": 30, "y": 70, "text": "ARIS\nCreate together", "font_size": 42}]}
        result = self.call("poster_render", args)
        artifact = result["artifacts"][0]
        self.assertFalse(result["generated_by_model"])
        self.assertEqual((artifact["width"], artifact["height"]), (640, 800))
        self.assertGreater(artifact["bytes"], 1000)
        self.assertEqual(len(artifact["sha256"]), 64)
        with Image.open(artifact["path"]) as image:
            self.assertEqual(image.format, "PNG")
            self.assertEqual(image.getpixel((35, 31))[:3], (22, 177, 209))
        with self.assertRaisesRegex(media.MediaError, "already exists"):
            self.call("poster_render", args)
        args["output"] = "overflow.png"
        args["elements"][1]["x"] = 630
        with self.assertRaisesRegex(media.MediaError, "exceeds the canvas"):
            self.call("poster_render", args)
        self.assertFalse((self.root / "overflow.png").exists())

    def test_chinese_auto_selects_real_system_font_and_accepts_workspace_font(self):
        args = {"output": "chinese.png", "width": 640, "height": 400,
                "elements": [{"type": "text", "x": 20, "y": 20, "text": "多智能体协作", "font_size": 48}]}
        with mock.patch.object(media, "system_font_paths", return_value=[]), self.assertRaisesRegex(media.MediaError, "No supported system CJK font"):
            self.call("poster_render", args)
        font = next((p for p in media.system_font_paths() if p.is_file()), None)
        if font is None:
            self.skipTest("No local CJK font available")
        auto = self.call("poster_render", args)
        self.assertTrue(auto["fonts_used"][0]["family"])
        self.assertEqual(Path(auto["fonts_used"][0]["path"]), font)
        shutil.copy2(font, self.root / "cjk.ttc")
        args["output"] = "explicit-font.png"
        args["elements"][0]["font"] = "cjk.ttc"
        result = self.call("poster_render", args)
        self.assertEqual(self.call("media_probe", {"path": result["artifacts"][0]["path"]})["width"], 640)

    def test_media_rejects_escape_url_aliases_and_hidden_state(self):
        for value in ("../escape.png", "https://example.com/a.png", ".aris/state.png", "file.png:secret", "NUL.png", "bad. /x.png"):
            with self.subTest(path=value), self.assertRaises(media.MediaError):
                self.call("poster_render", {"output": value, "width": 100, "height": 100, "elements": []})

    def test_symlink_cannot_escape_workspace(self):
        with tempfile.TemporaryDirectory() as outside:
            try:
                (self.root / "link").symlink_to(outside, target_is_directory=True)
            except OSError:
                self.skipTest("Symlink creation not available on this host")
            with self.assertRaisesRegex(media.MediaError, "escapes"):
                self.call("poster_render", {"output": "link/x.png", "width": 100, "height": 100, "elements": []})

    @unittest.skipUnless(shutil.which("ffmpeg") and shutil.which("ffprobe"), "FFmpeg/ffprobe not installed")
    def test_video_cuts_concatenates_audio_and_checks_real_frames(self):
        ffmpeg = media.program("ffmpeg")
        for name, color, sound in [("red", "red", True), ("blue", "blue", False)]:
            command = [ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", f"color=c={color}:s=320x240:r=24:d=2"]
            if sound:
                command += ["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "aac"]
            command += ["-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", str(self.root / f"{name}.mp4")]
            subprocess.run(command, check=True, capture_output=True)
        result = self.call("video_edit", {"clips": [{"path": "red.mp4", "start_seconds": .5, "duration_seconds": .5},
                                                    {"path": "blue.mp4", "start_seconds": 1, "duration_seconds": 1}],
                                           "output": "joined.mp4", "width": 320, "height": 240, "fps": 24})
        artifact = result["artifacts"][0]
        self.assertAlmostEqual(artifact["duration_seconds"], 1.5, delta=.2)
        self.assertEqual(artifact["audio_codec"], "aac")
        for timepoint, channel in [(.1, 0), (.9, 2)]:
            data = subprocess.check_output([ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-ss", str(timepoint),
                                            "-i", artifact["path"], "-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "pipe:1"])
            pixel = Image.open(io.BytesIO(data)).getpixel((100, 100))
            self.assertGreater(pixel[channel], 200)
        muted = self.call("video_edit", {"clips": [{"path": "red.mp4", "duration_seconds": .4}], "output": "muted.mp4", "audio": "mute", "width": 320, "height": 240})
        self.assertIsNone(muted["artifacts"][0]["audio_codec"])
        with self.assertRaisesRegex(media.MediaError, "exceeds input"):
            self.call("video_edit", {"clips": [{"path": "red.mp4", "start_seconds": 1, "duration_seconds": 2}], "output": "invalid.mp4"})

    def test_generated_image_protocol_redirect_and_error_redaction(self):
        buffer = io.BytesIO()
        Image.new("RGB", (64, 96), "cyan").save(buffer, "PNG")
        encoded = base64.b64encode(buffer.getvalue()).decode()
        requests = []
        mode = ["success"]

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                requests.append((self.path, self.headers.get("Authorization"), json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
                if mode[0] == "redirect":
                    self.send_response(302)
                    self.send_header("Location", f"http://127.0.0.1:{self.server.server_port}/steal")
                    self.end_headers()
                    return
                if mode[0] == "failure":
                    self.send_response(503)
                    self.end_headers()
                    self.wfile.write(b"echo fixture-secret")
                    return
                self.send_response(200)
                self.end_headers()
                if mode[0].startswith("responses"):
                    body = {"status": "incomplete" if mode[0] == "responses_incomplete" else "completed", "output": [
                        {"type": "message", "content": []},
                        {"type": "image_generation_call", "status": "completed", "result": encoded}]}
                    if mode[0] == "responses_missing":
                        body["output"] = [{"type": "message", "content": []}]
                else:
                    body = {"data": [{"b64_json": encoded}]} if mode[0] == "success" else {"data": [{"url": "https://example.com/never-fetch.png"}]}
                self.wfile.write(json.dumps(body).encode())

            def do_GET(self):
                requests.append((self.path, self.headers.get("Authorization"), {}))
                self.send_response(500)
                self.end_headers()

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        provider = {"enabled": True, "model": "fixture-image-model", "api_key": "fixture-secret",
                    "base_url": f"http://127.0.0.1:{server.server_port}/v1"}
        try:
            result = self.call("image_generate", {"prompt": "测试图片", "output": "generated.png"}, provider)
            self.assertEqual(result["model"], "fixture-image-model")
            self.assertEqual(result["artifacts"][0]["height"], 96)
            self.assertEqual(requests[0][0], "/v1/images/generations")
            self.assertEqual(requests[0][1], "Bearer fixture-secret")
            self.assertEqual(requests[0][2], {"model": "fixture-image-model", "prompt": "测试图片", "n": 1})
            self.assertNotIn("fixture-secret", json.dumps(result))
            for selected, expected in [("redirect", "redirect_rejected"), ("failure", "provider_http_error"), ("url", "invalid_response")]:
                mode[0] = selected
                with self.assertRaises(media.MediaError) as error:
                    self.call("image_generate", {"prompt": "test", "output": selected + ".png"}, provider)
                self.assertEqual(error.exception.code, expected)
                self.assertNotIn("fixture-secret", str(error.exception))
                self.assertFalse((self.root / (selected + ".png")).exists())
            self.assertEqual(len(requests), 4)  # No redirected or returned URL request.
            responses_provider = {**provider, "protocol": "responses", "model": "fixture-responses-model", "image_model": "fixture-tool-image-model"}
            mode[0] = "responses_success"
            response_result = self.call("image_generate", {"prompt": "Draw a cyan square", "output": "responses.png", "size": "1024x1024", "quality": "low"}, responses_provider)
            self.assertEqual(response_result["protocol"], "responses")
            self.assertEqual(response_result["artifacts"][0]["height"], 96)
            self.assertEqual(requests[-1][0], "/v1/responses")
            self.assertEqual(requests[-1][2], {"model": "fixture-responses-model", "input": "Draw a cyan square",
                "tools": [{"type": "image_generation", "model": "fixture-tool-image-model", "size": "1024x1024", "quality": "low"}],
                "tool_choice": {"type": "image_generation"}, "store": False})
            for selected, expected in [("responses_incomplete", "generation_incomplete"), ("responses_missing", "invalid_response")]:
                mode[0] = selected
                with self.assertRaises(media.MediaError) as error:
                    self.call("image_generate", {"prompt": "test", "output": selected + ".png"}, responses_provider)
                self.assertEqual(error.exception.code, expected)
                self.assertFalse((self.root / (selected + ".png")).exists())
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_disabled_provider_does_not_invent_host_capability(self):
        with self.assertRaisesRegex(media.MediaError, "not configured"):
            self.call("image_generate", {"prompt": "test", "output": "none.png"})
        self.assertFalse(self.call("media_status", {})["host_imagegen"])


if __name__ == "__main__":
    unittest.main()
