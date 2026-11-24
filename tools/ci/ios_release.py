#!/usr/bin/env python3
"""Audit an unsigned iOS build and prepare its versioned release assets."""
import argparse
import hashlib
import os
import plistlib
import re
import shutil
import stat
import struct
import tempfile
import unicodedata
import zipfile
from pathlib import Path, PurePosixPath
from xml.parsers.expat import ExpatError

VERSION = re.compile(r"[0-9]+(?:\.[0-9]+){0,2}\Z")
PRIVATE_NAMES = {"horizon-resources.json", "horizon-data.bin", "horizon-auth.json",
                 "horizon-player.txt", "servers.dat", "options.txt"}
PRIVATE_SUFFIXES = {".p12", ".pfx", ".mobileprovision", ".pem", ".key", ".cer",
                    ".crt", ".jar", ".nbt", ".mca", ".mcr"}
MACHO64 = b"\xcf\xfa\xed\xfe"
OTHER_MACHO = {b"\xfe\xed\xfa\xcf", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xce",
               b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}


# Run 88 was the first release in this sequence. Retries change only the build.
BASE_RUN = 88
BASE_PATCH = 44


def release_version(run, attempt):
    if not re.fullmatch(r"[1-9][0-9]*", str(run)) or not re.fullmatch(r"[1-9][0-9]*", str(attempt)):
        raise ValueError("Invalid workflow run or attempt")
    # Fresh forks have independent counters; keep their preview builds at the base.
    patch = BASE_PATCH + max(0, int(run) - BASE_RUN)
    return f"0.1.{patch}", f"{run}.{attempt}"


def read_bundle_info(data, path):
    try:
        info = plistlib.loads(data)
    except (ExpatError, ValueError) as error:
        raise ValueError(f"Unreadable bundle metadata: {path}") from error
    if not isinstance(info, dict):
        raise ValueError(f"Bundle metadata must be a dictionary: {path}")
    identifier = info.get("CFBundleIdentifier")
    if not isinstance(identifier, str) or not re.fullmatch(r"[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*", identifier):
        raise ValueError(f"Missing or invalid CFBundleIdentifier: {path}")
    return info


def stamp_app(app, run, attempt, commit, client_id):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Invalid source commit")
    version, build = release_version(run, attempt)
    paths = [app / "Info.plist", *sorted((app / "Frameworks").glob("*.framework/Info.plist"))]
    # Read and validate all metadata before changing any file.
    records = []
    for path in paths:
        info = read_bundle_info(path.read_bytes(), path)
        info["CFBundleShortVersionString"] = version
        info["CFBundleVersion"] = build
        if path == app / "Info.plist":
            info["HorizonBuildCommit"] = commit
            info["HorizonMSAClientID"] = client_id
        records.append((path, plistlib.dumps(info)))
    for path, data in records:
        path.write_bytes(data)
    return version, build


def unsigned_macho(stream):
    magic = stream.read(4)
    if magic in OTHER_MACHO:
        raise ValueError("Expected a thin ARM64 iOS binary")
    if magic != MACHO64:
        return None
    header = magic + stream.read(28)
    if len(header) != 32:
        raise ValueError("Truncated Mach-O header")
    cpu, _, file_type, count, size = struct.unpack_from("<IIIII", header, 4)
    if cpu != 0x0100000C or size > 16 << 20:
        raise ValueError("Invalid ARM64 Mach-O load commands")
    commands = stream.read(size)
    if len(commands) != size:
        raise ValueError("Truncated Mach-O load commands")
    offset = 0
    minimum = None
    for _ in range(count):
        if offset + 8 > size:
            raise ValueError("Invalid Mach-O command count")
        command, length = struct.unpack_from("<II", commands, offset)
        if length < 8 or offset + length > size:
            raise ValueError("Invalid Mach-O command size")
        if command == 0x1D:  # LC_CODE_SIGNATURE, including ad-hoc signatures.
            raise ValueError("The IPA contains a signed binary")
        if command == 0x32:  # LC_BUILD_VERSION: arm64 is shared by devices, simulators and Macs.
            if length < 24:
                raise ValueError("Invalid Mach-O build-version command")
            platform, required, _, tools = struct.unpack_from("<IIII", commands, offset + 8)
            if 24 + tools * 8 > length:
                raise ValueError("Truncated Mach-O build-version tools")
            if platform != 2:  # PLATFORM_IOS, not PLATFORM_IOSSIMULATOR (7).
                raise ValueError("Expected an iOS device Mach-O platform")
            minimum = max(minimum or 0, required)
        elif command == 0x25:  # LC_VERSION_MIN_IPHONEOS, emitted by older device toolchains.
            if length < 16:
                raise ValueError("Invalid iOS device minimum-version command")
            required, = struct.unpack_from("<I", commands, offset + 8)
            minimum = max(minimum or 0, required)
        elif command in (0x24, 0x2F, 0x30):  # macOS, tvOS and watchOS legacy minimum versions.
            raise ValueError("Expected an iOS device Mach-O platform")
        offset += length
    if offset != size:
        raise ValueError("Unexpected Mach-O load-command data")
    if minimum is None:
        raise ValueError("Missing iOS device Mach-O build target")
    return file_type, (minimum >> 16, (minimum >> 8) & 0xff, minimum & 0xff)


