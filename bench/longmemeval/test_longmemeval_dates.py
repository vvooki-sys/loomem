import unittest

from longmemeval_dates import to_iso


class ToIsoTests(unittest.TestCase):
    def test_dataset_pattern_maps_to_iso_date(self):
        self.assertEqual(to_iso("2023/05/20 (Sat) 02:21"), "2023-05-20")
        self.assertEqual(to_iso("2023/01/01 (Sun) 23:59"), "2023-01-01")
        self.assertEqual(to_iso("2024/02/29 (Thu) 00:00"), "2024-02-29")

    def test_iso_date_passes_through_unchanged(self):
        self.assertEqual(to_iso("2023-05-20"), "2023-05-20")
        self.assertEqual(to_iso("2026-10-21"), "2026-10-21")

    def test_near_misses_and_other_values_are_none_never_guessed(self):
        for value in ["2023/5/20", "2023/5/20 (Sat) 02:21", "2023/05/20 (Sat)", "2023/05/20",
                      "2023/05/20 (Sat) 2:21", "2023/05/20 (Sat) 02:21 ", " 2023/05/20 (Sat) 02:21",
                      "2023/05/20 (Fri) 02:21", "2023/05/20 (Sat) 24:00", "2023/05/20 (Sat) 02:60",
                      "2023/02/30 (Thu) 02:21", "2023-02-30", "2023-5-20", "20230520",
                      "May 20, 2023", "2023-05-20T00:00:00Z", "", None, 20230520, ["2023-05-20"]]:
            with self.subTest(value=value):
                self.assertIsNone(to_iso(value))


if __name__ == "__main__":
    unittest.main()
