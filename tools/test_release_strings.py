"""Host tests for release_strings.py. Run: python3 -I tools/test_release_strings.py"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import release_strings  # noqa: E402


class ReleaseStrings(unittest.TestCase):
    def test_the_diag_texts_are_found(self):
        data = b"\0{} diag inject site=\0x" + b"sd:/ultimate/memory-guard/diag.txt\0force_commit_failure=\0"
        self.assertEqual(release_strings.hits(data),
                         ["diag inject", "diag.txt", "force_commit_failure"])

    def test_release_text_is_clean(self):
        data = (b"guard site= verdict= alloc_pages_refused caller=main+ free_pages_refused abort caller="
                b" plugin=memory-guard/ alloc_pages=known diagnostics")
        self.assertEqual(release_strings.hits(data), [])


if __name__ == "__main__":
    unittest.main()
