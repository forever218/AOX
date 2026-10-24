"""textstat: 极简文本统计工具。"""

from .core import DEFAULT_STOPWORDS, char_count, line_count, top_words, word_count

__all__ = [
    "DEFAULT_STOPWORDS",
    "char_count",
    "line_count",
    "top_words",
    "word_count",
]
