"""A log file stays within its limit."""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import pcmlink  # noqa: E402


class TrimLog(unittest.TestCase):
    def write(self, text):
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as fh:
            fh.write(text)
        self.addCleanup(os.remove, fh.name)
        return fh.name

    def test_keeps_the_newest_half_on_a_line_boundary(self):
        path = self.write("".join(f"line {i:05}\n" for i in range(2000)))
        pcmlink.trim_log(path, 8000)
        with open(path) as fh:
            kept = fh.read()
        self.assertLessEqual(len(kept), 4000)
        self.assertTrue(kept.startswith("line "))
        self.assertTrue(kept.endswith("line 01999\n"))

    def test_under_the_limit_nothing_changes(self):
        path = self.write("short\n")
        pcmlink.trim_log(path, 8000)
        with open(path) as fh:
            self.assertEqual(fh.read(), "short\n")

    def test_a_missing_file_is_fine(self):
        pcmlink.trim_log("/nonexistent/pcmlink.log")


if __name__ == "__main__":
    unittest.main()
