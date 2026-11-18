#!/usr/bin/env python3
"""ARIS bounded media adapter. JSON stdin/stdout; no shell, device capture or URL inputs."""
from __future__ import annotations

import base64
import hashlib
import io
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

MAX_FILE = 1024 * 1024 * 1024
MAX_IMAGE = 20 * 1024 * 1024
VIDEO_EXTENSIONS = {".mp4", ".mov", ".mkv", ".webm", ".avi"}
IMAGE_EXTENSIONS = {".png", ".jpg", ".jpeg", ".webp"}


class MediaError(Exception):
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


def require(condition, message, code="invalid_arguments"):
    if not condition:
        raise MediaError(code, message)


def number(value, minimum, maximum, name):
    require(isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value) and minimum <= value <= maximum,
            f"{name} must be between {minimum} and {maximum}")
    return value


def integer(value, minimum, maximum, name):
    number(value, minimum, maximum, name)
    require(isinstance(value, int), f"{name} must be an integer")
    return value


def local_path(root, value, extensions, output=False):
    require(isinstance(value, str) and 0 < len(value) <= 4096, "A local media path is required")
    require(not any(ord(c) < 32 for c in value) and not re.match(r"^[a-zA-Z]+://", value),
            "Only local workspace files are accepted")
    p = Path(value)
    p = p if p.is_absolute() else root / p
    # Ban Windows alternate streams, device aliases, and trailing-dot/space aliases on every host.
    for part in p.parts[1:] if p.anchor else p.parts:
        require(part not in ("..",) and ":" not in part and part == part.rstrip(" ."),
                "Ambiguous media paths are not supported")
        require(not re.match(r"^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)", part, re.I),
                "Device paths are not supported")
    resolved = p.resolve()
    require(resolved.is_relative_to(root) and resolved != root, "Media path escapes the workspace")
    rel = resolved.relative_to(root)
    require(not any(part.startswith(".") for part in rel.parts), "Hidden workspace state is not media storage")
    require(resolved.suffix.lower() in extensions, "Unsupported media file extension")
    if output:
        require(not resolved.exists() and not p.is_symlink(), "Output already exists; choose a new filename", "output_exists")
    else:
        require(resolved.is_file(), "Input media file does not exist", "missing_input")
        require(resolved.stat().st_size <= MAX_FILE, "Input exceeds 1 GiB", "input_too_large")
    return resolved


def program(name):
    requested = os.environ.get("ARIS_" + name.upper(), name)
    result = shutil.which(requested)
    require(result is not None, f"{name} is unavailable; install FFmpeg and ffprobe or set ARIS_{name.upper()}", "missing_dependency")
    return result


def run(command, deadline, limit=128 * 1024):
    remaining = deadline - time.monotonic()
    require(remaining > 0, "Media operation timed out", "timeout")
    # Temporary files prevent pipe deadlocks and unbounded in-memory log accumulation.
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        try:
            result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                    timeout=remaining, creationflags=0x08000000 if os.name == "nt" else 0)
        except subprocess.TimeoutExpired:
            raise MediaError("timeout", "Media operation timed out") from None
        require(result.returncode == 0, "FFmpeg could not decode or encode the supplied media; check codecs and input files", "codec_error")
        stdout.seek(0)
        data = stdout.read(limit + 1)
        require(len(data) <= limit, "Media metadata exceeded its limit", "output_too_large")
        return data


def probe(path, deadline):
    data = json.loads(run([program("ffprobe"), "-v", "error", "-protocol_whitelist", "file,pipe",
                           "-format_whitelist", "mov,matroska,webm,avi",
                           "-show_entries", "format=duration,size:stream=codec_type,codec_name,width,height,duration",
                           "-of", "json", str(path)], deadline))
    streams = data.get("streams", [])
    video = next((s for s in streams if s.get("codec_type") == "video"), None)
    audio = next((s for s in streams if s.get("codec_type") == "audio"), None)
    duration = float(data.get("format", {}).get("duration", 0))
    require(math.isfinite(duration) and duration >= 0, "Invalid media duration", "invalid_media")
    return {"duration_seconds": duration, "width": video.get("width") if video else None,
            "height": video.get("height") if video else None, "video_codec": video.get("codec_name") if video else None,
            "audio_codec": audio.get("codec_name") if audio else None}


