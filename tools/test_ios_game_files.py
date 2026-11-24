#!/usr/bin/env python3
"""Test iOS first-run setup with a loopback-only synthetic download.

Requires Xcode, XcodeGen, and a booted iOS simulator. No Minecraft files or
public download service are used. Example:
  python3 tools/test_ios_game_files.py --device SIMULATOR_UUID --derived-data /tmp/horizon-download-tests
"""
import argparse
import http.server
import json
from pathlib import Path
import plistlib
import subprocess
import threading
import time

try:
    from . import lab
except ImportError:
    import lab


class Fixture(http.server.BaseHTTPRequestHandler):
    requests = []

    def do_GET(self):
        self.requests.append({
            "path": self.path,
            "authorization_present": self.headers.get("Authorization") is not None,
        })
        failed = "fail=1" in self.path
        partial = "partial=1" in self.path
        empty = "empty=1" in self.path
        cut_once = "cut_once=" in self.path and sum(
            request["path"] == self.path for request in self.requests
        ) == 1
        truncated = "truncated=1" in self.path or cut_once
        body = b"temporary fixture failure" if failed else b"horizon-download-fixture"
        if empty or "empty_ok=1" in self.path:
            body = b""
        if "delay=1" in self.path:
            time.sleep(0.2)
        status = 503 if failed else 206 if partial else 204 if empty else 200
        self.send_response(status)
        if partial:
            self.send_header("Content-Range", f"bytes 0-{len(body) - 1}/{len(body) * 2}")
        if not empty and "without_length=1" not in self.path:
            length = (2 << 30) + 1 if "oversized=1" in self.path else len(body)
            if truncated:
                length *= 2
            self.send_header("Content-Length", str(length))
        self.send_header("Content-Type", "application/zip")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def configure_fixture(spec, url):
    version = spec.get("__xctestrun_metadata__", {}).get("FormatVersion", 1)
    if version == 1:
        targets = spec.items()
    elif version == 2:
        targets = [
            (target["BlueprintName"], target)
            for configuration in spec["TestConfigurations"]
            if configuration.get("IsEnabled", True)
            for target in configuration["TestTargets"]
        ]
    else:
        raise RuntimeError(f"Unsupported xctestrun format version: {version}")

    required = {"LauncherLogicTests", "LauncherPreviewUITests"}
    configured = set()
    for name, target in targets:
        if name in required:
            target["EnvironmentVariables"] = lab.game_env(
                target.get("EnvironmentVariables", {}), HORIZON_DOWNLOAD_FIXTURE_URL=url,
            )
            configured.add(name)
    if missing := required - configured:
        raise RuntimeError("The xctestrun is missing test targets: " + ", ".join(sorted(missing)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", required=True, help="Booted iOS simulator UUID")
    parser.add_argument("--derived-data", required=True, type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    derived = args.derived_data.resolve()
    destination = "platform=iOS Simulator,id=" + args.device
    subprocess.run([
        "xcodegen", "generate", "--spec", str(repo / "packaging/ios/Preview/project.yml"),
    ], check=True, cwd=repo)
    subprocess.run([
        "xcodebuild", "-project", str(repo / "packaging/ios/Preview/LauncherPreview.xcodeproj"),
        "-scheme", "LauncherPreview", "-destination", destination,
        "-derivedDataPath", str(derived), "build-for-testing",
    ], check=True, cwd=repo)
    products = derived / "Build/Products"
    original = max(products.glob("LauncherPreview_*.xctestrun"), key=lambda p: p.stat().st_mtime)
    spec = plistlib.loads(original.read_bytes())
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        url = f"http://127.0.0.1:{server.server_port}/game.zip"
        configure_fixture(spec, url)
        fixture = products / "AutoFilesFixture.xctestrun"
        fixture.write_bytes(plistlib.dumps(spec))
        log_path = derived / "auto-files-test.log"
        with log_path.open("w") as log:
            result = subprocess.run([
                "xcodebuild", "-xctestrun", str(fixture), "-destination", destination,
                "-parallel-testing-enabled", "NO",
                # This loopback fixture records its own requests and test log.
                # Automatic simulator diagnostics can stall after tests finish.
                "-collect-test-diagnostics", "never",
                "-only-testing:LauncherLogicTests/AutomaticGameFilesTests",
                "-only-testing:LauncherPreviewUITests/LauncherLayoutTests/testFirstRunAutomaticallyDownloadsAndEnablesPlay",
                "-only-testing:LauncherPreviewUITests/LauncherLayoutTests/testImportCommitKeepsPlayDisabledUntilAcknowledged",
                "-only-testing:LauncherPreviewUITests/LauncherLayoutTests/testFirstRunDownloadFailureKeepsRetryAndManualFallbackAvailable",
                "-only-testing:LauncherPreviewUITests/LauncherLayoutTests/testOversizedAutomaticArchiveStopsBeforeImport",
                "test-without-building",
            ], cwd=repo, stdout=log, stderr=subprocess.STDOUT)
        (derived / "auto-files-requests.json").write_text(json.dumps(Fixture.requests, indent=2))
        if any(request["authorization_present"] for request in Fixture.requests):
            raise RuntimeError("Custom credentials leaked to the automatic download source")
        if result.returncode == 0 and not any("oversized=1" in request["path"] for request in Fixture.requests):
            raise RuntimeError("The oversized-download fixture was never requested")
        print(f"Loopback requests: {len(Fixture.requests)}; Authorization headers: none")
        print(f"Test log: {log_path}")
        return result.returncode
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


if __name__ == "__main__":
    raise SystemExit(main())
