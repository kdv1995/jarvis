#!/usr/bin/env python3
"""Local Kokoro-82M text-to-speech server for Jarvis.

Loads the Kokoro model once and keeps it warm in memory, then serves synthesized
speech over plain HTTP. The Rust app (`tts.rs`) POSTs text and gets back WAV
audio — no per-request model cold-start. This mirrors how the Ollama LLM server
is used: a small always-on local service the app talks to over localhost.

    POST /speak   {"text": "..."}   ->   audio/wav  (24 kHz mono)
    GET  /health                    ->   "ok"

Run it with the project venv's Python:
    kokoro-tts/.venv/bin/python kokoro-tts/server.py

Change VOICE below to taste — natural English options include:
    af_heart   American female — highest-rated, most natural
    am_michael American male
    am_adam    American male
    bm_george  British male  (closest to the classic "Jarvis" feel)
    bm_lewis   British male
"""

import io
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np
import soundfile as sf
from kokoro import KPipeline

VOICE = "bm_george"  # British male — closest to the classic Iron Man "Jarvis" feel.
PORT = 11435  # one above Ollama's 11434, by convention.
SAMPLE_RATE = 24_000  # Kokoro's native output rate.

print("[kokoro] loading model (first run downloads ~330 MB)…", flush=True)
_pipeline = KPipeline(lang_code="a")  # 'a' = American English

# Warm the pipeline so the first real request isn't slow.
for _ in _pipeline("Ready.", voice=VOICE):
    pass
print(f"[kokoro] ready on :{PORT} (voice={VOICE})", flush=True)


def synthesize(text: str) -> bytes:
    """Render `text` to in-memory WAV bytes."""
    chunks = [audio for _, _, audio in _pipeline(text, voice=VOICE)]
    audio = np.concatenate(chunks) if chunks else np.zeros(1, dtype=np.float32)
    buf = io.BytesIO()
    sf.write(buf, audio, SAMPLE_RATE, format="WAV")
    return buf.getvalue()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self._send(200, b"ok", "text/plain")
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/speak":
            self.send_error(404)
            return

        length = int(self.headers.get("Content-Length", 0))
        try:
            payload = json.loads(self.rfile.read(length) or b"{}")
            text = (payload.get("text") or "").strip()
        except Exception as e:  # noqa: BLE001 — report any parse failure to the client
            self.send_error(400, f"bad request: {e}")
            return

        if not text:
            self.send_error(400, "empty text")
            return

        try:
            data = synthesize(text)
        except Exception as e:  # noqa: BLE001 — synthesis errors go back as 500
            self.send_error(500, f"synthesis failed: {e}")
            return

        self._send(200, data, "audio/wav")

    def _send(self, status: int, body: bytes, content_type: str):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass  # stay quiet; Jarvis logs the timing itself


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