def parse_minimum_os(value):
    if not isinstance(value, str) or not VERSION.fullmatch(value):
        raise ValueError("Invalid iOS minimum version")
    components = tuple(map(int, value.split(".")))
    return components + (0,) * (3 - len(components))


def minimum_os(info):
    return parse_minimum_os(info["MinimumOSVersion"]) if "MinimumOSVersion" in info else None


def bundle_executable(bundle, info, binaries, file_type, label):
    executable = info.get("CFBundleExecutable")
    if (not isinstance(executable, str) or not executable or "/" in executable
            or "\\" in executable or executable in (".", "..")):
        raise ValueError(f"Invalid {label} executable name")
    binary = binaries.get(bundle / executable)
    if binary is None or binary[0] != file_type:
        raise ValueError(f"Missing unsigned ARM64 {label} executable")
    return binary[1]


def verify_native_game(archive, executable):
    # The Objective-C placeholder is also a valid unsigned ARM64 Mach-O.
    # The Rust engine uses both strings for its native launcher dlsym call.
    markers = {
        b"horizon_launcher_present\0",
        b"Frameworks/HorizonLauncherKit.framework/HorizonLauncherKit",
    }
    overlap = max(map(len, markers)) - 1
    tail = b""
    with archive.open(executable.as_posix()) as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            chunk = tail + block
            markers = {marker for marker in markers if marker not in chunk}
            if not markers:
                return
            tail = chunk[-overlap:]
    raise ValueError("App executable is missing the Rust game's native launcher bridge")


def audit_payload_layout(paths, app):
    # Installers may unpack on case-insensitive or normalization-insensitive volumes.
    # Check implicit directories too: Assets/a and assets/b need not be ZIP entries
    # for both spellings of Assets to collide.
    normalized = {}
    payload = PurePosixPath("Payload")
    for path in paths:
        if path != payload and path != app and app not in path.parents:
            raise ValueError("Unexpected entry outside the IPA application bundle")
        for component in (path, *path.parents):
            if not component.parts:
                continue
            key = unicodedata.normalize("NFD", component.as_posix().casefold())
            if normalized.setdefault(key, component) != component:
                raise ValueError("IPA paths collide under case or Unicode normalization")
            if component != path and paths.get(component) is False:
                raise ValueError("An IPA file is also used as a directory")


