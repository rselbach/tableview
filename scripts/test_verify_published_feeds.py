import base64
import http.server
import importlib.util
import io
import json
import os
import tarfile
import tempfile
import threading
import unittest


def load_module():
    script_path = os.path.join(os.path.dirname(__file__), "verify-published-feeds.py")
    spec = importlib.util.spec_from_file_location("verify_published_feeds", script_path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


VERIFY = load_module()


def make_feed(version: str, build: int, extra: str = "") -> bytes:
    payload = json.dumps(
        {
            "schema": 1,
            "app": "com.rselbach.tableview",
            "channel": "stable",
            "version": version,
            "build": build,
            "extra": extra,
        }
    ).encode("utf-8")
    envelope = {
        "payload": base64.b64encode(payload).decode("ascii"),
        "signature": "AAAA",
    }
    return (json.dumps(envelope, indent=2) + "\n").encode("utf-8")


def write_tar(path: str, feed_bytes: bytes) -> None:
    with tarfile.open(path, "w") as archive:
        info = tarfile.TarInfo("./update.json")
        info.size = len(feed_bytes)
        archive.addfile(info, io.BytesIO(feed_bytes))


class FeedServer:
    def __init__(self, status: int, body: bytes) -> None:
        self.status = status
        self.body = body
        parent = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                self.send_response(parent.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(parent.body)))
                self.end_headers()
                self.wfile.write(parent.body)

            def log_message(self, format: str, *args: object) -> None:
                return

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def __enter__(self) -> "FeedServer":
        self.thread.start()
        return self

    def __exit__(self, *args: object) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()

    @property
    def url(self) -> str:
        port = self.httpd.server_address[1]
        return f"http://127.0.0.1:{port}/update.json"


class VerifyPublishedFeedsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.orig_sleep = VERIFY.time.sleep
        VERIFY.time.sleep = lambda _secs: None

    def tearDown(self) -> None:
        VERIFY.time.sleep = self.orig_sleep

    def test_check_advance_allows_initial_404(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tar_path = os.path.join(tmp, "artifact.tar")
            write_tar(tar_path, make_feed("0.3.1", 7))
            with FeedServer(404, b"not found") as server:
                VERIFY.check_advance(tar_path, server.url)

    def test_check_advance_allows_higher_build(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tar_path = os.path.join(tmp, "artifact.tar")
            write_tar(tar_path, make_feed("0.3.1", 7))
            with FeedServer(200, make_feed("0.3.0", 6)) as server:
                VERIFY.check_advance(tar_path, server.url)

    def test_check_advance_rejects_identical_and_lower_or_same_build(self) -> None:
        feed = make_feed("0.3.0", 6)
        with tempfile.TemporaryDirectory() as tmp:
            tar_same_bytes = os.path.join(tmp, "identical.tar")
            write_tar(tar_same_bytes, feed)
            tar_old = os.path.join(tmp, "old.tar")
            write_tar(tar_old, make_feed("0.2.0", 5))
            tar_same = os.path.join(tmp, "same.tar")
            write_tar(tar_same, make_feed("0.3.0", 6, extra="rebuilt"))
            with FeedServer(200, feed) as server:
                with self.assertRaises(SystemExit):
                    VERIFY.check_advance(tar_same_bytes, server.url)
                with self.assertRaises(SystemExit):
                    VERIFY.check_advance(tar_old, server.url)
                with self.assertRaises(SystemExit):
                    VERIFY.check_advance(tar_same, server.url)

    def test_check_advance_fails_closed_on_server_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tar_path = os.path.join(tmp, "artifact.tar")
            write_tar(tar_path, make_feed("0.3.1", 7))
            with FeedServer(500, b"boom") as server:
                with self.assertRaises(SystemExit):
                    VERIFY.check_advance(tar_path, server.url)

    def test_verify_published_matches_and_fails_on_stale(self) -> None:
        feed = make_feed("0.3.1", 7)
        with tempfile.TemporaryDirectory() as tmp:
            tar_path = os.path.join(tmp, "artifact.tar")
            write_tar(tar_path, feed)
            with FeedServer(200, feed) as server:
                VERIFY.verify_published(tar_path, server.url, attempts=2, retry_delay=0.0)
            with FeedServer(200, make_feed("0.3.0", 6)) as stale:
                with self.assertRaises(SystemExit):
                    VERIFY.verify_published(tar_path, stale.url, attempts=2, retry_delay=0.0)


if __name__ == "__main__":
    unittest.main()
