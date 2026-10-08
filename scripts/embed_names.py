#!/usr/bin/env python3
"""External embedder for BuyWise's unmatched-line suggestion shortlist.

This is the default command for ``EMBEDDING_COMMAND`` (see
``crates/core/src/embedding.rs`` and ``buywise-cli embed-suggestions``). It runs
the text part of EmbeddingGemma 2 (Apache 2.0, 768 dimensions) on CPU, entirely
offline, and speaks a tiny JSON protocol on stdin/stdout so the Rust binary needs
no ML runtime and no native dependency:

  * stdin  : ``{"kind": "document"|"query", "model": <id>, "model_path": <path|null>,
               "dim": <int>, "texts": [<string>, ...]}``
  * stdout : ``{"dim": <int>, "vectors": [[<float>, ...], ...]}`` (one vector per
             input text, in order)

Everything that is not the JSON response goes to stderr, so the protocol is never
polluted.

The model weights are NOT part of this repository. Point the script at a local
copy with ``--model-path`` or the ``BUYWISE_EMBED_MODEL_PATH`` environment
variable (the embedder forwards ``EMBEDDING_MODEL_PATH`` there), or let
``transformers`` resolve the model id from its own cache (``HF_HOME``). The
weights and the venv live outside the repository and are never committed.

Requires ``transformers`` + ``torch`` (the bake-off used transformers 5.x on
CPU). Example:

    EMBEDDING_ENABLED=true \\
    EMBEDDING_COMMAND="python3 scripts/embed_names.py" \\
    EMBEDDING_MODEL_PATH=/path/to/embeddinggemma-2 \\
    HF_HOME=/path/to/model-cache \\
    cargo run -p buywise-cli -- embed-suggestions --db buywise.db --household-id <uuid>
"""

from __future__ import annotations

import argparse
import json
import os
import sys

# The model's own task prompts (from its config_sentence_transformers.json): a
# document (catalogue string) and a query (line name) are embedded differently,
# which is what the bake-off measured.
DOCUMENT_PROMPT = "title: none | text: "
QUERY_PROMPT = "task: search result | query: "

DEFAULT_MODEL = "google/embeddinggemma-2"
DEFAULT_DIM = 768
BATCH_SIZE = 32
MAX_TOKENS = 256


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--model-path",
        default=os.environ.get("BUYWISE_EMBED_MODEL_PATH"),
        help="Local model directory or id (default: $BUYWISE_EMBED_MODEL_PATH).",
    )
    parser.add_argument(
        "--threads",
        type=int,
        default=os.cpu_count(),
        help="CPU threads for torch (default: all cores).",
    )
    return parser.parse_args(argv)


def prompt_for(kind: str) -> str:
    if kind == "query":
        return QUERY_PROMPT
    if kind == "document":
        return DOCUMENT_PROMPT
    raise SystemExit(f"unknown kind {kind!r}: expected 'query' or 'document'")


def embed(texts: list[str], kind: str, model_id: str, threads: int) -> tuple[int, list[list[float]]]:
    # Imported lazily so `--help` and argument errors never require torch.
    import numpy as np
    import torch
    from transformers import AutoModel, AutoTokenizer

    torch.set_num_threads(max(1, threads))
    prompt = prompt_for(kind)

    tokenizer = AutoTokenizer.from_pretrained(model_id)
    model = AutoModel.from_pretrained(model_id, dtype=torch.float32).eval()

    vectors: list[list[float]] = []
    for start in range(0, len(texts), BATCH_SIZE):
        chunk = [prompt + text for text in texts[start : start + BATCH_SIZE]]
        encoded = tokenizer(
            chunk, padding=True, truncation=True, max_length=MAX_TOKENS, return_tensors="pt"
        )
        with torch.no_grad():
            hidden = model(**encoded).last_hidden_state
        mask = encoded["attention_mask"].unsqueeze(-1).float()
        pooled = (hidden * mask).sum(1) / mask.sum(1)
        pooled = torch.nn.functional.normalize(pooled, dim=-1)
        vectors.extend(pooled.float().numpy().tolist())
    dim = len(vectors[0]) if vectors else 0
    return dim, vectors


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    request = json.load(sys.stdin)
    texts = request.get("texts") or []
    kind = request.get("kind", "query")
    model_id = request.get("model_path") or args.model_path or request.get("model") or DEFAULT_MODEL
    expected_dim = int(request.get("dim") or DEFAULT_DIM)

    if not texts:
        json.dump({"dim": expected_dim, "vectors": []}, sys.stdout)
        return 0

    print(f"embedding {len(texts)} {kind} text(s) with {model_id}", file=sys.stderr)
    dim, vectors = embed(texts, kind, model_id, args.threads)
    json.dump({"dim": dim, "vectors": vectors}, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
