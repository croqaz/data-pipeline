#!/usr/bin/env python3
"""Train the two tiny tokenizers the tests count tokens with.

Both are byte-level BPE like the real ones (no normalizer, ByteLevel
pre-tokenizer and decoder), trained on the golden fixture texts, and differ
only in vocabulary size, so they give different counts and hashes:

    tests/fixtures/tok-a/tokenizer.json   300 tokens
    tests/fixtures/tok-b/tokenizer.json   400 tokens

Needs `pip install tokenizers`. Run from the repo root:
    python3 tests/make_tokenizers.py
"""

import json
from pathlib import Path

from tokenizers import Tokenizer, decoders, models, pre_tokenizers, trainers

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"


def texts():
    with open(FIXTURES / "golden.jsonl", encoding="utf-8") as f:
        return [json.loads(line)["text"] for line in f]


def train(name, vocab_size):
    tok = Tokenizer(models.BPE())
    tok.pre_tokenizer = pre_tokenizers.ByteLevel(add_prefix_space=False)
    tok.decoder = decoders.ByteLevel()
    trainer = trainers.BpeTrainer(
        vocab_size=vocab_size,
        special_tokens=["<|bos|>", "<|eos|>"],
        initial_alphabet=pre_tokenizers.ByteLevel.alphabet(),
        show_progress=False,
    )
    tok.train_from_iterator(texts(), trainer)
    out = FIXTURES / name
    out.mkdir(parents=True, exist_ok=True)
    tok.save(str(out / "tokenizer.json"))
    print(f"{name}: {tok.get_vocab_size()} tokens")


if __name__ == "__main__":
    train("tok-a", 300)
    train("tok-b", 400)
