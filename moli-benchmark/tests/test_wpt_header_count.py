from __future__ import annotations

import tempfile
import unittest
from http.client import HTTPConnection
from pathlib import Path
from unittest.mock import patch

from moli_benchmark.wpt_cross.server import WptFixtureServer


class HeaderCountFixtureTests(unittest.TestCase):
    def test_more_than_250_request_and_response_headers_are_supported(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (directory / "resources").mkdir()
            (directory / "resources/testharness.js").write_text("// testharness")
            (directory / "headers.txt").write_text("ok")
            (directory / "headers.txt.headers").write_text(
                "\n".join(f"X-Response-{index}: value-{index}" for index in range(253))
            )
            with patch("moli_benchmark.wpt_cross.server._global_ipv6_address", return_value=None), WptFixtureServer(directory) as server:
                connection = HTTPConnection("127.0.0.1", server.port, timeout=2)
                self.addCleanup(connection.close)
                connection.request("GET", "/headers.txt", headers={
                    f"X-Request-{index}": f"value-{index}" for index in range(253)
                })
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(response.read(), b"ok")
                for index in range(253):
                    self.assertEqual(response.headers[f"X-Response-{index}"], f"value-{index}")

    def test_header_count_and_line_length_remain_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (directory / "resources").mkdir()
            (directory / "resources/testharness.js").write_text("// testharness")
            with patch("moli_benchmark.wpt_cross.server._global_ipv6_address", return_value=None), WptFixtureServer(directory) as server:
                for headers in ({f"val{index}": "value" for index in range(512)}, {"X-Long": "x" * (64 * 1024)}):
                    with self.subTest(header_count=len(headers)):
                        connection = HTTPConnection("127.0.0.1", server.port, timeout=2)
                        try:
                            connection.request("GET", "/fetch/api/resources/status.py", headers=headers)
                            response = connection.getresponse()
                            self.assertEqual(response.status, 431)
                            response.read()
                        finally:
                            connection.close()


if __name__ == "__main__":
    unittest.main()
