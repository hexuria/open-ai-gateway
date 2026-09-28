#!/usr/bin/env python3
"""mutants-diff.py -- keep moved-but-unchanged code out of the mutation gate.

    scripts/mutants-diff.py origin/main...HEAD > pr.diff
    scripts/mutants-diff.py --selftest

`cargo mutants --in-diff` tests every mutant on a changed line. Moving code
between files changes every moved line, so a pure move mutation-tests the
whole moved file (a split of repo.rs alone is ~250 mutants, hours past the CI
budget) while testing nothing that changed.

Which lines moved is git's call, not ours: `git diff --color-moved=blocks`
marks a block as moved only when it reappears unchanged somewhere else in the
diff and holds at least 20 alphanumeric characters, so a lone `Ok(())` or
`return 1` that merely repeats deleted text is never taken for a move.
`--color-moved-ws=allow-indentation-change` still matches a block that was
re-indented as it moved.

This script runs that diff itself (so a failed `git diff` fails the script,
not an empty run) and prints a new, minimal diff naming only the lines that
really changed, in the new files:

- every added line git did not mark as moved;
- the new-file lines either side of every removed line git did not mark as
  moved (what cargo-mutants itself counts for a deletion).

Each run of such lines is written as an insert-only hunk, `@@ -N,0 +N,len @@`.
Hunks never overlap and stay in order by construction, so the output always
parses. Every inserted line carries the new file's real text at that number,
because cargo-mutants compares it with the tree and rejects the whole diff
(exit 5) on any mismatch; a line past the end of the file is never written.
"""

import os
import re
import subprocess
import sys
import tempfile

ANSI = re.compile(r"\x1b\[[0-9;]*m")
# Colours forced on the command line, so a user's git config cannot change
# what the parser sees.
NEW, NEW_MOVED, OLD, OLD_MOVED = "32", "34", "31", "35"
COLOURS = [
    "-c", "color.diff.new=green", "-c", "color.diff.newMoved=blue",
    "-c", "color.diff.old=red", "-c", "color.diff.oldMoved=magenta",
    "-c", "color.diff.whitespace=normal",
]
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")


def first_code(raw: str) -> str:
    m = re.match(r"\x1b\[([0-9;]*)m", raw)
    return m.group(1).split(";")[-1] if m else ""


def git_diff(args, cwd=None) -> str:
    return subprocess.run(
        ["git", *COLOURS, "diff", "--no-ext-diff", "--color=always",
         "--color-moved=blocks", "--color-moved-ws=allow-indentation-change",
         "--ws-error-highlight=none", "--unified=3", *args],
        cwd=cwd, check=True, capture_output=True, text=True,
    ).stdout


def changed_lines(coloured: str) -> dict:
    """{new-file path: {line number: text}} for lines that really changed.

    The text is always the line as it stands in the new file (from the diff's
    own `+` and context lines), because cargo-mutants checks it against the
    tree and refuses the whole diff on a mismatch.
    """
    texts, marks, path, new_ln = {}, {}, None, 0
    for raw in coloured.split("\n"):
        text = ANSI.sub("", raw)
        if text.startswith("diff --git "):
            path = None
            continue
        if text.startswith("+++ "):
            target = text[4:]
            path = None if target == "/dev/null" else target[2:]
            if path:
                texts.setdefault(path, {})
                marks.setdefault(path, set())
            continue
        if text.startswith("--- "):
            continue
        m = HUNK.match(text)
        if m:
            new_ln = int(m.group(1))
            continue
        if path is None or not text:
            continue
        kind, code = text[0], first_code(raw)
        if kind == " ":
            texts[path][new_ln] = text[1:]
            new_ln += 1
        elif kind == "+":
            texts[path][new_ln] = text[1:]
            if code != NEW_MOVED:
                marks[path].add(new_ln)
            new_ln += 1
        elif kind == "-" and code != OLD_MOVED:
            # cargo-mutants counts a deletion against the lines either side.
            marks[path].update((new_ln - 1, new_ln))
    return {
        p: {n: texts[p][n] for n in marks[p] if n in texts[p]}
        for p in texts
    }


