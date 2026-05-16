#!/usr/bin/env python3
"""Local faster-whisper speech-to-text server for Jarvis.

Loads the `large-v3-turbo` Whisper model once and keeps it warm in memory,
then serves transcriptions over plain HTTP. The Rust app (`stt.rs`) POSTs
WAV audio and gets back JSON `{"text": "...", "language": "..."}` — no
per-request model cold-start. Mirrors the Kokoro TTS server on :11435.

    POST /transcribe   <raw WAV bytes>   ->   {"text": "...", "language": "..."}
    GET  /health                          ->   "ok"

Run it with the project venv's Python:
    whisper-stt/.venv/bin/python whisper-stt/server.py

Model choices (set MODEL_SIZE below):
    tiny       ~75 MB  — fastest, lowest accuracy
    base       ~145 MB
    small      ~480 MB
    medium     ~1.5 GB
    large-v3-turbo  ~1.5 GB — fast + high accuracy, multilingual
    large-v3        ~3 GB  — best quality, slower

First run downloads the CT2-formatted model into ~/.cache/huggingface/.
For "large-v3-turbo" that's ~800 MB CT2 (separate from any .pt file you
have for openai-whisper).
"""

import io
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

from faster_whisper import WhisperModel

import os

# Default to "small" (244M params) — best speed/quality tradeoff on Apple
# Silicon CPU for voice commands. Override with JARVIS_WHISPER_MODEL env var:
#   tiny   ~39M params, ~0.3s/utterance, lower accuracy
#   base   ~74M params, ~0.5s
#   small  ~244M params, ~1s/utterance, multilingual — GOOD DEFAULT
#   medium ~769M, ~2-3s, better quality
#   large-v3-turbo ~809M, ~6s on M2 Pro CPU, best quality
MODEL_SIZE = os.environ.get("JARVIS_WHISPER_MODEL", "small").strip() or "small"
# Beam size: 1 = greedy (~30% faster), 5 = default beam search (best quality).
BEAM_SIZE = int(os.environ.get("JARVIS_WHISPER_BEAM", "1"))
PORT = 11436  # one above Kokoro's 11435, by convention.

# `device="cpu"` because we don't depend on Metal/CUDA for portability.
# `compute_type="int8"` quantizes weights for ~2x speedup on Apple Silicon
# with minimal quality loss. Bump to "float16" if you have a GPU.
print(f"[whisper] loading {MODEL_SIZE} (first run downloads model weights)…", flush=True)
_model = WhisperModel(MODEL_SIZE, device="cpu", compute_type="int8")

# Warm the model so the first real request isn't slow. A tiny dummy
# transcription primes all the buffers / kernels.
try:
    import numpy as np

    silence = np.zeros(16000, dtype=np.float32)  # 1 s of silence at 16 kHz
    for _ in _model.transcribe(silence, beam_size=1)[0]:
        pass
except Exception as e:  # noqa: BLE001
    print(f"[whisper] warmup skipped: {e}", flush=True)

print(f"[whisper] ready on :{PORT} (model={MODEL_SIZE}, beam={BEAM_SIZE})", flush=True)


def transcribe(wav_bytes: bytes, language: str | None = None) -> dict:
    """Transcribe in-memory WAV bytes. `language=None` auto-detects."""
    audio_io = io.BytesIO(wav_bytes)
    # `beam_size=5` is the faster-whisper default for accuracy. For voice
    # commands beam_size=1 (greedy) is ~30% faster with marginal quality
    # loss — set to 1 if you find transcription too slow.
    segments, info = _model.transcribe(
        audio_io,
        beam_size=BEAM_SIZE,
        language=language,  # None → auto-detect
        vad_filter=False,   # we already gated speech in Jarvis's VAD
    )
    text = " ".join(seg.text.strip() for seg in segments).strip()
    return {
        "text": text,
        "language": info.language,
        "language_probability": info.language_probability,
    }


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self._send(200, b"ok", "text/plain")
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path not in ("/transcribe", "/transcribe-auto"):
            self.send_error(404)
            return

        length = int(self.headers.get("Content-Length", 0))
        if length == 0:
            self.send_error(400, "empty body")
            return

        body = self.rfile.read(length)

        # Optional `X-Language` header: "auto" → detect, "" or absent → detect,
        # any ISO code → constrain to that language (e.g. "en", "uk", "ru").
        lang_hdr = (self.headers.get("X-Language") or "").strip().lower()
        language = None if lang_hdr in ("", "auto") else lang_hdr

        try:
            result = transcribe(body, language=language)
        except Exception as e:  # noqa: BLE001 — return errors to the client
            self.send_error(500, f"transcription failed: {e}")
            return

        payload = json.dumps(result).encode("utf-8")
        self._send(200, payload, "application/json")

    def _send(self, status: int, body: bytes, content_type: str):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass  # stay quiet; Jarvis logs timing itself


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