def audit(ipa, expected_commit=None, expected_version=None, *,
          require_launcher=False, expected_client_id=None, expected_minimum_os=None):
    with zipfile.ZipFile(ipa) as archive:
        paths = {}
        binaries = {}
        infos = []
        framework_paths = set()
        frameworks = {}
        for entry in archive.infolist():
            path = PurePosixPath(entry.filename)
            parts = {p.lower() for p in path.parts}
            name = path.name.lower()
            if (path.is_absolute() or ".." in parts or "\\" in entry.filename
                    or not path.parts or path.parts[0] != "Payload"
                    or entry.orig_filename != path.as_posix() + ("/" if entry.is_dir() else "")):
                raise ValueError("Unexpected IPA path")
            if path in paths:
                raise ValueError("Duplicate IPA entries")
            paths[path] = entry.is_dir()
            if (name in PRIVATE_NAMES or path.suffix.lower() in PRIVATE_SUFFIXES
                    or name.startswith(".env")
                    or parts & {"_codesignature", "xcuserdata", "saves", "logs"}):
                raise ValueError("The IPA contains signing material, private files or game assets")
            file_type = stat.S_IFMT(entry.external_attr >> 16)
            if file_type == stat.S_IFLNK:
                raise ValueError("The IPA contains a symbolic link")
            expected_type = stat.S_IFDIR if entry.is_dir() else stat.S_IFREG
            if file_type not in (0, expected_type):
                raise ValueError(f"Unexpected IPA entry file type: {path}")
            for component in (path, *path.parents):
                if component.suffix == ".framework":
                    framework_paths.add(component)
            if entry.is_dir():
                continue
            with archive.open(entry) as stream:
                binary = unsigned_macho(stream)
                if binary is not None:
                    binaries[path] = binary
            if len(path.parts) == 3 and path.parts[1].endswith(".app") and path.name == "Info.plist":
                if entry.file_size > 1 << 20:
                    raise ValueError("Oversized app Info.plist")
                infos.append((path.parent, read_bundle_info(archive.read(entry), path)))
            if path.name == "Info.plist" and path.parent.suffix == ".framework":
                if entry.file_size > 1 << 20:
                    raise ValueError("Oversized framework Info.plist")
                frameworks[path.parent] = read_bundle_info(archive.read(entry), path)
        if archive.testzip() is not None:
            raise ValueError("The IPA contains a corrupt ZIP entry")
        if len(infos) != 1:
            raise ValueError("Expected exactly one application")
        app, info = infos[0]
        audit_payload_layout(paths, app)
        # Manual device bundles may omit Xcode's platform metadata. The load
        # commands above prove the real target of every executable independently;
        # an explicit contradictory plist value still refuses the package.
        platforms = info.get("CFBundleSupportedPlatforms")
        if platforms is not None and platforms != ["iPhoneOS"]:
            raise ValueError("Expected a device IPA, not a simulator app")
        app_minimum = minimum_os(info)
        if expected_minimum_os is not None and app_minimum != expected_minimum_os:
            raise ValueError("IPA MinimumOSVersion does not match the configured iOS deployment target")
        required = bundle_executable(app, info, binaries, 2, "app")
        executable_mode = archive.getinfo((app / info["CFBundleExecutable"]).as_posix()).external_attr >> 16
        if not stat.S_ISREG(executable_mode) or not executable_mode & stat.S_IXUSR:
            raise ValueError("The IPA app executable must be a regular file with owner execute permission")
        if app_minimum is not None and required > app_minimum:
            raise ValueError("App executable requires a newer iOS version than MinimumOSVersion")
        version, build = info.get("CFBundleShortVersionString"), info.get("CFBundleVersion")
        if not all(isinstance(v, str) and VERSION.fullmatch(v) for v in (version, build)):
            raise ValueError("Invalid app version or build number")
        if framework_paths != frameworks.keys():
            raise ValueError("Missing framework Info.plist")
        if require_launcher:
            launcher_name = "HorizonLauncherKit"
            launcher = frameworks.get(app / "Frameworks" / f"{launcher_name}.framework")
            # The engine opens this exact relative path at runtime.
            if launcher is None or launcher.get("CFBundleExecutable") != launcher_name:
                raise ValueError("Missing HorizonLauncherKit launcher framework at the expected path")
            verify_native_game(archive, app / info["CFBundleExecutable"])
        if expected_client_id is not None:
            if not expected_client_id or info.get("HorizonMSAClientID") != expected_client_id:
                raise ValueError("IPA Microsoft client ID does not match the build configuration")
        for bundle, framework in frameworks.items():
            if (framework.get("CFBundleShortVersionString"), framework.get("CFBundleVersion")) != (version, build):
                raise ValueError("Framework version does not match the app")
            if framework.get("CFBundleSupportedPlatforms", ["iPhoneOS"]) != ["iPhoneOS"]:
                raise ValueError("Expected an iOS device framework")
            framework_minimum = minimum_os(framework)
            required = bundle_executable(bundle, framework, binaries, 6, "framework")
            if framework_minimum is not None and required > framework_minimum:
                raise ValueError("Framework executable requires a newer iOS version than MinimumOSVersion")
            if app_minimum is not None and max(required, framework_minimum or required) > app_minimum:
                raise ValueError("Framework requires a newer iOS version than the app")
        if app_minimum is not None:
            for path, (_, required) in binaries.items():
                if required > app_minimum:
                    raise ValueError(f"{path} requires a newer iOS version than the app")
        if expected_commit is not None:
            if not re.fullmatch(r"[0-9a-f]{40}", expected_commit):
                raise ValueError("Invalid source commit")
            if info.get("HorizonBuildCommit") != expected_commit:
                raise ValueError("IPA source commit does not match this workflow")
        if expected_version is not None and (version, build) != expected_version:
            raise ValueError("IPA version does not match this workflow run")
        return version, build


