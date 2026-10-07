"""String-literal i18n toolkit — the "copy phase" of AOX's English-source i18n.

The source literals become English; `locales/zh.toml` maps English -> Chinese.
These commands operate on raw source text (bytes in / bytes out, CRLF preserved,
no universal-newline folding — see the newline traps in .workbuddy/memory/MEMORY.md).

Commands:
  dump <file>                       list unique CJK string literals: index, occurrence
                                    count, line numbers, the exact source token (with
                                    quotes/escapes) and the first containing line.
  apply <file> <pairs.json>         replace exact source-text pairs, longest `old`
                                    first, all occurrences. A missing `old` is
                                    reported as MISS (file is still written with the
                                    pairs that did match — the final CJK scan is the
                                    safety net).
  accum <file> <pairs.json> <acc>   apply, plus record en->zh pairs into <acc> JSON
                                    (runtime-decoded strings, for zh.toml emission).
  toml <acc> <out.toml>             emit the accumulated catalogue as TOML.
"""

import json
import pathlib
import re
import sys

CJK = re.compile(r"[\u3000-\u303f\u4e00-\u9fff\uff00-\uffef]")
# A normal string literal (may span lines; escapes allowed inside).
STR = re.compile(r'"(?:[^"\\\n]|\\.)*"', re.S)
# Raw strings r"..." and r#"..."# etc.
RAW = re.compile(r'r(#+)"([\s\S]*?)"\1')


def read(path):
    b = pathlib.Path(path).read_bytes()
    return b, b.decode("utf-8")


def write(path, b, text):
    assert "\ufffd" not in text, "replacement char appeared — aborting"
    pathlib.Path(path).write_bytes(text.encode("utf-8"))


def decode_rust(s):
    """Decode a Rust string literal body (escapes) to the runtime string."""
    out, i, n = [], 0, len(s)
    while i < n:
        c = s[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        e = s[i + 1]
        simple = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", '"': '"',
                  "\\": "\\", "'": "'", "b": "\b", "f": "\f"}
        if e in simple:
            out.append(simple[e])
            i += 2
        elif e == "\n" or (e == "\r" and i + 2 < n and s[i + 2] == "\n"):
            # Rust line continuation: swallow the newline and the next line's indent
            i += 2 if e == "\n" else 3
            while i < n and s[i] in " \t":
                i += 1
        elif e == "u":
            j = s.index("}", i)
            out.append(chr(int(s[i + 3:j], 16)))
            i = j + 1
        elif e == "x":
            out.append(chr(int(s[i + 2:i + 4], 16)))
            i += 4
        else:  # line continuation \<newline> or unknown — keep verbatim
            out.append(e)
            i += 2
    return "".join(out)


def literals_with_cjk(text):
    """Yield (line_no, whole_token, inner_body) for CJK string literals."""
    hits, covered = [], [False] * len(text)
    for m in RAW.finditer(text):
        if CJK.search(m.group(0)):
            hits.append((m.start(), m.group(0), m.group(2)))
        for k in range(m.start(), m.end()):
            covered[k] = True
    for m in STR.finditer(text):
        if covered[m.start()]:
            continue
        if CJK.search(m.group(0)):
            hits.append((m.start(), m.group(0), m.group(0)[1:-1]))
    return hits


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def cmd_dump(path):
    _, text = read(path)
    seen = {}
    for pos, tok, body in literals_with_cjk(text):
        e = seen.setdefault(tok, {"n": 0, "lines": [], "ctx": None})
        e["n"] += 1
        ln = line_of(text, pos)
        e["lines"].append(ln)
        if e["ctx"] is None:
            line = text.split("\n")[ln - 1].strip()
            e["ctx"] = line[:170]
    print("=== %s : %d unique literals, %d occurrences ==="
          % (path, len(seen), sum(e["n"] for e in seen.values())))
    for k, (tok, e) in enumerate(sorted(seen.items(), key=lambda kv: kv[1]["lines"][0]), 1):
        print("[%d] x%d  L%s" % (k, e["n"], ",".join(map(str, e["lines"][:6]))))
        print("    %s" % tok[:300])
        print("    > %s" % e["ctx"])


