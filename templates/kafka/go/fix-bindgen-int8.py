#!/usr/bin/env python3
"""Post-process componentize-go/wit-bindgen 0.59 Go bindings.

The generator lowers enum discriminants as `int8(int32(N))`. The canonical ABI
stores them as u8, so any enum with more than 128 cases (cosmonic:kafka's
error-code has ~350) produces Go constant-overflow compile errors. Rewrite the
constant to its two's-complement int8 value. Idempotent.

Usage: fix-bindgen-int8.py <gen-dir>
"""
import re
import sys
from pathlib import Path

def fix(text: str) -> str:
    def repl(m):
        n = int(m.group(1))
        return f"int8(int32({n - 256}))" if n > 127 else m.group(0)
    return re.sub(r"int8\(int32\((\d+)\)\)", repl, text)

changed = 0
for path in Path(sys.argv[1]).rglob("*.go"):
    src = path.read_text()
    out = fix(src)
    if out != src:
        path.write_text(out)
        changed += 1
print(f"patched {changed} file(s)")
