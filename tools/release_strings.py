#!/usr/bin/env python3
"""Check that a release NRO carries none of the diagnostic build's log text (`diag ` lines, the planted-value and
forced-refusal settings). Exit 0 when clean, 1 with the hits listed.

Usage: python3 -I tools/release_strings.py target/aarch64-skyline-switch/release/libmemory_guard.nro
"""
import re
import sys

DIAG = re.compile(rb"diag (inject|hits|force_commit)|diag\.txt|force_free_failure|force_commit_failure|force-commit\.now")


def hits(data):
    return sorted({m.group().decode() for m in DIAG.finditer(data)})


def main():
    found = hits(open(sys.argv[1], "rb").read())
    for h in found:
        print("diag text in release:", h)
    print("clean" if not found else "%d diag string(s)" % len(found))
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