def render(files: dict) -> str:
    out = []
    for path in sorted(files):
        lines = files[path]
        if not lines:
            continue
        out += [f"diff --git a/{path} b/{path}", f"--- a/{path}", f"+++ b/{path}"]
        numbers = sorted(lines)
        start = prev = numbers[0]
        run = [start]
        for n in numbers[1:] + [None]:
            if n is not None and n == prev + 1:
                run.append(n)
                prev = n
                continue
            out.append(f"@@ -{start},0 +{start},{len(run)} @@")
            out += ["+" + lines[k] for k in run]
            if n is not None:
                start = prev = n
                run = [n]
    return "\n".join(out) + ("\n" if out else "")


def selftest() -> None:
    fn_b = ["fn b(values: &[u32]) -> u32 {", "    let total: u32 = values.iter().sum();",
            "    total.saturating_mul(2)", "}"]
    fn_c = ["fn c(name: &str) -> String {", "    let greeting = format!(\"hello {name}\");",
            "    greeting.to_uppercase()", "}"]
    with tempfile.TemporaryDirectory() as d:
        def run(*a):
            subprocess.run(["git", *a], cwd=d, check=True, capture_output=True)

        def write(name, lines):
            with open(os.path.join(d, name), "w") as f:
                f.write("\n".join(lines) + "\n")

        run("init", "-q"); run("config", "user.email", "t@t"); run("config", "user.name", "t")
        pad = [f"// filler line {i} keeps hunks apart" for i in range(12)]
        write("a.rs", ["fn a() -> u32 {", "    return 1", "}", *pad, *fn_b, *pad, *fn_c])
        write("b.rs", ["// b"])
        run("add", "."); run("commit", "-qm", "base")

        def matches_tree(got):
            # What cargo-mutants checks before it runs anything: every line
            # the diff inserts is that line of the file, word for word.
            for path, lines in got.items():
                with open(os.path.join(d, path)) as f:
                    tree = f.read().split("\n")[:-1]
                for n, text in lines.items():
                    assert 1 <= n <= len(tree), f"{path}:{n} is past the end: {got}"
                    assert tree[n - 1] == text, f"{path}:{n} is {tree[n - 1]!r}, diff says {text!r}"
            return got

        def case(a_lines, b_lines):
            write("a.rs", a_lines); write("b.rs", b_lines)
            return matches_tree(changed_lines(git_diff([], cwd=d)))

        # 1. A pure move: nothing changed, nothing to test.
        got = case(["fn a() -> u32 {", "    return 1", "}", *pad, *pad, *fn_c], ["// b", *fn_b])
        assert got == {"a.rs": {}, "b.rs": {}}, f"pure move: {got}"

        # 2. An edit made while moving stays a change; the unchanged block it
        #    moved with does not. (A block too small for git's 20-character
        #    rule, like a lone closing brace after the edit, counts as changed:
        #    the filter errs toward testing more, never less.)
        edited = fn_b[:2] + ["    total.saturating_mul(3)"] + fn_b[3:]
        got = case(["fn a() -> u32 {", "    return 1", "}", *pad, *pad, *fn_c], ["// b", *edited])
        assert 4 in got["b.rs"], f"the edit was dropped: {got}"
        assert 2 not in got["b.rs"] and 3 not in got["b.rs"], f"moved lines kept: {got}"

        # 3. The review's case: fn a is deleted and a moved line is changed to
        #    text fn a had. Not a move -- it must stay a change.
        short_b = ["fn b() -> u32 {", "    return 1", "}"]
        write("a.rs", ["fn a() -> u32 {", "    return 1", "}", *pad, "fn b() -> u32 {", "    return 2", "}"])
        write("b.rs", ["// b"]); run("add", "."); run("commit", "-qm", "short")
        got = case([*pad], ["// b", *short_b])
        assert got["b.rs"], f"a changed line matching deleted text was hidden: {got}"

        # 4. A plain deletion marks the lines either side of it, with their
        #    real text (cargo-mutants checks it), as cargo-mutants does.
        write("a.rs", [*pad, *fn_c]); write("b.rs", ["// b"]); run("add", "."); run("commit", "-qm", "c")
        got = case([*pad, fn_c[0], fn_c[2], fn_c[3]], ["// b"])
        assert sorted(got["a.rs"]) == [len(pad) + 1, len(pad) + 2], f"plain deletion: {got}"

        # 5. The review's other shape: a block moved within the same file,
        #    plus an edit elsewhere in it. The edit is kept, the move is not,
        #    and the hunks come out in order.
        write("a.rs", [*fn_b, *pad, *fn_c]); write("b.rs", ["// b"]); run("add", "."); run("commit", "-qm", "d")
        edited_c = fn_c[:2] + ["    greeting.to_lowercase()"] + fn_c[3:]
        got = case([*pad, *edited_c, *pad, *fn_b], ["// b"])
        lines = got["a.rs"]
        assert len(pad) + 3 in lines, f"the edit near a move was dropped: {got}"
        moved_at = range(2 * len(pad) + 5, 2 * len(pad) + 9)
        assert not any(n in lines for n in moved_at), f"moved block kept: {got}"
        heads = [h for h in render(got).split("\n") if h.startswith("@@")]
        starts = [int(h.split("+")[1].split(",")[0]) for h in heads]
        assert starts == sorted(starts), f"hunks out of order: {heads}"

        # 6. A block removed once and added twice counts as moved both times
        #    (git's rule, pinned here so a change to it is noticed). A copy of
        #    code that still exists elsewhere is not a move and is kept.
        write("a.rs", [*fn_b, *pad]); write("b.rs", ["// b"]); run("add", "."); run("commit", "-qm", "e")
        got = case([*pad], ["// b", *fn_b, "// between", *fn_b])
        copies = [*range(2, 6), *range(7, 11)]
        assert not any(n in got["b.rs"] for n in copies), f"a twice-moved block was kept: {got}"
        got = case([*fn_b, *pad], ["// b", *fn_b])
        assert got["b.rs"], f"a copy (original kept) was taken for a move: {got}"

        # 8. The reviews' repros, each of which once wrote a line whose text
        #    was not the tree's: a function moved out from between two
        #    others (blank lines around it), a deletion above a blank line,
        #    and deleting the last line of a file (nothing past the end).
        first = ["fn first(x: u32) -> bool {", "    let limit_value = x > 2;", "    limit_value", "}"]
        last = ["fn last(y: u32) -> u32 {", "    y.saturating_add(40)", "}"]
        write("a.rs", [*first, "", *fn_b, "", *last]); write("b.rs", ["// b"])
        run("add", "."); run("commit", "-qm", "f")
        got = case([*first, "", *last], ["// b", "", *fn_b])
        assert not any(n in got["b.rs"] for n in range(3, 7)), f"a pure mid-file move was kept: {got}"
        got = case([first[0], first[2], first[3], "", *fn_b, "", *last], ["// b"])
        assert sorted(got["a.rs"]) == [1, 2], f"deletion above a blank: {got}"
        got = case([*first, "", *fn_b, "", *last[:-1]], ["// b"])
        assert sorted(got["a.rs"]) == [len(first) + len(fn_b) + 4], f"deleted last line: {got}"

    # 7. Output always parses: runs never overlap and stay in order.
    rendered = render({"x.rs": {3: "a", 4: "b", 9: "c", 10: "d", 11: ""}})
    heads = [l for l in rendered.split("\n") if l.startswith("@@")]
    assert heads == ["@@ -3,0 +3,2 @@", "@@ -9,0 +9,3 @@"], heads
    assert render({"y.rs": {}}) == "", "a file with nothing changed writes nothing"
    print("mutants-diff selftest: ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--selftest"]:
        selftest()
    elif len(sys.argv) == 2:
        sys.stdout.write(render(changed_lines(git_diff([sys.argv[1]]))))
    else:
        sys.exit("usage: mutants-diff.py <base>...HEAD | --selftest")
