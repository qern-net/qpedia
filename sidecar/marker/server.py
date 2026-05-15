"""
Qpedia Marker sidecar.

A thin HTTP wrapper around marker-pdf so the Rust extractor can
delegate high-fidelity PDF extraction (tables, equations, multi-column,
reading order) to it. Disabled by default; opt in by starting docker
compose with `--profile marker` and setting QPEDIA_MARKER_URL on the
app container.

API:
    GET  /healthz           -> "ok"
    POST /extract           multipart "file" -> { markdown, metadata }

Notes:
    - Model weights download to /root/.cache/marker on first request.
      To pre-warm, send a tiny PDF to /extract after startup.
    - CPU works; GPU recommended for anything beyond a couple of pages.
"""
from __future__ import annotations

import logging
import os
import tempfile
from typing import Any

from fastapi import FastAPI, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
log = logging.getLogger("marker-sidecar")

app = FastAPI(title="qpedia-marker", version="0.1")

# Lazy global converter — created on first request to avoid blocking
# /healthz during model download.
_converter: Any = None


def _ensure_converter() -> Any:
    global _converter
    if _converter is None:
        log.info("loading marker model dict (first request — may download weights)")
        # Imports here so module import doesn't pull torch+models when the
        # container is starting up just for /healthz.
        from marker.converters.pdf import PdfConverter
        from marker.models import create_model_dict
        _converter = PdfConverter(artifact_dict=create_model_dict())
        log.info("marker converter ready")
    return _converter


@app.get("/healthz")
def healthz() -> str:
    return "ok"


@app.post("/extract")
async def extract(file: UploadFile = File(...)) -> JSONResponse:
    raw = await file.read()
    if not raw:
        raise HTTPException(status_code=400, detail="empty file")

    converter = _ensure_converter()

    with tempfile.NamedTemporaryFile(delete=False, suffix=".pdf") as tmp:
        tmp.write(raw)
        path = tmp.name
    try:
        rendered = converter(path)
        # marker-pdf exposes a helper to flatten a rendered doc to text.
        from marker.output import text_from_rendered
        text, metadata, _images = text_from_rendered(rendered)
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass

    return JSONResponse({"markdown": text, "metadata": metadata or {}})
