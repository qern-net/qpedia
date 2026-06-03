---
title: "bge-reranker-v2-m3 cross-encoder"
kind: entity
tags: ["models", "reranking"]
---

# bge-reranker-v2-m3

The cross-encoder reranker Qpedia uses to reorder candidates after hybrid
fusion. Unlike the bi-encoder embedder, it processes the query and
document jointly, modeling token-level interaction, which yields higher
final relevance. It runs locally in-process and is multilingual.
