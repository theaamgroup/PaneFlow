#!/usr/bin/env python3
"""Probe the public updater delivery path without GitHub credentials or installation."""
import argparse
import pathlib
import plistlib
import subprocess
import tempfile
import time
import urllib.parse
import xml.etree.ElementTree as ET


def download(url, destination, limit):
    if urllib.parse.urlsplit(url).scheme != "https":
        raise ValueError("Update downloads must use HTTPS")
    subprocess.run([
        "curl", "--fail", "--location", "--silent", "--show-error",
        "--proto", "=https", "--proto-redir", "=https",
        "--connect-timeout", "15", "--max-time", "180", "--retry", "3",
        "--max-filesize", str(limit), "--output", str(destination), url,
    ], check=True)


def main():
    root = pathlib.Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--info-plist", type=pathlib.Path, default=root / "assets/Info.plist")
    parser.add_argument("--expected-version", required=True)
    args = parser.parse_args()
    with args.info_plist.open("rb") as handle:
        info = plistlib.load(handle)
    ns = {"sparkle": "http://www.andymatuschak.org/xml-namespaces/sparkle"}
    with tempfile.TemporaryDirectory(prefix="paneflow-update-check-") as tmp:
        folder = pathlib.Path(tmp)
        feed = folder / "appcast.xml"
        for attempt in range(6):
            try:
                download(info["SUFeedURL"], feed, 2 * 1024 * 1024)
                items = ET.parse(feed).findall("./channel/item")
                if len(items) == 1 and items[0].findtext("sparkle:version", namespaces=ns) == args.expected_version:
                    break
            except (subprocess.CalledProcessError, ET.ParseError):
                if attempt == 5:
                    raise
            if attempt == 5:
                raise ValueError("Public feed does not advertise the expected release")
            # GitHub's latest-release redirect may take a moment to update.
            time.sleep(10)
        enclosure = items[0].find("enclosure")
        if enclosure is None:
            raise ValueError("Appcast has no enclosure")
        url = enclosure.attrib["url"]
        filename = f"paneflow-{args.expected_version}-aarch64-apple-darwin.dmg"
        if pathlib.PurePosixPath(urllib.parse.urlsplit(url).path).name != filename:
            raise ValueError("Unexpected update archive filename")
        archive = folder / filename
        download(url, archive, 512 * 1024 * 1024)
        subprocess.run(["swift", str(root / "scripts/verify-update.swift"),
                        str(args.info_plist), str(feed), str(archive)], check=True)
        print("Anonymous update delivery verified (feed, archive length, and public-key signature).")


if __name__ == "__main__":
    main()
