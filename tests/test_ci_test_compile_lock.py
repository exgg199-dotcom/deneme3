"""Selective compiler locking preserves parallelism and the configured cache wrapper."""
import errno
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
WRAPPER = ROOT / "tools/ci/test-compile-lock.sh"


@unittest.skipIf(os.name == "nt", "requires Bash, flock and pipe readiness")
class TestCompileLockTests(unittest.TestCase):
    def test_background_compiler_child_cannot_keep_the_lock_after_compiler_exit(self):
        import signal

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pid_file = root / "child-pid"
            compiler = root / "fake-rustc"
            compiler.write_text(f"#!{sys.executable}\n"
                                "import os, pathlib, subprocess, sys\n"
                                "if '--spawn-child' in sys.argv:\n"
                                "    child = subprocess.Popen(['sleep', '30'], close_fds=False,\n"
                                "                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
                                "    pathlib.Path(os.environ['CHILD_PID']).write_text(str(child.pid))\n"
                                "sys.exit(7)\n")
            compiler.chmod(0o755)
            env = dict(os.environ, RUNNER_TEMP=directory, CHILD_PID=str(pid_file),
                       HORIZON_TEST_RUSTC_WRAPPER="")
            try:
                first = subprocess.run([WRAPPER, compiler, '--crate-name', 'horizon_client_lib',
                                        '--test', '--spawn-child'], env=env, timeout=5)
                self.assertEqual(first.returncode, 7)
                child_pid = int(pid_file.read_text())
                os.kill(child_pid, 0)  # Descendant is still alive when the second call runs.
                second = subprocess.run([WRAPPER, compiler, '--crate-name', 'horizon_client',
                                         '--test'], env=env, timeout=5)
                self.assertEqual(second.returncode, 7)
                os.kill(child_pid, 0)
            finally:
                if pid_file.exists():
                    os.kill(int(pid_file.read_text()), signal.SIGTERM)

    def test_python_fallback_forwards_cargo_jobserver_descriptors(self):
        with tempfile.TemporaryDirectory() as directory:
            read_fd, write_fd = os.pipe()
            try:
                os.write(write_fd, b"+")
                env = dict(os.environ, CARGO_MAKEFLAGS=f"-j --jobserver-auth={read_fd},{write_fd}")
                command = [sys.executable, str(WRAPPER.with_name("test_compile_lock.py")),
                           str(Path(directory) / "compile.lock"), sys.executable, "-c",
                           "import os,sys; r,w=map(int,sys.argv[1:]); "
                           "assert os.read(r,1)==b'+'; os.write(w,b'+'); print('inherited')",
                           str(read_fd), str(write_fd)]
                result = subprocess.run(command, env=env, pass_fds=(read_fd, write_fd),
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "inherited\n")
                self.assertTrue(select.select([read_fd], [], [], 1)[0])
                self.assertEqual(os.read(read_fd, 1), b"+")
            finally:
                os.close(read_fd)
                os.close(write_fd)

    def test_python_fallback_does_not_forward_files_as_a_jobserver(self):
        with tempfile.TemporaryDirectory() as directory, \
                tempfile.TemporaryFile() as first, tempfile.TemporaryFile() as second:
            pair = (first.fileno(), second.fileno())
            self.check_invalid_jobserver(directory, pair, pair)

    def test_python_fallback_does_not_forward_a_partial_jobserver_pair(self):
        with tempfile.TemporaryDirectory() as directory:
            read_fd, write_fd = os.pipe()
            os.close(write_fd)
            try:
                self.check_invalid_jobserver(directory, (read_fd, write_fd), (read_fd,))
            finally:
                os.close(read_fd)

    def test_python_fallback_discards_out_of_range_jobserver_numbers(self):
        with tempfile.TemporaryDirectory() as directory:
            self.check_invalid_jobserver(directory, (10 ** 100, 10 ** 100 + 1), ())

    def test_python_fallback_honors_disabled_jobserver_descriptors(self):
        # GNU jobservers explicitly disable inheritance with negative fd numbers.
        with tempfile.TemporaryDirectory() as directory:
            for pair in [(-2, -2), (-1, 42), (42, -1)]:
                with self.subTest(pair=pair):
                    self.check_invalid_jobserver(directory, pair, ())

    def test_malformed_jobserver_flags_cannot_bypass_the_serial_fallback(self):
        with tempfile.TemporaryDirectory() as directory:
            for advertised in ("broken", "fifo:", "3,4,5", ""):
                with self.subTest(advertised=advertised):
                    flags = f"-j --jobserver-auth=-2,-2 --jobserver-auth={advertised} --verbose"
                    result = subprocess.run(
                        [sys.executable, str(WRAPPER.with_name("test_compile_lock.py")),
                         str(Path(directory) / "compile.lock"), sys.executable, "-c",
                         "import os; print(os.environ['CARGO_MAKEFLAGS'])"],
                        env=dict(os.environ, CARGO_MAKEFLAGS=flags),
                        capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.split(), ["-j1", "--verbose"])

    def test_python_fallback_preserves_named_fifo_advertisements(self):
        with tempfile.TemporaryDirectory() as directory:
            flags = "-j --jobserver-auth=fifo:/private/make-pipe --verbose"
            result = subprocess.run(
                [sys.executable, str(WRAPPER.with_name("test_compile_lock.py")),
                 str(Path(directory) / "compile.lock"), sys.executable, "-c",
                 "import os; print(os.environ['CARGO_MAKEFLAGS'])"],
                env=dict(os.environ, CARGO_MAKEFLAGS=flags),
                capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), flags)

    def test_python_fallback_preserves_read_write_fifo_descriptors(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "jobserver"
            try:
                os.mkfifo(path)
            except OSError as error:
                if error.errno in (errno.ENOTSUP, errno.EOPNOTSUPP):
                    self.skipTest("temporary filesystem does not support named pipes")
                raise
            first = os.open(path, os.O_RDWR | os.O_NONBLOCK)
            second = os.dup(first)
            try:
                os.write(second, b"+")
                flags = f"-j --jobserver-auth={first},{second}"
                result = subprocess.run(
                    [sys.executable, str(WRAPPER.with_name("test_compile_lock.py")),
                     str(Path(directory) / "compile.lock"), sys.executable, "-c",
                     "import os,sys; r,w=map(int,sys.argv[1:]); "
                     "assert os.read(r,1)==b'+'; os.write(w,b'+')",
                     str(first), str(second)],
                    env=dict(os.environ, CARGO_MAKEFLAGS=flags), pass_fds=(first, second),
                    capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(select.select([first], [], [], 1)[0])
                self.assertEqual(os.read(first, 1), b"+")
            finally:
                os.close(first)
                os.close(second)

    def check_invalid_jobserver(self, directory, advertised, inherited):
        flags = f"-j --jobserver-auth={advertised[0]},{advertised[1]} --verbose"
        env = dict(os.environ, CARGO_MAKEFLAGS=flags, MAKEFLAGS=flags)
        script = """import json,os,sys
live=[]
for value in sys.argv[1:]:
 try:os.fstat(int(value));live.append(int(value))
 except OSError:pass
print(json.dumps({'live':live,'cargo':os.environ['CARGO_MAKEFLAGS'],'make':os.environ['MAKEFLAGS']}))
"""
        result = subprocess.run(
            [sys.executable, str(WRAPPER.with_name("test_compile_lock.py")),
             str(Path(directory) / "compile.lock"), sys.executable, "-c", script,
             *map(str, inherited)], env=env, pass_fds=inherited,
            capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output['live'], [], "unrelated or partial descriptors escaped to the compiler")
        for name in ('cargo', 'make'):
            self.assertNotIn('--jobserver-', output[name])
            self.assertIn('--verbose', output[name])
            self.assertNotIn('-j', output[name].split(), "missing jobserver must not mean unlimited jobs")

    def test_response_file_and_equals_form_take_the_lock_without_rewriting_arguments(self):
        import fcntl

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiler = root / "fake-rustc"
            compiler.write_text(f"#!{sys.executable}\n"
                                "import json, sys\n"
                                "print(json.dumps(sys.argv[1:]), flush=True)\n"
                                "sys.exit(7)\n")
            compiler.chmod(0o755)
            response = root / "arguments with spaces"
            response.write_text('--crate-name\nhorizon_client_lib\n--cfg\nfeature="with spaces"\n--test')
            env = dict(os.environ, RUNNER_TEMP=directory, HORIZON_TEST_RUSTC_WRAPPER="")
            for args in (["@" + str(response)], ["--crate-name=horizon_client", "--test"]):
                with self.subTest(args=args), (root / "horizon-lib-test-compile.lock").open("w") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX)
                    process = subprocess.Popen([WRAPPER, compiler, *args], env=env,
                                               stdout=subprocess.PIPE, text=True)
                    try:
                        self.assertFalse(select.select([process.stdout], [], [], 0.2)[0],
                                         "compiler bypassed the held test-unit lock")
                        fcntl.flock(lock, fcntl.LOCK_UN)
                        output, _ = process.communicate(timeout=5)
                        self.assertEqual(process.returncode, 7)
                        self.assertEqual(json.loads(output), args)
                    finally:
                        if process.poll() is None:
                            process.kill()
                        process.communicate()

    def test_large_test_units_serialize_while_other_calls_pass_through(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiler = root / "fake-rustc"
            compiler.write_text(f"#!{sys.executable}\n"
                                "import json, sys\n"
                                "print(json.dumps(sys.argv[1:]), flush=True)\n"
                                "if '--hold' in sys.argv: sys.stdin.readline()\n"
                                "sys.exit(7)\n")
            compiler.chmod(0o755)
            cache = root / "cache-wrapper"
            cache.write_text('#!/usr/bin/env bash\necho cached >> "$RUNNER_TEMP/cache-calls"\nexec "$@"\n')
            cache.chmod(0o755)
            env = dict(os.environ, RUNNER_TEMP=directory,
                       HORIZON_TEST_RUSTC_WRAPPER=str(cache))
            args = ["--crate-name", "horizon_client_lib", "--test", "--hold"]
            first = subprocess.Popen([WRAPPER, compiler, *args], env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            second = None
            try:
                self.assertTrue(select.select([first.stdout], [], [], 5)[0])
                self.assertEqual(json.loads(first.stdout.readline()), args)
                second_args = ["--test", "--crate-name", "horizon_client", "--cfg", 'feature="with spaces"']
                second = subprocess.Popen([WRAPPER, compiler, *second_args], env=env,
                                          stdout=subprocess.PIPE, text=True)
                self.assertFalse(select.select([second.stdout], [], [], 0.2)[0],
                                 "second large test started before first released its lock")
                for other_args in (["--crate-name", "unrelated", "--test"],
                                   ["--crate-name", "horizon_client"],
                                   ["--crate-name", "horizon_client_lib_extra", "--test"]):
                    result = subprocess.run([WRAPPER, compiler, *other_args], env=env,
                                            capture_output=True, text=True, timeout=5)
                    self.assertEqual(result.returncode, 7)
                    self.assertEqual(json.loads(result.stdout), other_args)
                first.communicate("release\n", timeout=5)
                output, _ = second.communicate(timeout=5)
                self.assertEqual(first.returncode, 7)
                self.assertEqual(second.returncode, 7)
                self.assertEqual(json.loads(output), second_args)
                self.assertEqual((root / "cache-calls").read_text().splitlines(), ["cached"] * 5)
                # An unset upstream wrapper executes the compiler directly too.
                env.pop("HORIZON_TEST_RUSTC_WRAPPER")
                direct = subprocess.run([WRAPPER, compiler, "--version"], env=env,
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(direct.returncode, 7)
                self.assertEqual(json.loads(direct.stdout), ["--version"])
            finally:
                for process in (first, second):
                    if process is not None:
                        if process.poll() is None:
                            process.kill()
                        process.communicate()
