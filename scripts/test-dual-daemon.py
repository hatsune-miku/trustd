#!/usr/bin/env python3
"""Unix CI smoke test using two real processes with different OS identities.

Requires an ordinary account with passwordless sudo. Does not install services.
"""
import json
import os
from pathlib import Path
import signal
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid
import urllib.error
import urllib.request


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(base, path, body=None, session=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if session:
        headers["Authorization"] = "Bearer " + session
    req = urllib.request.Request(base + path, data, headers)
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=15) as response:
        return json.load(response)


def wait_ready(base, process):
    for _ in range(100):
        if process.poll() is not None:
            raise RuntimeError("daemon exited during startup")
        try:
            return request(base, "/health")
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise RuntimeError("daemon did not become ready")


def signal_group(process, elevated, sig):
    # start_new_session=True makes this child's PID its process group ID.
    # Use killpg directly: negative PIDs can be parsed as options by /bin/kill.
    if elevated:
        subprocess.run(
            ["sudo", "-n", "--", sys.executable, "-c",
             "import os, sys\n"
             "try: os.killpg(int(sys.argv[1]), int(sys.argv[2]))\n"
             "except ProcessLookupError: pass\n",
             str(process.pid), str(int(sig))],
            check=True, timeout=5,
        )
    else:
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass  # The process group exited between poll() and killpg().


def stop_processes(processes):
    errors = []
    for process, elevated in reversed(processes):
        if process.poll() is not None:
            continue
        try:
            try:
                signal_group(process, elevated, signal.SIGTERM)
                process.wait(timeout=10)
            except (OSError, subprocess.SubprocessError) as error:
                print(f"Daemon {process.pid} did not stop after SIGTERM ({error}); sending SIGKILL.",
                      file=sys.stderr)
                signal_group(process, elevated, signal.SIGKILL)
                process.wait(timeout=5)
        except (OSError, subprocess.SubprocessError) as error:
            errors.append(f"daemon {process.pid}: {error}")
    if errors:
        raise RuntimeError("Failed to clean up " + "; ".join(errors))


class Agent:
    """A real named process; only authenticated management sets its path policy."""

    def __init__(self, binary, directory, endpoint, policy_base, session):
        directory.mkdir()
        executable = directory / "codex"
        shutil.copy2(binary.with_name("trustd-stdio"), executable)
        self.process = subprocess.Popen(
            [str(executable), "--endpoint", endpoint], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.counter = 0
        try:
            self.initialize()
            for _ in range(100):
                agents = request(policy_base, "/v1/admin/agents", session=session)["agents"]
                agent = next((a for a in agents if any(p["pid"] == self.process.pid for p in a["processes"])), None)
                if agent:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError("real agent process was not discovered")
            assert Path(agent["path"]).resolve() == executable.resolve(), agent
            request(policy_base, "/v1/admin/agents/policy", {
                "id": agent["id"], "revision": agent["revision"], "policy": "once",
                "command_id": str(uuid.uuid4()),
            }, session)
            self.agent_id = agent["id"]
        except BaseException:
            self.close()
            raise

    def initialize(self):
        response = self.rpc("initialize", {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "trustd-ci", "version": "1"},
        })
        assert "error" not in response, response
        assert response["result"]["protocolVersion"] == "2025-06-18", response
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.process.stdin.flush()

    def rpc(self, method, params):
        self.counter += 1
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.counter,
                                            "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        assert select.select([self.process.stdout], [], [], 75)[0], "stdio request timed out"
        line = self.process.stdout.readline()
        assert line, "stdio terminated without a response"
        response = json.loads(line)
        assert response.get("id") == self.counter, response
        return response

    def call(self, name, arguments=None):
        return self.rpc("tools/call", {"name": name, "arguments": arguments or {}})

    def enter(self):
        response = self.call("enter_interactive", {"agent_name": "untrusted display name",
                                                   "request_reason": "CI identity check", "confirm_text": "OK"})
        assert "error" not in response, response
        result = response["result"]["structuredContent"]
        assert result["status"] == "authorized", response
        assert all(field not in result for field in ("handle", "token", "automatic", "reused", "policy"))
        return result

    def run(self, sudo):
        response = self.call("run", {"command": "id -u", "sudo": sudo, "timeout_seconds": 10})
        if "error" in response or response.get("result", {}).get("isError"):
            return response
        return response["result"]["structuredContent"]

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)


