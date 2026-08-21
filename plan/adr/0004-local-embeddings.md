# ADR 0004 — Local embeddings (fastembed) by default

**Status:** accepted · **Date:** 2026-08-21

## Context
Memory needs embeddings, but Kevin holds no provider API keys in v1 (ADR 0003), and must work offline on a laptop.

## Decision
`Embedder` trait with `FastEmbedEmbedder` (ONNX, `BAAI/bge-small-en-v1.5`, 384 dims, model cached in `data_dir`) as default, run on the blocking pool; `NoopEmbedder` for tests/`memory.enabled=false`. Hybrid search = cosine + tsvector + importance decay. Changing the embedding model requires `kevin memory reindex`.

## Alternatives
- Provider embedding APIs (Voyage/OpenAI): better quality, needs keys and network; can be added as another `Embedder`.

## Consequences
First run downloads a small model; the image for Kohral pre-bakes it. Dimension is part of config/validation.