def apply_pairs(text, pairs):
    """Apply [old, new] pairs longest-first; return (text, misses).

    Pairs may be written with plain \\n; they are normalized to the file's own newline
    style before matching (CRLF files get \\r\\n pairs, LF files get \\n pairs)."""
    nl = "\r\n" if "\r\n" in text else "\n"
    misses = []
    for pair in sorted(pairs, key=lambda p: -len(p[0].replace("\n", nl))):
        old, new = pair[0], pair[1]
        old, new = old.replace("\n", nl), new.replace("\n", nl)
        if old in text:
            text = text.replace(old, new)
        else:
            misses.append(old)
    return text, misses


def cmd_apply(path, pairs_path, acc_path=None):
    b, text = read(path)
    pairs = json.loads(pathlib.Path(pairs_path).read_text(encoding="utf-8"))
    text, misses = apply_pairs(text, pairs)
    write(path, b, text)
    if acc_path:
        acc = json.loads(pathlib.Path(acc_path).read_text(encoding="utf-8")) \
            if pathlib.Path(acc_path).exists() else {}
        before = b.decode("utf-8")
        for pair in pairs:
            if len(pair) < 3 or pair[0] not in before:
                continue  # 3rd element = the zh catalogue value for a wrapped string
            en = decode_rust(inner_of(pair[1]))
            if en and pair[2]:
                acc[en] = pair[2]
        pathlib.Path(acc_path).write_text(
            json.dumps(acc, ensure_ascii=False, indent=1, sort_keys=True), encoding="utf-8")
    if misses:
        print("MISS %d:" % len(misses))
        for m in misses[:12]:
            print("   ", m[:120])
    rest = len(CJK.findall(text))
    print("%s: applied, %d CJK chars remain" % (path, rest))
    return 1 if misses else 0


def inner_of(src):
    """Best-effort: the body of the first quoted literal inside `src`."""
    m = re.search(r'"((?:[^"\\]|\\.)*)"', src, re.S)
    return m.group(1) if m else src


def cmd_toml(acc_path, out_path):
    acc = json.loads(pathlib.Path(acc_path).read_text(encoding="utf-8"))
    # json.dumps' basic-string escaping (\\ \" \n \r \t \uXXXX) is TOML-compatible as-is;
    # ensure_ascii=False keeps the Chinese readable instead of \uXXXX soup.
    lines = ["# Generated by tools/str_l10n.py — English source -> Chinese.",
             "# Keys are the English literals in the source (must match character for",
             "# character); placeholders are positional {} with matching counts."]
    for k in sorted(acc):
        lines.append("%s = %s" % (json.dumps(k, ensure_ascii=False),
                                  json.dumps(acc[k], ensure_ascii=False)))
    pathlib.Path(out_path).write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("%s: %d entries" % (out_path, len(acc)))


def cmd_applymap(map_path, acc_path):
    """{file: [pairs...]} — apply to every file; 3-element pairs register (en, zh)."""
    mapping = json.loads(pathlib.Path(map_path).read_text(encoding="utf-8"))
    acc = json.loads(pathlib.Path(acc_path).read_text(encoding="utf-8")) \
        if pathlib.Path(acc_path).exists() else {}
    for f, pairs in mapping.items():
        b, text = read(f)
        text, misses = apply_pairs(text, pairs)
        write(f, b, text)
        before = b.decode("utf-8")
        for pair in pairs:
            if len(pair) >= 3 and pair[0] in before:
                acc[decode_rust(inner_of(pair[1]))] = pair[2]
        cjk = len(CJK.findall(text))
        print("%-26s applied=%2d miss=%d cjk_left=%d"
              % (f, len(pairs) - len(misses), len(misses), cjk))
        for m in misses:
            print("    MISS:", m[:100].replace("\n", " "))
    pathlib.Path(acc_path).write_text(
        json.dumps(acc, ensure_ascii=False, indent=1, sort_keys=True), encoding="utf-8")
    print("acc entries:", len(acc))


def main():
    cmd = sys.argv[1]
    if cmd == "dump":
        cmd_dump(sys.argv[2])
    elif cmd == "apply":
        sys.exit(cmd_apply(sys.argv[2], sys.argv[3]))
    elif cmd == "accum":
        sys.exit(cmd_apply(sys.argv[2], sys.argv[3], sys.argv[4]))
    elif cmd == "applymap":
        cmd_applymap(sys.argv[2], sys.argv[3])
    elif cmd == "toml":
        cmd_toml(sys.argv[2], sys.argv[3])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
