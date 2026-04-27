# syntax=docker/dockerfile:1.7

ARG PDFIUM_VERSION=chromium/7802

# ---------- pdfium download ----------
FROM debian:bookworm-slim AS pdfium
ARG PDFIUM_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSLf "https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_VERSION}/pdfium-linux-x64.tgz" \
        -o /tmp/pdfium.tgz \
    && mkdir -p /opt/pdfium \
    && tar -xzf /tmp/pdfium.tgz -C /opt/pdfium \
    && rm /tmp/pdfium.tgz

# ---------- build ----------
FROM rust:1.81-bookworm AS build
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev libsqlite3-dev cmake clang \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY crates ./crates
RUN cargo fetch
RUN cargo build --release --bin qpedia-api

# ---------- frontend ----------
FROM node:22-bookworm AS web
WORKDIR /web
COPY web/package*.json ./
RUN [ -f package.json ] && npm ci || echo "no frontend yet"
COPY web ./
RUN [ -f package.json ] && npm run build || mkdir -p build

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates tini \
        tesseract-ocr tesseract-ocr-eng \
        pandoc \
        libssl3 libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -u 10001 -m qpedia
WORKDIR /app

COPY --from=build   /build/target/release/qpedia-api /usr/local/bin/qpedia-api
COPY --from=web     /web/build /app/web
COPY --from=pdfium  /opt/pdfium/lib/libpdfium.so /usr/local/lib/libpdfium.so
RUN ldconfig

RUN mkdir -p /data/wiki /data/raw /data/sqlite /data/models && chown -R qpedia:qpedia /data /app

USER qpedia
EXPOSE 8080
ENV QPEDIA_DATA_DIR=/data
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["qpedia-api"]
