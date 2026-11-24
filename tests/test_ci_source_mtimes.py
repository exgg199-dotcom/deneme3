import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

from tools.ci.source_mtimes import restore_and_snapshot
from tests.test_ci_concurrency import ROOT, yaml
import re


class SourceMtimesTests(unittest.TestCase):
    def setUp(self):
        # The Cargo fixture's build output stays under this checkout's target.
        target = Path(__file__).resolve().parents[1] / "target"
        target.mkdir(exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(prefix="source-mtimes-", dir=target)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.snapshot = self.root / "target/ci-source-mtimes.json"
        self.git("init", "--quiet")
        (self.root / ".gitignore").write_text("target/\n")
        (self.root / "src").mkdir()
        (self.root / "src/value.txt").write_text("original\n")
        self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args])

    def commit(self):
        self.git("add", "-A")
        self.git("-c", "user.name=CI test", "-c", "user.email=ci@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture")

    def test_unchanged_files_and_trees_restore_but_changed_inputs_do_not(self):
        restore_and_snapshot(self.root, self.snapshot)
        original = json.loads(self.snapshot.read_text())
        source = self.root / "src/value.txt"
        later = source.stat().st_mtime_ns + 10_000_000_000
        for name in original:
            os.utime(self.root / name, ns=(later, later))
        restore_and_snapshot(self.root, self.snapshot)
        for name, (_, mtime) in original.items():
            self.assertEqual((self.root / name).stat().st_mtime_ns, mtime)

        # A new file changes the directory tree too, even if an old file is unchanged.
        added = self.root / "src/added.txt"
        added.write_text("new input\n")
        self.commit()
        # An input can even predate the restored build (checkout/cache-save race).
        os.utime(added.parent, ns=(1, 1))
        os.utime(added, ns=(1, 1))
        restore_and_snapshot(self.root, self.snapshot)
        self.assertGreater(added.parent.stat().st_mtime_ns, original["src"][1])
        self.assertGreater(added.stat().st_mtime_ns, original["src"][1])
        self.assertEqual(source.stat().st_mtime_ns, original["src/value.txt"][1])

        # A deletion must invalidate a directory watch as well.
        added.unlink()
        self.commit()
        before = source.parent.stat().st_mtime_ns
        os.utime(source.parent, ns=(1, 1))
        restore_and_snapshot(self.root, self.snapshot)
        self.assertGreater(source.parent.stat().st_mtime_ns, before)

    def test_snapshot_refreshes_both_sides_of_a_rename_and_deleted_parents(self):
        root = self.root
        left, right = root / "src/left", root / "src/right"
        removed = root / "src/removed"
        for directory in (left, right, removed):
            directory.mkdir()
        (left / "kept.txt").write_text("kept")
        (left / "moved.txt").write_text("moved")
        (removed / "deleted.txt").write_text("deleted")
        self.commit()
        restore_and_snapshot(root, self.snapshot)
        pinned = (left / "kept.txt").stat().st_mtime_ns
        (left / "moved.txt").rename(right / "moved.txt")
        shutil.rmtree(removed)
        self.commit()

        for path in (root / "src", left, right, right / "moved.txt", left / "kept.txt"):
            os.utime(path, ns=(1, 1))
        restore_and_snapshot(root, self.snapshot)
        self.assertEqual((left / "kept.txt").stat().st_mtime_ns, pinned)
        for path in (root / "src", left, right, right / "moved.txt"):
            self.assertGreater(path.stat().st_mtime_ns, 1)
        self.assertFalse(removed.exists())

    def test_unreadable_or_malformed_snapshot_refreshes_file_timestamps(self):
        self.snapshot.parent.mkdir()
        source = self.root / "src/value.txt"
        for prior in (b'{"interrupted":', b"[]", b'{"src/value.txt": []}'):
            with self.subTest(prior=prior):
                self.snapshot.write_bytes(prior)
                os.utime(source, ns=(1, 1))
                restore_and_snapshot(self.root, self.snapshot)
                fresh = json.loads(self.snapshot.read_text())
                self.assertGreater(fresh["src/value.txt"][1], 1)
                self.assertEqual(source.stat().st_mtime_ns, fresh["src/value.txt"][1])

    def test_interrupted_snapshot_write_preserves_the_previous_snapshot(self):
        restore_and_snapshot(self.root, self.snapshot)
        previous = self.snapshot.read_bytes()

        def fail(_, stream, **kwargs):
            stream.write('{"interrupted":')
            raise OSError("simulated snapshot write error")

        with mock.patch("tools.ci.source_mtimes.json.dump", side_effect=fail):
            with self.assertRaisesRegex(OSError, "snapshot write error"):
                restore_and_snapshot(self.root, self.snapshot)
        self.assertEqual(self.snapshot.read_bytes(), previous)
        self.assertEqual(list(self.snapshot.parent.iterdir()), [self.snapshot])

    @unittest.skipIf(os.name == "nt", "creating a symlink needs a privilege on Windows")
    def test_modified_checkout_is_refused_and_symlinks_are_not_followed(self):
        external = self.root / "untracked.txt"
        external.write_text("not a source\n")
        (self.root / "src/link").symlink_to(external)
        self.git("add", "src/link")
        self.git("-c", "user.name=CI test", "-c", "user.email=ci@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "link")
        restore_and_snapshot(self.root, self.snapshot)
        self.assertNotIn("src/link", json.loads(self.snapshot.read_text()))
        (self.root / "src/value.txt").write_text("uncommitted\n")
        with self.assertRaises(subprocess.CalledProcessError):
            restore_and_snapshot(self.root, self.snapshot)

    def test_clippy_reuses_both_feature_graphs_and_rechecks_changed_source(self):
        # verify runs Python tests before installing its Rust components. The
        # fixture is exercised wherever Clippy is already available.
        if not shutil.which("cargo"):
            self.skipTest("Cargo is not installed")
        available = subprocess.run(["cargo", "clippy", "--version"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if available.returncode:
            self.skipTest("the Clippy component is not installed")
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "mtime-fixture"\nversion = "0.0.0"\nedition = "2024"\n'
            '[workspace]\n[features]\ndefault = ["audio"]\naudio = []\n'
        )
        (self.root / "src/lib.rs").write_text('pub const VALUE: &str = include_str!("value.txt");\n')
        (self.root / "build.rs").write_text(
            'fn main() { println!("cargo:rerun-if-changed=src"); }\n'
        )
        self.commit()
        restore_and_snapshot(self.root, self.snapshot)

        def check(*flags):
            # The hosted lint job has no sccache wrapper. Isolate the fixture from
            # a box's wrapper too; it may explicitly forbid incremental builds.
            env = {**os.environ, "CARGO_TARGET_DIR": str(self.root / "target"),
                   "CARGO_INCREMENTAL": "1", "RUSTC_WRAPPER": ""}
            output = subprocess.check_output(
                ["cargo", "clippy", "--offline", "--all-targets", "--message-format=json", *flags,
                 "--", "-D", "warnings"], cwd=self.root, env=env,
            )
            return [item["fresh"] for line in output.splitlines()
                    if (item := json.loads(line))["reason"] == "compiler-artifact"]

        self.assertIn(False, check())
        self.assertIn(False, check("--no-default-features"))
        # As after checkout on another runner: only the source mtimes have changed.
        original = json.loads(self.snapshot.read_text())
        for name, (_, mtime) in original.items():
            os.utime(self.root / name, ns=(mtime + 10_000_000_000, mtime + 10_000_000_000))
        restore_and_snapshot(self.root, self.snapshot)
        self.assertTrue(all(check()))
        self.assertTrue(all(check("--no-default-features")))

        (self.root / "src/value.txt").write_text("changed\n")
        self.commit()
        # Even a backdated changed input must rebuild, not look fresh by accident.
        os.utime(self.root / "src/value.txt", ns=(1, 1))
        restore_and_snapshot(self.root, self.snapshot)
        self.assertIn(False, check())
        self.assertIn(False, check("--no-default-features"))



# Steps that compile into the job's Cargo target: Cargo itself and the tools that run it.
BUILDS = re.compile(r"\bcargo (\+\S+ )?(build|test|check|clippy|rustc|run|doc)\b|vanilla-data\.sh|prebuilt_game\.py|"
                    r"ignored_tests\.py|run_test_binaries\.py")


class BlacksmithBuildFolderTests(unittest.TestCase):
    def test_every_verify_build_on_blacksmith_restores_its_own_disk_first(self):
        # A build before the restore would compare against checkout dates and trust another tree's artifacts' dates;
        # two jobs on one disk would each replace the other's build.
        workflow = ROOT / ".github/workflows/ci.yml"
        if not workflow.is_file():
            self.skipTest("The mobile fork does not run upstream desktop CI")
        jobs = yaml.safe_load(workflow.read_text())["jobs"]
        keys, commits = [], []
        for name, job in jobs.items():
            steps = job.get("steps", [])
            builds = [i for i, step in enumerate(steps) if BUILDS.search(step.get("run", ""))]
            if not builds or "blacksmith-" not in str(job.get("runs-on")):
                continue
            with self.subTest(job=name):
                mount = next(i for i, step in enumerate(steps)
                             if step.get("uses", "").startswith("useblacksmith/stickydisk@"))
                restore = next(i for i, step in enumerate(steps) if "tools/ci/build_folder.sh" in step.get("run", ""))
                self.assertLess(mount, restore)
                self.assertLess(restore, builds[0])
                self.assertEqual(steps[restore]["run"], f"tools/ci/build_folder.sh {steps[mount]['with']['path']}")
                self.assertEqual(steps[mount]["if"], steps[restore]["if"])
                keys.append(steps[mount]["with"]["key"])
                commits.append(steps[mount]["with"].get("commit"))
        self.assertGreaterEqual(len(keys), 4)
        self.assertEqual(len(set(keys)), len(keys))
        for key in keys:
            # Forks' pull requests mount main's disks too: a disk of their own was never saved (pull_request jobs
            # cannot), so they built from scratch.
            self.assertTrue(key.endswith("-own"), key)
        self.assertEqual(commits, ["${{ env.SAVE_BUILD_DISK }}"] * len(keys))


if __name__ == "__main__":
    unittest.main()
