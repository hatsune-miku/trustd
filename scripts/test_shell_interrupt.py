"""Exercise the embedded Bash bootstrap on the host's actual system Bash."""
import base64
import os
from pathlib import Path
import selectors
import shlex
import signal
import subprocess
import tempfile
import time
import unittest
import uuid

ROOT = Path(__file__).resolve().parent.parent


class BashSession:
    def __init__(self):
        self.temp = tempfile.TemporaryDirectory(prefix="trustd-shell-")
        self.directory = Path(self.temp.name)
        self.child = subprocess.Popen(
            ["/bin/bash", "--noprofile", "--norc", "-c", (ROOT / "src/shell/bash.sh").read_text()],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True, cwd=self.directory,
        )

    def start(self, command, stdin=b""):
        marker = uuid.uuid4().hex
        source = self.directory / (marker + ".sh")
        source.write_text("builtin eval -- " + shlex.quote(command) + "\n")
        input_file = self.directory / (marker + ".stdin")
        input_file.write_bytes(stdin)
        self.child.stdin.write(f"{marker}|{source}|{input_file}\n".encode())
        self.child.stdin.flush()
        return marker

    def finish(self, marker):
        buffers = {"stdout": b"", "stderr": b""}
        result = {}
        needle = b"\x1e" + marker.encode() + b"|"
        deadline = time.monotonic() + 5
        with selectors.DefaultSelector() as selector:
            selector.register(self.child.stdout, selectors.EVENT_READ, "stdout")
            selector.register(self.child.stderr, selectors.EVENT_READ, "stderr")
            while len(result) != 2 and time.monotonic() < deadline:
                for key, _ in selector.select(.1):
                    data = os.read(key.fileobj.fileno(), 8192)
                    if not data:
                        raise AssertionError(f"shell exited unexpectedly: {buffers}")
                    buffers[key.data] += data
                    before, found, tail = buffers[key.data].partition(needle)
                    if found and b"\x1f" in tail:
                        frame = tail.split(b"\x1f", 1)[0]
                        code, cwd = frame.split(b"|", 1)
                        result[key.data] = (before, int(code), base64.b64decode(cwd).decode())
                        selector.unregister(key.fileobj)
        if len(result) != 2:
            raise AssertionError(f"shell did not acknowledge interruption: {buffers}")
        return result

    def interrupt(self):
        pairs = [tuple(map(int, line.split())) for line in subprocess.check_output(
            ["/bin/ps", "-eo", "pid=,ppid="], text=True).splitlines()]
        descendants = [self.child.pid]
        for parent in descendants:
            descendants.extend(pid for pid, ppid in pairs if ppid == parent and pid not in descendants)
        os.kill(self.child.pid, signal.SIGINT)
        for pid in reversed(descendants[1:]):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def close(self):
        try:
            os.killpg(self.child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.child.wait(timeout=5)
        for stream in [self.child.stdin, self.child.stdout, self.child.stderr]:
            stream.close()
        self.temp.cleanup()


@unittest.skipIf(os.name == "nt", "requires the platform's Unix signal semantics")
class BashInterruptTests(unittest.TestCase):
    def setUp(self):
        self.shell = BashSession()

    def tearDown(self):
        self.shell.close()

    def test_timeout_preserves_pid_state_and_skips_remaining_source(self):
        initial_pid = self.shell.child.pid
        for wait in [
            "while :; do :; done", "sleep 30", "sleep 30 | cat",
            "busy() { while :; do :; done; }; busy",
            "inner() { while :; do :; done; }; outer() { inner; echo WRONG-inner; }; outer",
            "inner() { sleep 30; echo WRONG-inner; }; outer() { inner; echo WRONG-outer; }; outer",
        ]:
            marker = self.shell.start("saved=kept; export SAVED_ENV=yes; saved_function() { echo function; }; "
                                      "printf partial; printf error >&2; " + wait + "; echo WRONG")
            time.sleep(.15)
            self.shell.interrupt()
            result = self.shell.finish(marker)
            self.assertEqual(result["stdout"][0], b"partial")
            self.assertNotIn(b"WRONG", result["stdout"][0])
            self.assertIn(b"error", result["stderr"][0])
            next_result = self.shell.finish(self.shell.start("printf '%s/%s\\n' \"$saved\" \"$SAVED_ENV\"; saved_function"))
            self.assertEqual(next_result["stdout"][0], b"kept/yes\nfunction\n")
            self.assertEqual(next_result["stdout"][1], 0)
            self.assertEqual(self.shell.child.pid, initial_pid)

    def test_stdin_return_declare_and_full_parse_remain_correct(self):
        result = self.shell.finish(self.shell.start("declare saved=declared; cat; return 7; echo WRONG", b"input\x00value"))
        self.assertEqual(result["stdout"][0], b"input\x00value")
        self.assertEqual(result["stdout"][1], 7)
        result = self.shell.finish(self.shell.start("echo \"$saved\""))
        self.assertEqual(result["stdout"][0], b"declared\n")
        result = self.shell.finish(self.shell.start("echo WRONG; echo broken |"))
        self.assertNotIn(b"WRONG", result["stdout"][0])
        self.assertNotEqual(result["stdout"][1], 0)


if __name__ == "__main__":
    unittest.main()
