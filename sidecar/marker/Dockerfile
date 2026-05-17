# Marker sidecar — high-fidelity PDF extraction.
# Optional 3rd container. Built only when `docker compose --profile marker up`.
#
# Dependency note: marker-pdf pulls in torch → surya-ocr → triton (201 MB,
# GPU-only CUDA kernel compiler). We install the CPU-only torch wheel first
# from the PyTorch index so pip's resolver sees torch already satisfied and
# never fetches the CUDA build or triton.

FROM python:3.11-slim

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl ca-certificates libgomp1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Step 1: CPU-only torch — must come before marker-pdf so the resolver
# never considers the CUDA wheel or its triton dependency.
RUN pip install torch --index-url https://download.pytorch.org/whl/cpu

COPY requirements.txt ./

# Step 2: marker-pdf and the rest. torch is already satisfied above.
RUN pip install -r requirements.txt

COPY server.py ./

# Marker caches model weights under /root/.cache/marker — mount this
# as a volume in compose so they survive restarts.
VOLUME ["/root/.cache"]

EXPOSE 8000
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=30 \
    CMD curl -fsS http://localhost:8000/healthz || exit 1

CMD ["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "8000"]
