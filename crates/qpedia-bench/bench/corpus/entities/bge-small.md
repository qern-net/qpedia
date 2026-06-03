---
title: "bge-small-en-v1.5 embedding model"
kind: entity
tags: ["models", "embeddings"]
---

# bge-small-en-v1.5

The default embedding model Qpedia uses for vectorization. It produces
384-dimensional vectors and runs locally in-process via fastembed-rs
(ONNX Runtime). It is a bi-encoder: query and document are embedded
independently.