def pillow():
    try:
        from PIL import Image, ImageDraw, ImageFont, ImageOps
    except ImportError:
        raise MediaError("missing_dependency", "Pillow is unavailable; install the media requirements.txt") from None
    Image.MAX_IMAGE_PIXELS = 16_777_216
    return Image, ImageDraw, ImageFont, ImageOps


def image_info(path):
    Image, _, _, _ = pillow()
    with Image.open(path) as image:
        require(image.width * image.height <= 16_777_216, "Image exceeds 16 megapixels", "input_too_large")
        image.verify()
        return {"width": image.width, "height": image.height}


def artifact(path, mime, metadata):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"path": str(path), "mime": mime, "bytes": path.stat().st_size,
            "sha256": digest.hexdigest(), **metadata}


def publish(temporary, output):
    require(temporary.stat().st_size > 0, "No media output was produced", "invalid_media")
    # Same-filesystem hard link atomically publishes a verified file without replacing an existing one.
    try:
        os.link(temporary, output)
    except FileExistsError:
        raise MediaError("output_exists", "Output already exists; choose a new filename") from None
    except OSError:
        raise MediaError("publish_failed", "Could not atomically publish media on this filesystem") from None


def video_edit(root, args):
    clips = args.get("clips")
    require(isinstance(clips, list) and 1 <= len(clips) <= 12, "Supply 1 to 12 clips")
    width = integer(args.get("width", 1280), 64, 3840, "width")
    height = integer(args.get("height", 720), 64, 2160, "height")
    require(width % 2 == 0 and height % 2 == 0, "Video dimensions must be even")
    fps = integer(args.get("fps", 30), 1, 60, "fps")
    audio = args.get("audio", "preserve")
    require(audio in ("preserve", "mute"), "audio must be preserve or mute")
    output = local_path(root, args.get("output"), {".mp4"}, True)
    deadline = time.monotonic() + 280
    sources = []
    for clip in clips:
        path = local_path(root, clip.get("path"), VIDEO_EXTENSIONS)
        start = number(clip.get("start_seconds", 0), 0, 86400, "start_seconds")
        duration = number(clip.get("duration_seconds"), 0.05, 600, "duration_seconds")
        info = probe(path, deadline)
        require(info["video_codec"], "Every clip must have a video stream", "invalid_media")
        require(start + duration <= info["duration_seconds"] + 0.05, "Clip range exceeds input duration")
        sources.append((path, start, duration, info))
    total = sum(s[2] for s in sources)
    require(total <= 600, "Edited video may not exceed 600 seconds")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".aris-media-", dir=output.parent) as temporary:
        tmp = Path(temporary)
        normalized = []
        for i, (path, start, duration, info) in enumerate(sources):
            part = tmp / f"part-{i}.mp4"
            command = [program("ffmpeg"), "-nostdin", "-hide_banner", "-loglevel", "error", "-n",
                       "-protocol_whitelist", "file,pipe", "-format_whitelist", "mov,matroska,webm,avi",
                       "-ss", str(start), "-i", str(path)]
            if audio == "preserve" and not info["audio_codec"]:
                command += ["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]
            command += ["-t", str(duration), "-map", "0:v:0", "-vf",
                        f"scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}",
                        "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-threads", "2"]
            if audio == "preserve":
                command += ["-map", "0:a:0" if info["audio_codec"] else "1:a:0", "-af", "apad",
                            "-c:a", "aac", "-ar", "48000", "-ac", "2"]
            else:
                command += ["-an"]
            command += ["-map_metadata", "-1", "-movflags", "+faststart", "-fs", str(MAX_FILE), str(part)]
            run(command, deadline)
            normalized.append(part)
        # The list contains only generated basenames, never caller-controlled ffconcat directives.
        listing = tmp / "clips.txt"
        listing.write_text("".join(f"file '{p.name}'\n" for p in normalized), encoding="utf-8")
        rendered = tmp / "rendered.mp4"
        run([program("ffmpeg"), "-nostdin", "-hide_banner", "-loglevel", "error", "-n", "-f", "concat",
             "-safe", "1", "-protocol_whitelist", "file,pipe", "-i", str(listing), "-c", "copy",
             "-map_metadata", "-1", "-movflags", "+faststart", str(rendered)], deadline)
        info = probe(rendered, deadline)
        require(abs(info["duration_seconds"] - total) <= max(0.2, len(clips) * 0.06),
                "Rendered duration did not match the requested edit", "verification_failed")
        require(info["width"] == width and info["height"] == height, "Rendered dimensions did not match", "verification_failed")
        publish(rendered, output)
    return {"artifacts": [artifact(output, "video/mp4", info)], "clips": len(clips), "audio": audio}


def color(value):
    require(isinstance(value, str) and re.fullmatch(r"#[0-9a-fA-F]{6}(?:[0-9a-fA-F]{2})?", value), "Colors must be #RRGGBB or #RRGGBBAA")
    return value


def system_font_paths():
    """Fixed OS font candidates only; the model cannot nominate outside paths."""
    return [Path(os.environ.get("WINDIR", "C:/Windows")) / "Fonts/msyh.ttc",
            Path(os.environ.get("WINDIR", "C:/Windows")) / "Fonts/msyhbd.ttc",
            Path("/System/Library/Fonts/PingFang.ttc"),
            Path("/System/Library/Fonts/STHeiti Medium.ttc"),
            Path("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            Path("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc")]


def poster_font(root, element, text, size, ImageFont):
    if element.get("font"):
        path = local_path(root, element["font"], {".ttf", ".otf", ".ttc"})
        return ImageFont.truetype(str(path), size), str(path)
    if text.isascii():
        return ImageFont.load_default(size=size), "Pillow default"
    for path in system_font_paths():
        if path.is_file():
            try:
                return ImageFont.truetype(str(path), size), str(path)
            except OSError:
                continue
    raise MediaError("font_required", "No supported system CJK font is installed; place a supporting TTF/OTF/TTC in the workspace and pass font")


def poster_render(root, args):
    Image, ImageDraw, ImageFont, ImageOps = pillow()
    width = integer(args.get("width"), 64, 4096, "width")
    height = integer(args.get("height"), 64, 4096, "height")
    output = local_path(root, args.get("output"), {".png"}, True)
    elements = args.get("elements", [])
    require(isinstance(elements, list) and len(elements) <= 80, "At most 80 poster elements are supported")
    canvas = Image.new("RGBA", (width, height), color(args.get("background", "#ffffff")))
    draw = ImageDraw.Draw(canvas)
    fonts_used = {}
    for element in elements:
        x = integer(element.get("x"), 0, width - 1, "element.x")
        y = integer(element.get("y"), 0, height - 1, "element.y")
        kind = element.get("type")
        if kind == "text":
            text = element.get("text", "")
            require(isinstance(text, str) and 0 < len(text) <= 2000, "Text must contain 1 to 2000 characters")
            size = integer(element.get("font_size", 48), 8, 512, "font_size")
            font, font_source = poster_font(root, element, text, size, ImageFont)
            fonts_used[font_source] = {"path": font_source, "family": font.getname()[0], "style": font.getname()[1]}
            bbox = draw.multiline_textbbox((x, y), text, font=font, spacing=round(size * 0.25))
            require(bbox[0] >= 0 and bbox[1] >= 0 and bbox[2] <= width and bbox[3] <= height,
                    "Text exceeds the canvas; wrap it or reduce font_size", "layout_overflow")
            draw.multiline_text((x, y), text, font=font, fill=color(element.get("color", "#111827")), spacing=round(size * 0.25))
        elif kind in ("rectangle", "image"):
            w = integer(element.get("width"), 1, width - x, "element.width")
            h = integer(element.get("height"), 1, height - y, "element.height")
            if kind == "rectangle":
                draw.rectangle((x, y, x + w - 1, y + h - 1), fill=color(element.get("color", "#111827")))
            else:
                source = local_path(root, element.get("path"), IMAGE_EXTENSIONS)
                image_info(source)
                with Image.open(source) as image:
                    fitted = ImageOps.fit(ImageOps.exif_transpose(image).convert("RGBA"), (w, h))
                    canvas.alpha_composite(fitted, (x, y))
        else:
            raise MediaError("invalid_arguments", "Unknown poster element type")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".aris-media-", dir=output.parent) as temporary:
        rendered = Path(temporary) / "poster.png"
        canvas.save(rendered, "PNG")
        info = image_info(rendered)
        publish(rendered, output)
    return {"artifacts": [artifact(output, "image/png", info)], "renderer": "Pillow", "fonts_used": list(fonts_used.values()), "generated_by_model": False}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise MediaError("redirect_rejected", "Image provider redirects are disabled; configure its final API endpoint")


def image_generate(root, args, provider):
    require(provider.get("enabled") is True, "Image generation is not configured; enable an exact image model in outer media.json", "not_configured")
    model, key, base = provider.get("model", ""), provider.get("api_key", ""), provider.get("base_url", "")
    require(model and key, "An exact image model and provider credential are required", "not_configured")
    parsed = urllib.parse.urlsplit(base)
    require(parsed.scheme == "https" or (parsed.scheme == "http" and parsed.hostname in ("127.0.0.1", "localhost", "::1")),
            "Image providers require HTTPS, except loopback test servers", "invalid_configuration")
    require(not parsed.username and not parsed.password and not parsed.query and not parsed.fragment,
            "Invalid image provider API base", "invalid_configuration")
    output = local_path(root, args.get("output"), {".png"}, True)
    prompt = args.get("prompt", "")
    require(isinstance(prompt, str) and 1 <= len(prompt) <= 16000, "prompt must contain 1 to 16000 characters")
    protocol = provider.get("protocol", "images")
    require(protocol in ("images", "responses"), "Unknown image provider protocol", "invalid_configuration")
    options = {}
    for option in ("size", "quality"):
        if option in args:
            require(isinstance(args[option], str) and 0 < len(args[option]) <= 64, f"Invalid {option}")
            options[option] = args[option]
    if protocol == "responses":
        tool = {"type": "image_generation", **options}
        if provider.get("image_model"):
            tool["model"] = provider["image_model"]
        body = {"model": model, "input": prompt, "tools": [tool],
                "tool_choice": {"type": "image_generation"}, "store": False}
        endpoint = "/responses"
    else:
        body = {"model": model, "prompt": prompt, "n": 1, **options}
        if provider.get("response_format") == "b64_json":
            body["response_format"] = "b64_json"
        endpoint = "/images/generations"
    request = urllib.request.Request(base.rstrip("/") + endpoint, json.dumps(body).encode("utf-8"),
                                     {"Content-Type": "application/json", "Authorization": "Bearer " + key}, method="POST")
    # No credential follows redirects; response URLs are never fetched.
    try:
        with urllib.request.build_opener(NoRedirect()).open(request, timeout=330) as response:
            raw = response.read(MAX_IMAGE * 2 + 1)
            require(len(raw) <= MAX_IMAGE * 2, "Image response exceeds 40 MiB", "output_too_large")
    except urllib.error.HTTPError as error:
        # Provider bodies may echo the key or prompt. Never include them in logs or tool results.
        raise MediaError("provider_http_error", f"Image provider returned HTTP {error.code}") from None
    except (urllib.error.URLError, TimeoutError):
        raise MediaError("provider_unavailable", "Image provider request failed or timed out") from None
    try:
        value = json.loads(raw)
        if protocol == "responses":
            require(value.get("status", "completed") == "completed", "Responses image generation did not complete", "generation_incomplete")
            outputs = [item for item in value.get("output", []) if isinstance(item, dict)
                       and item.get("type") == "image_generation_call" and item.get("status", "completed") == "completed"]
            require(len(outputs) == 1, "Expected exactly one completed Responses image_generation_call", "invalid_response")
            encoded = outputs[0]["result"]
        else:
            encoded = value["data"][0]["b64_json"]
        require(isinstance(encoded, str), "Provider did not return base64 image data", "invalid_response")
        decoded = base64.b64decode(encoded, validate=True)
        require(len(decoded) <= MAX_IMAGE, "Generated image exceeds 20 MiB", "output_too_large")
    except (KeyError, IndexError, ValueError, TypeError):
        expected = "output[].image_generation_call.result" if protocol == "responses" else "data[0].b64_json"
        raise MediaError("invalid_response", f"Provider must return base64 {expected}; remote image URLs are not downloaded") from None
    Image, _, _, _ = pillow()
    with Image.open(io.BytesIO(decoded)) as image:
        require(image.format in ("PNG", "JPEG", "WEBP") and image.width * image.height <= 16_777_216,
                "Provider returned an unsupported or oversized image", "invalid_response")
        image.load()
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".aris-media-", dir=output.parent) as temporary:
            rendered = Path(temporary) / "generated.png"
            image.convert("RGBA").save(rendered, "PNG")
            info = image_info(rendered)
            publish(rendered, output)
    return {"artifacts": [artifact(output, "image/png", info)], "model": model, "protocol": protocol,
            "image_model": provider.get("image_model"), "generated_by_model": True}


def dispatch(envelope):
    root = Path(envelope["workspace"]).resolve(strict=True)
    operation, args = envelope["operation"], envelope.get("args", {})
    if operation == "media_status":
        try:
            import PIL
            version = PIL.__version__
        except ImportError:
            version = None
        provider = envelope.get("provider", {})
        return {"ffmpeg": bool(shutil.which(os.environ.get("ARIS_FFMPEG", "ffmpeg"))),
                "ffprobe": bool(shutil.which(os.environ.get("ARIS_FFPROBE", "ffprobe"))),
                "pillow": version, "image_generation": {"enabled": provider.get("enabled", False), "model": provider.get("model"),
                    "protocol": provider.get("protocol", "images"), "image_model": provider.get("image_model"),
                    "credential_configured": provider.get("credential_configured", False), "provider_verified": False,
                    "configuration_error": provider.get("configuration_error")},
                "host_imagegen": False}
    if operation == "media_probe":
        path = local_path(root, args.get("path"), VIDEO_EXTENSIONS | IMAGE_EXTENSIONS)
        info = image_info(path) if path.suffix.lower() in IMAGE_EXTENSIONS else probe(path, time.monotonic() + 25)
        return {"path": str(path), "bytes": path.stat().st_size, **info}
    if operation == "video_edit":
        return video_edit(root, args)
    if operation == "poster_render":
        return poster_render(root, args)
    if operation == "image_generate":
        return image_generate(root, args, envelope.get("provider", {}))
    raise MediaError("invalid_arguments", "Unknown media operation")


def main():
    try:
        raw = sys.stdin.buffer.read(256 * 1024 + 1)
        require(len(raw) <= 256 * 1024, "Media request exceeds 256 KiB")
        result = dispatch(json.loads(raw))
    except MediaError as error:
        result = {"error": {"code": error.code, "message": str(error)}}
    except Exception:
        result = {"error": {"code": "invalid_media", "message": "Media could not be processed; check its format, fonts and local dependencies"}}
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
