#!/usr/bin/env python3
"""Prints the accessibility tree a process exposes on AT-SPI, one JSON line per node with a
role and a name: {"depth", "role", "name", "states"}. Usage: a11y-dump.py PID"""
import json
import sys

import pyatspi


def walk(node, depth, out):
    try:
        role, name = node.getRoleName(), node.name
        states = [pyatspi.stateToString(s) for s in node.getState().getStates()
                  if s in (pyatspi.STATE_SELECTED, pyatspi.STATE_FOCUSED, pyatspi.STATE_FOCUSABLE)]
    except Exception as e:  # a node that went away while walking
        out.append({"depth": depth, "error": str(e)})
        return
    out.append({"depth": depth, "role": role, "name": name, "states": states})
    for i in range(min(node.childCount, 400)):
        child = node.getChildAtIndex(i)
        if child is not None:
            walk(child, depth + 1, out)


def main(pid):
    desktop = pyatspi.Registry.getDesktop(0)
    apps = [a for a in desktop if a is not None and a.get_process_id() == pid]
    print(json.dumps({"apps_on_bus": [a.name for a in desktop if a is not None], "matched": len(apps)}))
    for app in apps:
        out = []
        walk(app, 0, out)
        for line in out:
            print(json.dumps(line, ensure_ascii=False))


if __name__ == "__main__":
    main(int(sys.argv[1]))
