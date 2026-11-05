"""Exercise a Linux headless bundle without installing services or using real data."""
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request


def main():
    assert sys.platform == "linux", "Linux headless bundle only"
    bundle = Path(sys.argv[1]).resolve()
    for role in ("trustd", "trustd-root", "trustd-user-headless", "trustd-stdio"):
        binary = bundle / role
        libraries = subprocess.run(["ldd", str(binary)], check=True, capture_output=True, text=True).stdout
        assert "not found" not in libraries, libraries
        assert not re.search(r"lib(?:gtk|gdk|webkit|javascriptcoregtk|cairo|pango|X11|wayland)", libraries, re.I), libraries
        subprocess.run([str(binary), "--version"], check=True)

    with tempfile.TemporaryDirectory(prefix="trustd-headless-smoke-") as directory:
        temp = Path(directory)
        # In the CI container, launch the user daemon as nobody, never as root.
        identity = {}
        if os.geteuid() == 0:
            os.chown(temp, 65534, 65534)
            identity = {"user": 65534, "group": 65534, "extra_groups": []}
        env = {"PATH": "/usr/bin:/bin", "HOME": str(temp), "DISPLAY": ":99999",
               "WAYLAND_DISPLAY": "nonexistent-display", "XDG_CONFIG_HOME": str(temp / "config")}
        config = temp / "config.toml"
        password = "headless smoke fixture password"
        subprocess.run([str(bundle / "trustd"), "init", "--user", "--config", str(config), "--password-stdin"],
                       input=password + "\n", text=True, check=True, env=env, **identity)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        config.write_text(config.read_text().replace('bind = "127.0.0.1:32120"', f'bind = "127.0.0.1:{port}"'))
        base = f"http://127.0.0.1:{port}"
        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def request(path, body=None, session=None):
            headers = {"Content-Type": "application/json"}
            if session:
                headers["Authorization"] = "Bearer " + session
            data = None if body is None else json.dumps(body).encode()
            with http.open(urllib.request.Request(base + path, data, headers), timeout=3) as response:
                return response.read()

        with (temp / "daemon.log").open("w+") as log:
            # Deliberately omit --headless and leave fake display variables set:
            # the build itself must be independent of any desktop environment.
            process = subprocess.Popen([str(bundle / "trustd-user-headless"), "--config", str(config)],
                                       env=env, stdout=log, stderr=log, **identity)
            try:
                deadline = time.monotonic() + 20
                while True:
                    if process.poll() is not None:
                        log.seek(0)
                        raise AssertionError("user daemon exited: " + log.read())
                    try:
                        health = json.loads(request("/health"))
                        break
                    except OSError:
                        if time.monotonic() >= deadline:
                            log.seek(0)
                            raise AssertionError("user daemon not healthy: " + log.read())
                        time.sleep(0.1)
                assert health["status"] == "ok" and health["daemon"] == "user", health
                assert b"<html" in request("/").lower(), "management UI must remain embedded"
                session = json.loads(request("/v1/admin/login", {"credential": {"type": "password", "value": password}}))["session"]
                settings = json.loads(request("/v1/admin/settings", session=session))
                assert settings["desktop_available"] is False, settings
                print("Headless user daemon, management page and authenticated settings: OK")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)


if __name__ == "__main__":
    main()
