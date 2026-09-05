#!/usr/bin/env python3
"""Exercise Sparkle install-on-quit using disposable apps and a throwaway signing key.

Build `cargo build -p paneflow-app --example update_probe` first.
No installed app or production signing credentials are modified.
"""
import functools
import http.server
import os
import pathlib
import plistlib
import shutil
import subprocess
import tempfile
import threading
import time
import uuid
import xml.etree.ElementTree as ET


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def main():
    repo = pathlib.Path(__file__).resolve().parent.parent
    probe = repo / "target/debug/examples/update_probe"
    if not probe.is_file():
        raise RuntimeError("Build the update_probe example first")
    with tempfile.TemporaryDirectory(prefix="paneflow-install-test-") as temporary:
        root = pathlib.Path(temporary)
        served = root / "served"
        served.mkdir()
        handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(served))
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            base_url = f"http://127.0.0.1:{server.server_port}"
            signer = root / "sign-fixture.swift"
            signer.write_text('''import Foundation
import CryptoKit
let path = URL(fileURLWithPath: CommandLine.arguments[1])
let key: Curve25519.Signing.PrivateKey
if FileManager.default.fileExists(atPath: path.path) {
    key = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(contentsOf: path))
} else {
    key = Curve25519.Signing.PrivateKey()
    try key.rawRepresentation.write(to: path, options: .atomic)
}
if CommandLine.arguments.count == 2 {
    print(key.publicKey.rawRepresentation.base64EncodedString())
} else {
    print(try key.signature(for: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2]))).base64EncodedString())
}
''')
            key = root / "throwaway-test-key"
            public_key = run("swift", str(signer), str(key), capture_output=True, text=True).stdout.strip()
            old = root / "PaneFlow Probe.app"
            contents = old / "Contents"
            (contents / "MacOS").mkdir(parents=True)
            (contents / "Frameworks").mkdir()
            distribution = run(str(repo / "scripts/sparkle-dist.sh"), capture_output=True, text=True).stdout.strip()
            run("ditto", str(pathlib.Path(distribution) / "Sparkle.framework"), str(contents / "Frameworks/Sparkle.framework"))
            shutil.copy2(probe, contents / "MacOS/update_probe")
            info = plistlib.loads((repo / "assets/Info.plist").read_bytes())
            info.update({
                "CFBundleIdentifier": "com.theaamgroup.paneflow.update-probe." + uuid.uuid4().hex,
                "CFBundleName": "PaneFlow Probe", "CFBundleExecutable": "update_probe",
                "CFBundleVersion": "1.0.0", "CFBundleShortVersionString": "1.0.0",
                "SUPublicEDKey": public_key, "SUFeedURL": base_url + "/appcast.xml",
                "NSAppTransportSecurity": {"NSAllowsArbitraryLoads": True}, "LSUIElement": True,
            })
            info.pop("CFBundleIconFile", None)
            (contents / "Info.plist").write_bytes(plistlib.dumps(info))
            run("codesign", "--force", "--sign", "-", str(old))
            new = root / "new/PaneFlow Probe.app"
            run("ditto", str(old), str(new))
            info["CFBundleVersion"] = info["CFBundleShortVersionString"] = "1.0.1"
            (new / "Contents/Info.plist").write_bytes(plistlib.dumps(info))
            run("codesign", "--force", "--sign", "-", str(new))
            archive = served / "PaneFlow-Probe-1.0.1.dmg"
            run("hdiutil", "create", "-quiet", "-volname", "PaneFlow Probe", "-srcfolder", str(new.parent), "-format", "UDZO", str(archive))
            signature = run("swift", str(signer), str(key), str(archive), capture_output=True, text=True).stdout.strip()
            ns = "http://www.andymatuschak.org/xml-namespaces/sparkle"
            ET.register_namespace("sparkle", ns)
            rss = ET.Element("rss", version="2.0")
            item = ET.SubElement(ET.SubElement(rss, "channel"), "item")
            ET.SubElement(item, "title").text = "PaneFlow test update"
            ET.SubElement(item, f"{{{ns}}}version").text = "1.0.1"
            ET.SubElement(item, "enclosure", {
                "url": base_url + "/" + archive.name, "length": str(archive.stat().st_size),
                "type": "application/octet-stream", f"{{{ns}}}edSignature": signature,
            })
            ET.ElementTree(rss).write(served / "appcast.xml", encoding="utf-8", xml_declaration=True)
            print("Starting isolated update: 1.0.0 → 1.0.1", flush=True)
            run(str(contents / "MacOS/update_probe"), "--install-fixture",
                env={**os.environ, "RUST_LOG": "info"}, timeout=55)
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                try:
                    installed = plistlib.loads((contents / "Info.plist").read_bytes())
                    if installed["CFBundleVersion"] == "1.0.1":
                        run("codesign", "--verify", "--deep", "--strict", str(old))
                        print("PASS: Sparkle downloaded, verified, and installed 1.0.1 on normal quit.", flush=True)
                        return
                except (FileNotFoundError, plistlib.InvalidFileException):
                    pass
                time.sleep(0.25)
            raise RuntimeError("App quit but the replacement version never appeared")
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
