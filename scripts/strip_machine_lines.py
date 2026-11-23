#!/usr/bin/env python3
"""Remove the date and substrate lines from every document's frontmatter.

git-lex no longer writes createdDate, updatedDate or substrate into
documents (goodlux, 2026-11-22): the store reads both dates from the
commits, and author replaces substrate. This takes the old lines out,
including the older spellings dateCreated and dateUpdated.

Only frontmatter is touched, only whole lines whose key is one of those
five names (bare, or dotted like soul.Journal.updatedDate). Templates
(__Class.md) and dot folders are skipped.

    strip_machine_lines.py [--write] <repo> [<repo> ...]

Without --write it only reports what it would remove.
"""
import os
import re
import sys

GONE = ("createdDate", "updatedDate", "substrate", "dateCreated", "dateUpdated")
KEY = re.compile(r"^(?:[A-Za-z0-9_-]+\.)*(%s)\s*:" % "|".join(GONE))


def strip(text):
    """The text without those lines, and how many were removed."""
    lines = text.split("\n")
    if not lines or lines[0].rstrip() != "---":
        return text, 0
    out, removed, inside = [lines[0]], 0, True
    for line in lines[1:]:
        if inside and line.rstrip() == "---":
            inside = False
        if inside and KEY.match(line):
            removed += 1
            continue
        out.append(line)
    if inside:  # frontmatter never closed: leave the file alone
        return text, 0
    return "\n".join(out), removed


def documents(repo):
    for folder, dirs, files in os.walk(repo):
        dirs[:] = sorted(d for d in dirs if not d.startswith(".") and d not in ("target", "node_modules"))
        for name in sorted(files):
            if name.endswith(".md") and not name.startswith("__"):
                yield os.path.join(folder, name)


def main():
    args = sys.argv[1:]
    write = "--write" in args
    repos = [a for a in args if a != "--write"]
    if not repos:
        sys.exit(__doc__)
    for repo in repos:
        files = lines = 0
        for path in documents(repo):
            try:
                with open(path, encoding="utf-8") as f:
                    text = f.read()
            except (OSError, UnicodeDecodeError):
                continue
            new, removed = strip(text)
            if not removed:
                continue
            files += 1
            lines += removed
            if write:
                with open(path, "w", encoding="utf-8") as f:
                    f.write(new)
        verb = "removed" if write else "would remove"
        print(f"{repo}: {verb} {lines} line(s) in {files} file(s)")


if __name__ == "__main__":
    main()
