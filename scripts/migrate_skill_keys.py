#!/usr/bin/env python3
"""Rename skill frontmatter keys to the names on git-lex:Skill (git-lex.ttl 0.23.0).

  <kit>.Skill.skillDescription   -> <kit>.Skill.description
  <kit>.Skill.skillInvocability  -> <kit>.Skill.invocability
  <kit>.Skill.skillAllowedTools  -> <kit>.Skill.allowedTools
  <kit>.Skill.skillArgumentHint  -> <kit>.Skill.argumentHint
  <kit>.Skill.skillId            -> removed (<kit>.Skill.id carries the identity;
                                    written from skillId when the file has none)

Reads every Skill/*.md and */Skill/*.md of each repository given, frontmatter
only. Files whose name starts with `__` (kit templates) are left to the kit.
Without --write it only reports.

    migrate_skill_keys.py [--write] <repo> [<repo> ...]
"""
import re, sys
from pathlib import Path

RENAMES = {"skillDescription": "description", "skillInvocability": "invocability",
           "skillAllowedTools": "allowedTools", "skillArgumentHint": "argumentHint"}
KEY = re.compile(r"^(\s*)([A-Za-z0-9_-]+)\.Skill\.(\w+)(\s*:.*)$")

def migrate(text):
    if not text.startswith("---\n"):
        return text
    end = text.find("\n---", 4)
    if end < 0:
        return text
    lines = text[4:end].split("\n")
    present = {m.group(3) for m in map(KEY.match, lines) if m}
    out = []
    for line in lines:
        m = KEY.match(line)
        if not m:
            out.append(line); continue
        indent, kit, prop, rest = m.groups()
        if prop in RENAMES:
            if RENAMES[prop] not in present:          # the new key wins where both exist
                out.append(f"{indent}{kit}.Skill.{RENAMES[prop]}{rest}")
        elif prop == "skillId":
            if "id" not in present:
                value = rest.split(":", 1)[1].strip().strip('"')
                out.append(f"{indent}{kit}.Skill.id: <{kit}/Skill/{value}>")
        else:
            out.append(line)
    return "---\n" + "\n".join(out) + text[end:]

def main():
    write = "--write" in sys.argv
    repos = [Path(a) for a in sys.argv[1:] if a != "--write"]
    total = 0
    for repo in repos:
        files = sorted(set(repo.glob("Skill/*.md")) | set(repo.glob("*/Skill/*.md")))
        changed = 0
        for f in files:
            if f.name.startswith("__") or f.parts[len(repo.parts)].startswith("."):
                continue
            old = f.read_text()
            new = migrate(old)
            if new != old:
                changed += 1
                if write:
                    f.write_text(new)
        total += changed
        print(f"{'rewrote' if write else 'would rewrite'} {changed:3} of {len(files):3} skill files in {repo}")
    print(f"total: {total}")

if __name__ == "__main__":
    main()
