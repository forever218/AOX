"""核心统计函数：纯文本进，数字出，无副作用。"""

from collections import Counter
import re

DEFAULT_STOPWORDS = frozenset(
    {"the", "a", "an", "and", "or", "of", "to", "in", "is", "it", "for", "on", "at"}
)

_WORD_RE = re.compile(r"[A-Za-z']+")


def word_count(text: str) -> int:
    """空白分隔的词数。"""
    return len(text.split())


def char_count(text: str, include_spaces: bool = True) -> int:
    """字符数；默认包含空白。"""
    if include_spaces:
        return len(text)
    return sum(1 for ch in text if not ch.isspace())


def line_count(text: str) -> int:
    """行数；空文本记 0 行，末尾换行不额外多算一行。"""
    if text == "":
        return 0
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return len(lines)


def top_words(
    text: str,
    n: int = 5,
    stopwords: frozenset[str] | None = DEFAULT_STOPWORDS,
) -> list[tuple[str, int]]:
    """出现次数最多的 n 个词。

    只取字母词（忽略标点与数字），统一转小写；stopwords 为 None 时不过滤。
    次数相同则按词本身排序，保证输出稳定。返回 [(词, 次数), ...]。
    """
    if n <= 0:
        return []
    words = [w.lower() for w in _WORD_RE.findall(text)]
    if stopwords is not None:
        words = [w for w in words if w not in stopwords]
    counts = Counter(words)
    return sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:n]
