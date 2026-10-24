import unittest

from textstat.core import DEFAULT_STOPWORDS, top_words

TEXT = "The cat sat on the mat. The cat!"


class TestTopWords(unittest.TestCase):
    def test_filters_stopwords_by_default(self):
        self.assertEqual(top_words(TEXT, n=2), [("cat", 2), ("mat", 1)])

    def test_stopwords_none_keeps_everything(self):
        self.assertEqual(
            top_words(TEXT, n=3, stopwords=None),
            [("the", 3), ("cat", 2), ("mat", 1)],
        )

    def test_ignores_case(self):
        counts = dict(top_words("Word word WORD", stopwords=None))
        self.assertEqual(counts, {"word": 3})

    def test_ties_sorted_alphabetically(self):
        self.assertEqual(
            top_words("b a a b c", n=3, stopwords=None),
            [("a", 2), ("b", 2), ("c", 1)],
        )

    def test_limits_to_n(self):
        self.assertEqual(len(top_words("a b c d e f", n=2, stopwords=None)), 2)

    def test_non_positive_n_returns_empty(self):
        self.assertEqual(top_words(TEXT, n=0), [])
        self.assertEqual(top_words(TEXT, n=-3), [])

    def test_ignores_punctuation_and_digits(self):
        self.assertEqual(
            top_words("123 don't don't abc!", stopwords=None),
            [("don't", 2), ("abc", 1)],
        )

    def test_empty_text(self):
        self.assertEqual(top_words(""), [])

    def test_default_stopwords_is_frozenset(self):
        self.assertIsInstance(DEFAULT_STOPWORDS, frozenset)


if __name__ == "__main__":
    unittest.main()