def prepare(ipa, output, expected_commit, expected_version=None, *,
            require_launcher=False, expected_client_id=None, expected_minimum_os=None):
    import fcntl

    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".ios-release-", dir=output) as staging:
        staged_ipa = Path(staging) / "artifact.ipa"
        staged_checksum = Path(staging) / "artifact.sha256"
        shutil.copyfile(ipa, staged_ipa)
        # Verify the bytes that will actually be published, even if the input changed during copying.
        version, build = audit(
            staged_ipa, expected_commit, expected_version, require_launcher=require_launcher,
            expected_client_id=expected_client_id, expected_minimum_os=expected_minimum_os,
        )
        with staged_ipa.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        target = output / f"Horizon-iOS-{version}-build{build}-unsigned.ipa"
        checksum = target.with_suffix(target.suffix + ".sha256")
        staged_checksum.write_text(f"{digest}  {target.name}\n", encoding="utf-8")
        # A concurrent preparation may target the same version/build pair.
        # Hold the directory lock only while publishing, and let close release
        # it even on error. No sidecar lock file needs cleanup after interruption.
        directory_fd = os.open(output, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(directory_fd, fcntl.LOCK_EX)
            # Both files are ready. If publication stops between the renames, the
            # IPA may be present without a checksum, but never with the old one.
            previous_checksum = Path(staging) / "previous.sha256"
            if checksum.is_dir() and not checksum.is_symlink():
                raise IsADirectoryError(f"Checksum destination is a directory: {checksum}")
            try:
                checksum.replace(previous_checksum)
            except FileNotFoundError:
                pass
            try:
                staged_ipa.replace(target)
            except OSError as error:
                # The old IPA is still in place, so its checksum is still valid.
                if previous_checksum.exists() or previous_checksum.is_symlink():
                    try:
                        previous_checksum.replace(checksum)
                    except OSError as restore_error:
                        error.add_note(f"Could not restore the previous checksum: {restore_error}")
                raise
            staged_checksum.replace(checksum)
        finally:
            os.close(directory_fd)
    return {"ipa": str(target), "checksum": str(checksum), "version": version, "build": build}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ipa", type=Path, nargs="?")
    parser.add_argument("--stamp-app", type=Path)
    parser.add_argument("--run-number")
    parser.add_argument("--run-attempt")
    parser.add_argument("--client-id", help="Microsoft client ID to stamp or verify")
    parser.add_argument("--require-launcher", action="store_true")
    parser.add_argument("--minimum-ios", type=parse_minimum_os, help="Expected app deployment target")
    parser.add_argument("--expect-commit", required=True)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    if args.stamp_app:
        if (args.ipa or args.output_dir or args.require_launcher or args.minimum_ios is not None
                or not args.run_number or not args.run_attempt):
            parser.error("--stamp-app requires run number and attempt, without an IPA/output directory")
        stamp_app(args.stamp_app, args.run_number, args.run_attempt, args.expect_commit, args.client_id or "")
        return
    if not args.ipa:
        parser.error("an IPA or --stamp-app is required")
    expected = (release_version(args.run_number, args.run_attempt)
                if args.run_number or args.run_attempt else None)
    checks = {"require_launcher": args.require_launcher, "expected_client_id": args.client_id,
              "expected_minimum_os": args.minimum_ios}
    result = (prepare(args.ipa, args.output_dir, args.expect_commit, expected, **checks) if args.output_dir
              else dict(zip(("version", "build"), audit(args.ipa, args.expect_commit, expected, **checks))))
    for key, value in result.items():
        print(f"{key}={value}")


if __name__ == "__main__":
    main()
