#!/usr/bin/env python3
"""Check that GitHub Pages serves the exact signed update feed uploaded by this release."""

import sys
import tarfile
import time
import urllib.error
import urllib.request

FEED_URL = "https://rselbach.github.io/tableview/update.json"


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} ARTIFACT_TAR")

    wanted = None
    with tarfile.open(sys.argv[1]) as archive:
        for member in archive.getmembers():
            name = member.name.removeprefix("./")
            if name == "update.json" and member.isfile():
                extracted = archive.extractfile(member)
                if extracted is not None:
                    wanted = extracted.read()
                    break

    if wanted is None:
        raise SystemExit("The Pages artifact must contain update.json")

    for attempt in range(12):
        request = urllib.request.Request(
            FEED_URL,
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
        if attempt < 11:
            time.sleep(5)

    raise SystemExit("Pages reported success but is not serving the signed update.json")


if __name__ == "__main__":
    main()
