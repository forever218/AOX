"""Extract / apply translations of Chinese comment *blocks* in a source file.

Why blocks and not lines: translating a Chinese comment run into English often
needs a different number of lines.  A line-keyed replacement script silently
overwrites the code lines that follow (this actually happened once on
`gui/src/app.rs` and was only caught by an assertion).  Working on maximal runs
of comment lines -- and replacing them by exact text match -- is immune to that.

A "run" is a maximal sequence of consecutive lines that are Chinese comments.
Blank comment separators (`///` with nothing after) *inside* a run are kept, so
a doc paragraph stays one unit.

Usage:
    python tools/comment_blocks.py dump  <file> [--out blocks.json]
    python tools/comment_blocks.py apply <file> <pairs.json>
    python tools/comment_blocks.py count <file>

`pairs.json` is a JSON array of [["<original block>", "<english block>"], ...]
with "\n" between lines (the file's own line ending is restored on write).
"""

import json
import pathlib
import re
import sys

CJK = re.compile(r"[\u3000-\u303f\u4e00-\u9fff\uff00-\uffef]")

# Comment lead-in per file type. Rust/C-like use `//`; TOML/shell/Python use `#`;
# Inno Setup uses `;`; HTML uses `<!-- ... -->`.
COMMENT_STYLE = {
    ".rs": "slash",
    ".toml": "hash",
    ".iss": "semi",
    ".sh": "hash",
    ".ps1": "hash",
    ".py": "hash",
    ".html": "html",
}


def style_for(path):
    return COMMENT_STYLE.get(pathlib.Path(path).suffix.lower(), "slash")


def read_lines(path):
    """Return (lines, eol). `lines` never carry the line ending."""
    raw = pathlib.Path(path).read_bytes().decode("utf-8")
    eol = "\r\n" if b"\r\n" in pathlib.Path(path).read_bytes() else "\n"
    if eol == "\r\n":
        raw = raw.replace("\r\n", "\n")
    return raw.split("\n"), eol


def is_comment(line, style="slash"):
    s = line.lstrip()
    if style == "hash":
        return s.startswith("#")
    if style == "semi":
        return s.startswith(";")
    if style == "html":
        return s.startswith("<!--") or s.startswith("//") or s.startswith("-->")
    return s.startswith("//") or s.startswith("/*") or s.startswith("*")


def marker(line, style="slash"):
    """Return (prefix, body) for a comment line, else None."""
    if style == "hash":
        m = re.match(r"^(\s*#+\s?)(.*)$", line)
    elif style == "semi":
        m = re.match(r"^(\s*;\s?)(.*)$", line)
    elif style == "html":
        m = re.match(r"^(\s*(?:<!--|//|-->)\s?)(.*)$", line)
    else:
        m = re.match(r"^(\s*(?://[/!]?|/\*+|\*)\s?)(.*)$", line)
    return (m.group(1), m.group(2)) if m else None


def runs(lines, style="slash"):
    """Yield (start, end) 0-based half-open ranges of Chinese comment runs."""
    i, n = 0, len(lines)
    while i < n:
        if is_comment(lines[i], style) and CJK.search(lines[i]):
            j = i
            last_cjk = i
            # Extend over consecutive comment lines; keep blank comment
            # separators that are immediately followed by more CJK comments.
            while j + 1 < n and is_comment(lines[j + 1], style):
                j += 1
                if CJK.search(lines[j]):
                    last_cjk = j
            yield (i, last_cjk + 1)
            i = last_cjk + 1
        else:
            i += 1


def dump(path):
    lines, _ = read_lines(path)
    style = style_for(path)
    out = []
    for k, (a, b) in enumerate(runs(lines, style), 1):
        out.append({"i": k, "a": a + 1, "b": b, "text": "\n".join(lines[a:b])})
    return out


def apply_pairs(path, pairs):
    lines, eol = read_lines(path)
    text = "\n".join(lines)
    applied, errors = 0, []
    for idx, pair in enumerate(pairs, 1):
        if len(pair) != 2:
            errors.append("entry %d: expected [old, new]" % idx)
            continue
        old, new = pair
        cnt = text.count(old)
        if cnt != 1:
            errors.append("entry %d: found %d occurrences (need exactly 1); head=%r"
                          % (idx, cnt, old.split("\n")[0][:70]))
            continue
        text = text.replace(old, new, 1)
        applied += 1
    if errors:
        print("REFUSED -- nothing written. Problems:")
        for e in errors[:30]:
            print("  ", e)
        return 1
    pathlib.Path(path).write_bytes(text.replace("\n", eol).encode("utf-8"))
    print("applied %d/%d blocks to %s" % (applied, len(pairs), path))
    return 0


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    cmd, path = sys.argv[1], sys.argv[2]
    if cmd == "dump":
        data = dump(path)
        out = None
        if "--out" in sys.argv:
            out = sys.argv[sys.argv.index("--out") + 1]
        if out:
            pathlib.Path(out).write_text(
                json.dumps(data, ensure_ascii=False, indent=1), encoding="utf-8")
            print("wrote %d blocks -> %s" % (len(data), out))
        else:
            for d in data:
                print("### block %d  (lines %d-%d)" % (d["i"], d["a"], d["b"]))
                print(d["text"])
        return 0
    if cmd == "count":
        data = dump(path)
        total = sum(d["text"].count("\n") + 1 for d in data)
        print("%s: %d blocks, %d comment lines" % (path, len(data), total))
        return 0
    if cmd == "apply":
        pairs = json.loads(pathlib.Path(sys.argv[3]).read_text(encoding="utf-8"))
        return apply_pairs(path, pairs)
    if cmd == "apply-indexed":
        # Index-keyed mode: `<file> <blocks.json> <trans.json>` where trans.json is
        # {"<block index>": "<english block>"} and blocks.json is the dump output.
        # Lets the translator emit English only, without echoing the Chinese back.
        blocks = json.loads(pathlib.Path(sys.argv[3]).read_text(encoding="utf-8"))
        trans = json.loads(pathlib.Path(sys.argv[4]).read_text(encoding="utf-8"))
        by_i = {str(b["i"]): b["text"] for b in blocks}
        pairs, unknown = [], []
        for k, v in trans.items():
            if k not in by_i:
                unknown.append(k)
                continue
            pairs.append([by_i[k], v])
        if unknown:
            print("unknown block indexes:", unknown[:20])
            return 1
        print("translating %d of %d blocks" % (len(pairs), len(blocks)))
        return apply_pairs(path, pairs)
    print("unknown command", cmd)
    return 2


if __name__ == "__main__":
    sys.exit(main())
