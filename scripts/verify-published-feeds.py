#!/usr/bin/env python3
"""Check update feed monotonic build advancement and post-deploy Pages parity."""

from __future__ import annotations

import argparse
import base64
import json
import sys
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request

FEED_URL = "https://rselbach.github.io/TableView/update.json"
MAX_FEED_BYTES = 64 * 1024


def extract_update_json(tar_path: str) -> bytes:
    with tarfile.open(tar_path) as archive:
        for member in archive.getmembers():
            name = member.name.removeprefix("./")
            if name == "update.json" and member.isfile():
                extracted = archive.extractfile(member)
                if extracted is not None:
                    return extracted.read()
    raise SystemExit("The Pages artifact must contain update.json")


def parse_release(feed_bytes: bytes) -> dict:
    try:
        envelope = json.loads(feed_bytes.decode("utf-8"))
        payload_b64 = envelope["payload"]
        payload = base64.b64decode(payload_b64, validate=True)
        release = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(f"Invalid update feed envelope: {error}") from error
    if not isinstance(release, dict) or not isinstance(release.get("build"), int) or release["build"] <= 0:
        raise SystemExit("Update feed payload does not contain a valid positive integer build")
    return release


def cache_busted_url(feed_url: str) -> str:
    parsed = urllib.parse.urlsplit(feed_url)
    sep = "&" if parsed.query else "?"
    return f"{feed_url}{sep}t={time.time_ns()}"


def fetch_feed(feed_url: str, max_bytes: int = MAX_FEED_BYTES) -> bytes | None:
    """Fetch feed_url, returning None on HTTP 404 and failing on other errors."""
    last_error: Exception | None = None
    for attempt in range(3):
        request = urllib.request.Request(
            cache_busted_url(feed_url),
            headers={"Cache-Control": "no-cache"},
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                data = response.read(max_bytes + 1)
                if len(data) > max_bytes:
                    raise SystemExit(f"Live feed exceeds {max_bytes} bytes")
                return data
        except urllib.error.HTTPError as error:
            code = error.code
            error.close()
            if code == 404:
                return None
            last_error = error
        except (OSError, urllib.error.URLError) as error:
            last_error = error
        if attempt < 2:
            time.sleep(2)
    raise SystemExit(f"Could not fetch live update feed {feed_url}: {last_error}")


def check_advance(tar_path: str, feed_url: str) -> None:
    wanted = extract_update_json(tar_path)
    candidate = parse_release(wanted)
    cand_build = candidate["build"]
    cand_version = candidate.get("version", "unknown")

    live_bytes = fetch_feed(feed_url)
    if live_bytes is None:
        print(f"No live feed at {feed_url} (HTTP 404); allowing initial publish of {cand_version} build {cand_build}")
        return

    live = parse_release(live_bytes)
    live_build = live["build"]
    live_version = live.get("version", "unknown")
    if cand_build <= live_build:
        raise SystemExit(
            f"Refusing to publish {cand_version} build {cand_build}: "
            f"live feed already serves {live_version} build {live_build}"
        )
    print(f"Advancing stable feed from {live_version} build {live_build} to {cand_version} build {cand_build}")


def verify_published(tar_path: str, feed_url: str, attempts: int = 12, retry_delay: float = 5.0) -> None:
    wanted = extract_update_json(tar_path)
    for attempt in range(attempts):
        request = urllib.request.Request(
            cache_busted_url(feed_url),
            headers={"Cache-Control": "no-cache"},
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                actual = response.read(len(wanted) + 1)
            if actual == wanted:
                print("Public update.json matches the signed release artifact")
                return
            print(f"Attempt {attempt + 1}: stale update.json", file=sys.stderr)
        except (OSError, urllib.error.URLError) as error:
            print(f"Attempt {attempt + 1}: {error}", file=sys.stderr)
        if attempt < attempts - 1:
            time.sleep(retry_delay)

    raise SystemExit("Pages reported success but is not serving the signed update.json")


def main() -> None:
    parser = argparse.ArgumentParser(description="Verify TableView update feed publication")
    parser.add_argument("artifact_tar", help="Path to Pages artifact tarball containing update.json")
    parser.add_argument(
        "--check-advance",
        action="store_true",
        help="Verify the candidate feed strictly advances the currently published build before deploying",
    )
    parser.add_argument("--feed-url", default=FEED_URL, help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.check_advance:
        check_advance(args.artifact_tar, args.feed_url)
    else:
        verify_published(args.artifact_tar, args.feed_url)


if __name__ == "__main__":
    main()
