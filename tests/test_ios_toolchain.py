"""Check the actual CI preflight rejects SDKs that cannot compile Liquid Glass."""
import json
from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

from tests.test_ci_concurrency import yaml
from tools import test_ios_game_files

ROOT = Path(__file__).resolve().parents[1]


class IOSToolchainTests(unittest.TestCase):
    def test_download_fixture_environment_reaches_both_xctestrun_layouts(self):
        names = ("LauncherLogicTests", "LauncherPreviewUITests")
        url = "http://127.0.0.1:1234/game.zip"
        for version in (1, 2):
            with self.subTest(version=version):
                targets = {name: {"BlueprintName": name, "EnvironmentVariables": {"EXISTING": "1"}}
                           for name in names}
                spec = {"__xctestrun_metadata__": {"FormatVersion": version}}
                if version == 1:
                    spec.update(targets)
                else:
                    spec["TestConfigurations"] = [{"TestTargets": list(targets.values())}]
                with mock.patch.object(test_ios_game_files.lab, "game_env",
                                       side_effect=lambda base, **extra: dict(base, **extra)):
                    test_ios_game_files.configure_fixture(spec, url)
                for target in targets.values():
                    self.assertEqual(target["EnvironmentVariables"], {
                        "EXISTING": "1", "HORIZON_DOWNLOAD_FIXTURE_URL": url,
                    })

        with self.assertRaisesRegex(RuntimeError, "missing test targets"):
            test_ios_game_files.configure_fixture({}, url)

    def check_toolchain(self, version, sdk, *, missing_tool=None):
        with tempfile.TemporaryDirectory(prefix="horizon-xcode-preflight-") as temp:
            directory = Path(temp)
            # Only external tool output is substituted; run the production preflight.
            xcodebuild = directory / "xcodebuild"
            xcodebuild.write_text(
                "#!/bin/sh\nexit 1\n" if version is None else
                f"#!/bin/sh\nprintf 'Xcode %s\\nBuild version fixture\\n' '{version}'\n"
            )
            xcodebuild.chmod(0o755)
            xcrun = directory / "xcrun"
            xcrun.write_text(
                '#!/bin/sh\n'
                'case "$3" in\n'
                f'  --show-sdk-version) {"exit 1" if sdk is None else f"echo {sdk}"} ;;\n'
                f'  --find) [ "$4" != "{missing_tool or ""}" ] ;;\n'
                '  *) exit 2 ;;\n'
                'esac\n'
            )
            xcrun.chmod(0o755)
            env = {**os.environ, "PATH": str(directory) + os.pathsep + os.environ["PATH"]}
            return subprocess.run(["bash", str(ROOT / "tools/ci/ios-toolchain.sh")],
                                  env=env, capture_output=True, text=True, timeout=10)

    def test_xcode_bundles_the_executable_from_cargos_effective_target_directory(self):
        project = yaml.safe_load((ROOT / "packaging/ios/project.yml").read_text())["targets"]["HorizonGame"]
        phases = [project["preBuildScripts"][0]["script"], project["postCompileScripts"][0]["script"]]
        with tempfile.TemporaryDirectory(prefix="horizon cargo build ") as directory:
            root = Path(directory)
            srcroot = root / "packaging/ios"
            srcroot.mkdir(parents=True)
            helper = root / "tools/ci/ios-game-artifact.sh"
            helper.parent.mkdir(parents=True)
            shutil.copyfile(ROOT / "tools/ci/ios-game-artifact.sh", helper)
            target = "aarch64-apple-ios"
            artifacts = Path("aarch64-apple-ios/release/horizon-official")
            configured_root = root / "external target"
            for path, contents in ((configured_root / artifacts, b"current"),
                                   (root / "target" / artifacts, b"stale")):
                path.parent.mkdir(parents=True)
                path.write_bytes(contents)
                path.chmod(0o755)
            sysroot = root / "rust sysroot"
            (sysroot / "lib/rustlib" / target / "lib").mkdir(parents=True)
            tools = root / "bin"
            tools.mkdir()
            fake_tools = {
                "cargo": '#!/bin/sh\n[ "$1" = metadata ] || exit 88\nprintf "%s\\n" "$FIXTURE_METADATA"\n',
                "rustc": '#!/bin/sh\nprintf "%s\\n" "$FIXTURE_SYSROOT"\n',
                "stat": '#!/bin/sh\nprintf "7\\n"\n',
            }
            for name, script in fake_tools.items():
                path = tools / name
                path.write_text(script)
                path.chmod(0o755)
            products = root / "iOS products"
            products.mkdir()
            env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                       HOME=str(root), SRCROOT=str(srcroot), IOS_TARGET=target, HORIZON_GAME_PREBUILT="1",
                       BUILT_PRODUCTS_DIR=str(products), EXECUTABLE_PATH="Horizon",
                       FIXTURE_METADATA=json.dumps({"target_directory": str(configured_root)}),
                       FIXTURE_SYSROOT=str(sysroot))
            for phase in phases:
                result = subprocess.run(["bash", "-ec", phase], cwd=root, env=env,
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((products / "Horizon").read_bytes(), b"current")
            self.assertTrue(os.access(products / "Horizon", os.X_OK))

            # A failed copy can already have truncated its destination.
            # The existing bundle executable must survive that failure.
            cp = tools / "cp"
            cp.write_text('#!/bin/sh\nprintf partial > "$2"\nexit 1\n')
            cp.chmod(0o755)
            result = subprocess.run(["bash", "-ec", phases[1]], cwd=root, env=env,
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((products / "Horizon").read_bytes(), b"current")
            self.assertEqual(list(products.glob(".horizon-game.*")), [])


    def test_old_xcode_fails_before_the_build(self):
        result = self.check_toolchain("16.4", "18.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires Xcode 26", result.stderr)

    def test_sdk_override_cannot_silently_select_old_headers(self):
        result = self.check_toolchain("26.6", "18.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SDK must be 26", result.stderr)

    def test_incomplete_xcode_has_an_actionable_preflight_failure(self):
        for version, sdk, missing_tool, message in [
            (None, "26.6", None, "Could not run xcodebuild"),
            ("26.6", None, None, "Could not locate the iPhoneOS SDK"),
            ("26.6", "26.6", "swiftc", "does not provide swiftc"),
            ("26.6", "26.6", "clang", "does not provide clang"),
        ]:
            with self.subTest(message=message):
                result = self.check_toolchain(version, sdk, missing_tool=missing_tool)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)

    def test_current_and_newer_toolchains_pass(self):
        for version in ("26.6", "27.0"):
            with self.subTest(version=version):
                result = self.check_toolchain(version, version)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
