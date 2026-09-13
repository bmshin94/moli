from __future__ import annotations

import sys
import tempfile
import time
import unittest
from pathlib import Path

from moli_benchmark.config import _RESERVED_PORTS
from moli_benchmark.serve import MAX_SERVE_LOG_LINES
from moli_benchmark.wpt_cross.engine import EngineDriver


class WptEngineTests(unittest.TestCase):
    def test_verbose_engine_can_start_and_keep_serving(self) -> None:
        source = """
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

stream = getattr(sys, sys.argv[3])
noise = 'verbose engine output\\n' * 32768
stream.write(noise)
stream.flush()

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'{}')
        self.wfile.flush()
        stream.write(noise)
        stream.write('finished serving readiness\\n')
        stream.flush()
        Path(sys.argv[2]).write_text('done')

    def log_message(self, *args):
        pass

HTTPServer(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()
"""
        for stream_name in ("stdout", "stderr"):
            with self.subTest(stream=stream_name), tempfile.TemporaryDirectory() as directory:
                marker = Path(directory) / "finished"
                driver = EngineDriver(
                    name="verbose-fixture",
                    binary_env_var="WPT_VERBOSE_FIXTURE_BINARY",
                    default_binary_names=(),
                    build_command=lambda binary, port, _tmp: [
                        str(binary), "-u", "-c", source, str(port), str(marker), stream_name,
                    ],
                    version_args=("--version",),
                )
                handle = driver.launch(binary_override=sys.executable, ready_timeout_seconds=5)
                try:
                    deadline = time.monotonic() + 5
                    while not marker.exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertTrue(marker.exists(), "engine stalled while writing logs after startup")
                    self.assertIsNone(handle.process.poll())
                finally:
                    result = driver.shutdown(handle)
                    for stream in (handle.process.stdout, handle.process.stderr):
                        if stream is not None:
                            stream.close()
                self.assertIn(f"{stream_name}: finished serving readiness", result["log_tail"])
                self.assertLessEqual(len(handle.logs), MAX_SERVE_LOG_LINES)
                self.assertTrue(all(not thread.is_alive() for thread in handle.log_threads))
                self.assertNotIn(handle.port_lease.port, _RESERVED_PORTS)


if __name__ == "__main__":
    unittest.main()
