import unittest

from textstat.core import char_count, line_count, word_count


class TestWordCount(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(word_count(""), 0)

    def test_whitespace_only(self):
        self.assertEqual(word_count("  \n\t "), 0)

    def test_simple(self):
        self.assertEqual(word_count("hello world"), 2)

    def test_extra_spaces(self):
        self.assertEqual(word_count("  a   b\tc\nd "), 4)


class TestCharCount(unittest.TestCase):
    def test_with_spaces(self):
        self.assertEqual(char_count("a b"), 3)

    def test_without_spaces(self):
        self.assertEqual(char_count("a b\nc", include_spaces=False), 3)


class TestLineCount(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(line_count(""), 0)

    def test_single_line(self):
        self.assertEqual(line_count("abc"), 1)

    def test_trailing_newline_not_counted(self):
        self.assertEqual(line_count("a\nb\n"), 2)

    def test_blank_line_in_middle_counted(self):
        self.assertEqual(line_count("a\n\nb"), 3)


if __name__ == "__main__":
    unittest.main()
