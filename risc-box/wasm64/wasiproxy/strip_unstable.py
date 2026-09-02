#!/usr/bin/env python3
"""Drop every @unstable-gated item from a WIT file (the proxy exports only
the stable surface, and older wit parsers choke on gated world imports)."""
import sys, re
for path in sys.argv[1:]:
    lines = open(path).read().split("\n"); out = []; i = 0
    while i < len(lines):
        if lines[i].strip().startswith("@unstable"):
            # skip attribute lines, then the item: a brace block or a line ending in ';'
            while lines[i].strip().startswith("@"): i += 1
            if lines[i].rstrip().endswith("{"):
                depth = 0
                while True:
                    depth += lines[i].count("{") - lines[i].count("}"); i += 1
                    if depth == 0: break
            else:
                while not lines[i].rstrip().endswith(";"): i += 1
                i += 1
            continue
        out.append(lines[i]); i += 1
    open(path, "w").write("\n".join(out))
    print(path, "stripped")
