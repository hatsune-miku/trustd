"""Real Linux TTY prompts without installing services or requesting sudo."""
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time
import unittest

if sys.platform == "linux":
    import pty
    import termios


ROOT = Path(__file__).resolve().parent.parent
FIXTURE = "maintenance::terminal::tests::native_tty_fixture"


@unittest.skipUnless(sys.platform == "linux", "Linux terminal wizard only")
class TerminalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        result = subprocess.run(
            ["cargo", "test", "--bin", "trustd", "--no-run", "--message-format=json"],
            cwd=ROOT, capture_output=True, text=True,
        )
        artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
        if result.returncode:
            diagnostics = "".join(item["message"].get("rendered", "") for item in artifacts
                                  if item.get("reason") == "compiler-message")
            raise RuntimeError(result.stderr + diagnostics)
        cls.binary = next(item["executable"] for item in artifacts
                          if item.get("reason") == "compiler-artifact" and item.get("executable")
                          and item["target"]["name"] == "trustd" and item["profile"]["test"])

    def interact(self, answers, piped_stdin=False, fixture=FIXTURE):
        pid, master = pty.fork()
        if pid == 0:
            os.chdir(ROOT)
            if piped_stdin:
                read_fd, write_fd = os.pipe()
                os.write(write_fd, b"pipeline contents must not become answers\n")
                os.close(write_fd)
                os.dup2(read_fd, 0)
                os.close(read_fd)
            os.execv(self.binary, [self.binary, "--exact", fixture, "--ignored", "--nocapture"])
        transcript = bytearray()
        pending = list(answers)
        deadline = time.monotonic() + 15
        status = None
        scan_from = 0
        try:
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.05)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        chunk = b""
                    transcript.extend(chunk)
                if pending:
                    prompt, answer, secret = pending[0]
                    found = transcript.find(prompt.encode(), scan_from)
                    echo = termios.tcgetattr(master)[3] & termios.ECHO
                    if found >= 0 and (not secret or not echo):
                        scan_from = len(transcript)
                        os.write(master, answer)
                        pending.pop(0)
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    # Drain output written just before exit.
                    while select.select([master], [], [], 0)[0]:
                        try:
                            chunk = os.read(master, 65536)
                        except OSError:
                            break
                        if not chunk:
                            break
                        transcript.extend(chunk)
                    self.assertFalse(pending, transcript.decode(errors="replace"))
                    self.assertTrue(termios.tcgetattr(master)[3] & termios.ECHO,
                                    "terminal echo must be restored")
                    return os.waitstatus_to_exitcode(status), transcript.decode(errors="replace")
                status = None
            self.fail("TTY fixture timed out: " + transcript.decode(errors="replace"))
        finally:
            if status is None:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            os.close(master)

    def test_password_hidden_and_controlling_terminal_used_with_piped_stdin(self):
        for piped_stdin in (False, True):
            with self.subTest(piped_stdin=piped_stdin):
                code, output = self.interact([
                    ("主密码：", " test-终端 \n".encode(), True),
                    ("再次输入：", " test-终端 \n".encode(), True),
                    ("直接回车跳过：", b"1\n", False),
                    ("原配置会备份。 [y/N]：", b"n\n", False),
                    ("并写入所选客户端配置？ [y/N]：", b"y\n", False),
                ], piped_stdin=piped_stdin)
                self.assertEqual(code, 0, output)
                self.assertIn("TTY_FIXTURE_CONFIRMED", output)
                self.assertNotIn("test-终端", output)
                self.assertNotIn("pipeline contents", output)

    def test_default_confirmation_cancels(self):
        code, output = self.interact([
            ("主密码：", b"private\n", True),
            ("再次输入：", b"private\n", True),
            ("直接回车跳过：", b"\n", False),
            ("并写入所选客户端配置？ [y/N]：", b"\n", False),
        ])
        self.assertEqual(code, 0, output)
        self.assertIn("TTY_FIXTURE_CANCELLED", output)
        self.assertNotIn("private", output)

    def test_ssh_guide_prefills_session_and_prints_local_command(self):
        code, output = self.interact([
            ("另一台电脑打开管理页？ [Y/n]：", b"\n", False),
            ("[miku@192.0.2.20]：", b"\n", False),
            ("SSH 端口 [2222]：", b"\n", False),
        ], piped_stdin=True, fixture="maintenance::terminal::tests::native_ssh_guide_fixture")
        self.assertEqual(code, 0, output)
        self.assertIn("ssh -N -o ExitOnForwardFailure=yes", output)
        self.assertIn("-L '127.0.0.1:32120:127.0.0.1:32120' -p 2222 'miku@192.0.2.20'", output)
        self.assertIn("http://localhost:32119/", output)
        self.assertIn("系统管理页填写 127.0.0.1:32120", output)
        self.assertNotIn("pipeline contents", output)

    def test_password_eof_restores_echo_without_confirming(self):
        code, output = self.interact([("主密码：", b"\x04", True)])
        self.assertNotEqual(code, 0, output)
        self.assertNotIn("TTY_FIXTURE_CONFIRMED", output)

    def test_password_interrupt_restores_echo(self):
        code, output = self.interact([("主密码：", b"\x03", True)])
        self.assertNotEqual(code, 0, output)
        self.assertNotIn("TTY_FIXTURE_CONFIRMED", output)

    def test_missing_controlling_terminal_fails_without_reading_stdin(self):
        result = subprocess.run(
            [self.binary, "--exact", FIXTURE, "--ignored", "--nocapture"],
            input="not a password\n", capture_output=True, text=True,
            start_new_session=True, timeout=10, cwd=ROOT,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("交互终端", result.stderr)
        self.assertNotIn("not a password", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
