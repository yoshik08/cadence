"""Checks that the UI only uses design tokens. Run from the repo root: python scripts/lint_tokens.py

Outside ui/tokens.slint, a .slint file may not contain
  - a hex colour,
  - a px / ms / s literal, except 0px / 0ms, or as the value of a named property declaration
    (a size used by one component is declared once, at the top of that component).
Exit code 1 if anything is found.
"""
import os
import re
import sys

bad = 0
for name in sorted(os.listdir("ui")):
    if not name.endswith(".slint") or name == "tokens.slint":
        continue
    with open(os.path.join("ui", name), encoding="utf-8") as f:
        for n, line in enumerate(f.read().split("\n"), 1):
            code = re.sub(r'"[^"]*"', '""', line.split("//")[0])
            hexes = re.search(r"#[0-9a-fA-F]{3,8}\b", code)
            lits = [m for m in re.findall(r"(?<![\w.-])-?\d+(?:\.\d+)?(?:px|ms|s)\b", code) if m not in ("0px", "0ms")]
            declared = re.match(r"\s*(in|out|in-out|private)?\s*property\s*<", code)
            if hexes or (lits and not declared):
                print(f"ui/{name}:{n}: {line.strip()[:120]}")
                bad += 1
print(f"{bad} violation(s)")
sys.exit(1 if bad else 0)
