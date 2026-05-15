# Marker sidecar — high-fidelity PDF extraction.
# Optional 3rd container. Built only when `docker compose --profile marker up`.

FROM python:3.11-slim

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential git curl ca-certificates libgomp1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY requirements.txt ./
RUN pip install -r requirements.txt

COPY server.py ./

# Marker caches model weights under /root/.cache/marker — mount this
# as a volume in compose so they survive restarts.
VOLUME ["/root/.cache"]

EXPOSE 8000
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=30 \
    CMD curl -fsS http://localhost:8000/healthz || exit 1

CMD ["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "8000"]
