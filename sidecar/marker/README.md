# Marker sidecar (optional 3rd container)

High-fidelity PDF extraction via [marker-pdf](https://github.com/datalab-to/marker).
Preserves tables, equations, multi-column layout, and reading order — the
things plain pdfium misses. Off by default; opt in when wiki quality on
complex business docs becomes the bottleneck.

## When to use this

Use the marker sidecar if your sources include:
- Tables you want the LLM to read row-by-row (invoices, reports).
- Multi-column technical / academic docs.
- Equations or formula-heavy material.
- Scanned PDFs with form fields and complex layouts.

Skip it if:
- Your corpus is mostly text-layer PDFs of single-column prose
  (pdfium's output is already clean).
- You don't have a third container's worth of resources to spare.
- You need air-gapped operation but can't pre-download model weights.

## Cost

- **Image size**: ~5 GB (PyTorch + marker models).
- **Cold start**: 30-90 s on first request (model download to
  `/root/.cache/marker`). Use a Docker volume so it survives restarts.
- **Per-PDF latency** on CPU: seconds to minutes depending on page count.
  GPU recommended for anything beyond a few pages.

## Run it

```bash
# Build + start, profile gates this container off by default.
docker compose --profile marker up -d marker

# Tell qpedia-api to use it.
export QPEDIA_MARKER_URL=http://marker:8000   # or http://127.0.0.1:18081 for local
```

The Rust extractor registers `MarkerExtractor` ahead of `PdfExtractor`
when `QPEDIA_MARKER_URL` is set; failures fall back to pdfium so a
broken sidecar doesn't take ingestion down.

## API

- `GET /healthz` -> `ok`
- `POST /extract` (multipart `file`) -> `{ markdown, metadata }`