def main():
    assert os.name == "posix" and os.geteuid() != 0, "run as an ordinary Unix user with sudo access"
    binary = Path(sys.argv[1]).resolve()
    subprocess.run(["sudo", "-n", "true"], check=True)
    root_port, user_port = port(), port()
    while user_port == root_port:
        user_port = port()
    processes = []
    clients = []
    with tempfile.TemporaryDirectory(prefix="localshelld-dual-") as directory:
        temp = Path(directory)
        password = "localshelld CI test password"
        credential = {"type": "password", "value": password}
        try:
            for user, listen in [(False, root_port), (True, user_port)]:
                config = temp / ("user" if user else "root") / "config.toml"
                mode = ["--user"] if user else []
                subprocess.run([str(binary), "init", *mode, "--config", str(config), "--password-stdin"],
                               input=password + "\n", text=True, check=True, stdout=subprocess.DEVNULL)
                text = config.read_text().replace(
                    f'bind = "127.0.0.1:{32120 if user else 32119}"', f'bind = "127.0.0.1:{listen}"'
                ).replace('privileged_daemon = "127.0.0.1:32119"', f'privileged_daemon = "127.0.0.1:{root_port}"')
                config.write_text(text)
                prefix = [] if user else ["sudo", "-n", "--"]
                daemon = binary.with_name("trustd-user-headless" if user else "trustd-root")
                process = subprocess.Popen([*prefix, str(daemon), "--config", str(config)],
                                           start_new_session=True, stdout=subprocess.DEVNULL)
                processes.append((process, not user))
                health = wait_ready(f"http://127.0.0.1:{listen}", process)
                assert health["daemon"] == ("user" if user else "privileged")
            root_url, user_url = f"http://127.0.0.1:{root_port}", f"http://127.0.0.1:{user_port}"
            root_session = request(root_url, "/v1/admin/login", {"credential": credential})["session"]
            user_session = request(user_url, "/v1/admin/login", {"credential": credential})["session"]
            local = Agent(binary, temp / "local agent", f"127.0.0.1:{user_port}", user_url, user_session)
            clients.append(local)
            assert not local.enter()["sudo_available"]
            result = local.run(False)
            assert result["success"] and int(result["stdout"].strip()) == os.geteuid(), result
            rejected = local.run(True)
            assert "error" in rejected or rejected.get("result", {}).get("isError"), rejected
            local.call("destroy")
            # Pair only through authenticated human management.
            pairing = request(user_url, "/v1/admin/pairing/new", {}, user_session)
            request(root_url, "/v1/admin/pairing/connect", {
                "code": pairing["code"], "user_daemon": f"127.0.0.1:{user_port}"
            }, root_session)
            for label, endpoint, base, session in [
                ("paired user", f"127.0.0.1:{user_port}", user_url, user_session),
                ("paired root", f"127.0.0.1:{root_port}", root_url, root_session),
            ]:
                previous = {r["id"] for r in request(root_url, "/v1/admin/sessions/list", {}, root_session)}
                client = Agent(binary, temp / label, endpoint, base, session)
                clients.append(client)
                assert client.enter()["sudo_available"]
                listing = request(user_url, "/v1/admin/agents", session=user_session)["agents"]
                assert next(a for a in listing if a["id"] == client.agent_id)["policy"] == "ask"
                for sudo, uid in [(False, os.geteuid()), (True, 0)]:
                    result = client.run(sudo)
                    assert result["success"] and int(result["stdout"].strip()) == uid, result
                records = request(root_url, "/v1/admin/sessions/list", {}, root_session)
                created = [r for r in records if r["id"] not in previous]
                assert len(created) == 1, created
                request(root_url, "/v1/admin/sessions/revoke", {"id": created[0]["id"]}, root_session)
                rejected = client.run(False)
                assert "error" in rejected or rejected.get("result", {}).get("isError"), rejected
            rejected = subprocess.run(["sudo", "-n", "--", str(binary.with_name("trustd-user-headless")), "--config", str(config)],
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=10)
            assert rejected.returncode != 0 and "non-elevated" in rejected.stderr
            print("Dual daemon identities verified: sudo=false is current user; sudo=true is root.")
        finally:
            for client in reversed(clients):
                client.close()
            stop_processes(processes)


if __name__ == "__main__":
    main()
