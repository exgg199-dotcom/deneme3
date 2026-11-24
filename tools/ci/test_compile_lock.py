"""POSIX fallback for test-compile-lock.sh where util-linux flock is absent."""
import fcntl
import os
import re
import stat
import subprocess
import sys


JOBSERVER = re.compile(r"(?<!\S)--jobserver-(fds|auth)=(\S*)(?=\s|$)")
PIPE_DESCRIPTORS = re.compile(r"([+-]?\d+),([+-]?\d+)")


def jobserver_environment():
    # Capture before opening the lock: a stale advertised descriptor must not
    # accidentally name our new lock file and let a compiler child inherit it.
    descriptors = set()
    environment = os.environ.copy()
    for key in ("CARGO_MAKEFLAGS", "MAKEFLAGS"):
        if key not in environment:
            continue
        invalid = False
        usable = False

        def validate(match):
            nonlocal invalid, usable
            kind, value = match.groups()
            if kind == "auth" and value.startswith("fifo:") and value[5:]:
                usable = True
                return match.group(0)
            pair = PIPE_DESCRIPTORS.fullmatch(value)
            if pair is None:
                invalid = True
                return ""
            try:
                read_fd, write_fd = map(int, pair.groups())
                valid = (
                    read_fd >= 3 and write_fd >= 3 and read_fd != write_fd
                    and stat.S_ISFIFO(os.fstat(read_fd).st_mode)
                    and stat.S_ISFIFO(os.fstat(write_fd).st_mode)
                    and fcntl.fcntl(read_fd, fcntl.F_GETFL) & os.O_ACCMODE in (os.O_RDONLY, os.O_RDWR)
                    and fcntl.fcntl(write_fd, fcntl.F_GETFL) & os.O_ACCMODE in (os.O_WRONLY, os.O_RDWR)
                )
            except (OSError, OverflowError, ValueError):
                valid = False
            # Darwin's two pipe ends have different inode identities.
            if valid:
                usable = True
                descriptors.update((read_fd, write_fd))
                return match.group(0)
            invalid = True
            return ""

        flags = JOBSERVER.sub(validate, environment[key])
        if invalid and not usable:
            # Without its jobserver, a bare -j would give a child make unlimited jobs.
            flags = re.sub(r"(?<!\S)-j(?=\s|$)", "-j1", flags)
        environment[key] = flags
    return tuple(sorted(descriptors)), environment


def main(args):
    if len(args) < 2:
        raise SystemExit("usage: test_compile_lock.py LOCK COMMAND [ARG ...]")
    inherited, environment = jobserver_environment()
    with open(args[0], "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        status = subprocess.run(args[1:], pass_fds=inherited, env=environment).returncode
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
