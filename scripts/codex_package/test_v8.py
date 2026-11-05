import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.error import URLError

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package.v8 import download_file


def response(body: bytes, *, content_length: int | None = None) -> io.BytesIO:
    stream = io.BytesIO(body)
    stream.headers = {
        "Content-Length": str(len(body) if content_length is None else content_length)
    }
    return stream


class DownloadFileTest(unittest.TestCase):
    def test_retries_gateway_timeout_and_installs_complete_file(self) -> None:
        url = "https://example.com/v8.sha256"
        with tempfile.TemporaryDirectory() as directory:
            dest = Path(directory) / "v8.sha256"
            with (
                patch(
                    "codex_package.v8.urlopen",
                    side_effect=[
                        HTTPError(url, 504, "Gateway Time-out", {}, None),
                        response(b"checksums"),
                    ],
                ) as fetch,
                patch("codex_package.v8.time.sleep") as sleep,
            ):
                download_file(url, dest)
            self.assertEqual(dest.read_bytes(), b"checksums")
            self.assertEqual(fetch.call_count, 2)
            sleep.assert_called_once_with(2)
            self.assertFalse(dest.with_suffix(".sha256.tmp").exists())

    def test_retries_truncated_response_without_installing_partial_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            dest = Path(directory) / "v8.gz"
            dest.write_bytes(b"previous download")
            with (
                patch(
                    "codex_package.v8.urlopen",
                    side_effect=[
                        response(b"partial", content_length=100),
                        response(b"complete"),
                    ],
                ),
                patch("codex_package.v8.time.sleep"),
            ):
                download_file("https://example.com/v8.gz", dest)
            self.assertEqual(dest.read_bytes(), b"complete")
            self.assertFalse(dest.with_suffix(".gz.tmp").exists())

    def test_missing_release_fails_without_retrying_or_overwriting_cache(self) -> None:
        url = "https://example.com/missing.sha256"
        with tempfile.TemporaryDirectory() as directory:
            dest = Path(directory) / "v8.sha256"
            dest.write_bytes(b"cached")
            with (
                patch(
                    "codex_package.v8.urlopen",
                    side_effect=HTTPError(url, 404, "Not Found", {}, None),
                ) as fetch,
                patch("codex_package.v8.time.sleep") as sleep,
            ):
                with self.assertRaisesRegex(RuntimeError, "HTTP 404"):
                    download_file(url, dest)
            self.assertEqual(dest.read_bytes(), b"cached")
            fetch.assert_called_once()
            sleep.assert_not_called()

    def test_network_failure_stops_after_five_attempts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            dest = Path(directory) / "v8.gz"
            with (
                patch(
                    "codex_package.v8.urlopen",
                    side_effect=URLError("connection failed"),
                ) as fetch,
                patch("codex_package.v8.time.sleep") as sleep,
            ):
                with self.assertRaisesRegex(RuntimeError, "after 5 attempts"):
                    download_file("https://example.com/v8.gz", dest)
            self.assertEqual(fetch.call_count, 5)
            self.assertEqual(
                [call.args[0] for call in sleep.call_args_list], [2, 4, 8, 16]
            )
            self.assertFalse(dest.exists())
            self.assertFalse(dest.with_suffix(".gz.tmp").exists())
