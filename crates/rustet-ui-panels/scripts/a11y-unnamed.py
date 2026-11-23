#!/usr/bin/env python3
"""Reads a11y-dump.py output files; prints each interactive node without a name and exits 1
when there is one. Usage: a11y-unnamed.py FILE..."""
import json
import sys

INTERACTIVE = {"button", "push button", "toggle button", "switch", "combo box", "slider", "entry", "text",
               "page tab", "link", "check box", "radio button", "menu item", "list box",
               "spin button", "list item"}

bad = 0
for path in sys.argv[1:]:
    nodes = [json.loads(l) for l in open(path) if l.strip()]
    tree = [n for n in nodes if "role" in n]
    interactive = [n for n in tree if n["role"] in INTERACTIVE]
    unnamed = [n for n in interactive if not (n.get("name") or "").strip()]
    print(f"{path}: {len(tree)} nodes, {len(interactive)} interactive, {len(unnamed)} without a name")
    for n in unnamed:
        print("   unnamed:", n)
    if not tree:
        print("   (no tree: the app was not on the bus)")
        bad += 1
    bad += len(unnamed)
sys.exit(1 if bad else 0)
