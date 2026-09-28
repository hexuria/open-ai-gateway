#!/usr/bin/env python3
"""mutants-diff.py -- keep moved-but-unchanged lines out of the mutation gate.

    git diff origin/main...HEAD | scripts/mutants-diff.py > pr.diff
    scripts/mutants-diff.py --selftest

`cargo mutants --in-diff` tests every mutant on an added line. Moving code
between files adds every moved line, so a pure move mutation-tests the whole
moved file (a split of repo.rs alone is ~250 mutants, hours past the CI
budget) while testing nothing that changed.

An added line whose exact text (ignoring leading and trailing whitespace, so
re-indentation still counts as moved) also appears as a removed line anywhere
in the same diff is treated as moved: it is turned into a context line, so
cargo-mutants skips it and every line number in the new file stays right.
Each removed line vouches for one added line only. A line edited while it
moved matches nothing and stays added, so the function it sits in is still
mutation-tested -- which is the point: a move is free, a change is not.

Blank lines are left alone either way; they carry no mutants.
"""

import re
import sys
from collections import Counter

HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$")


def key(line: str) -> str:
    return line[1:].strip()


def filter_diff(text: str) -> str:
    lines = text.split("\n")
    # Every removed line's text, as a pool that added lines draw from.
    pool = Counter(
        key(line)
        for line in lines
        if line.startswith("-") and not line.startswith("---") and key(line)
    )

    out = []
    i = 0
    while i < len(lines):
        m = HUNK.match(lines[i])
        if not m:
            out.append(lines[i])
            i += 1
            continue
        old_start, old_len = int(m.group(1)), int(m.group(2) or "1")
        new_start, new_len = int(m.group(3)), int(m.group(4) or "1")
        tail = m.group(5)
        body = []
        i += 1
        while i < len(lines) and not HUNK.match(lines[i]) and not lines[i].startswith("diff --git"):
            body.append(lines[i])
            i += 1
        # A trailing empty string is the file's final newline, not a line.
        trailing = []
        while body and body[-1] == "":
            trailing.append(body.pop())
        moved = 0
        for j, line in enumerate(body):
            if line.startswith("+") and key(line) and pool[key(line)] > 0:
                pool[key(line)] -= 1
                body[j] = " " + line[1:]
                moved += 1
        # A context line exists on both sides, so the old side grows by one
        # for every line turned into context. The new side is unchanged.
        out.append(f"@@ -{old_start},{old_len + moved} +{new_start},{new_len} @@{tail}")
        out.extend(body)
        out.extend(trailing)
    return "\n".join(out)


def selftest() -> None:
    before = (
        "diff --git a/a.rs b/a.rs\n"
        "--- a/a.rs\n"
        "+++ b/a.rs\n"
        "@@ -1,4 +1,1 @@\n"
        " fn keep() {}\n"
        "-fn moved(x: u32) -> u32 {\n"
        "-    x + 1\n"
        "-}\n"
        "diff --git a/b.rs b/b.rs\n"
        "new file mode 100644\n"
        "--- /dev/null\n"
        "+++ b/b.rs\n"
        "@@ -0,0 +1,6 @@\n"
        "+fn moved(x: u32) -> u32 {\n"
        "+    x + 1\n"
        "+}\n"
        "+fn edited(x: u32) -> u32 {\n"
        "+    x * 2\n"
        "+}\n"
    )
    after = filter_diff(before)
    added = [l for l in after.split("\n") if l.startswith("+") and not l.startswith("+++")]
    # The moved function's lines are context now; the new function's are not.
    # Its closing brace matches the moved one's only once, so it stays added.
    assert added == ["+fn edited(x: u32) -> u32 {", "+    x * 2", "+}"], added
    assert "@@ -0,3 +1,6 @@" in after, after
    # Re-indentation still counts as moved.
    indented = filter_diff(
        "@@ -1,1 +1,0 @@\n-    let y = 3;\n@@ -0,0 +1,1 @@\n+let y = 3;\n"
    )
    assert "+let y = 3;" not in indented, indented
    print("mutants-diff selftest: ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--selftest"]:
        selftest()
    else:
        sys.stdout.write(filter_diff(sys.stdin.read()))
