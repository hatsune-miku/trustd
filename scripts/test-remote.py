#!/usr/bin/env python3
"""Exercise paired real hosts through their relays; never installs services.

--hosts reads a private JSON array of {name, endpoint, device_id, admin_session,
cwd, platform}. endpoint is a local management URL, optionally forwarded by SSH;
host_header can supply the original loopback host:port for such a forward.
platform is windows, linux or macos. Use dedicated test configurations.
--codex additionally uses each target's existing Codex login.
"""
import argparse
import base64
import hashlib
import json
import shlex
from pathlib import Path
import time
import urllib.error
import urllib.request
import uuid


class Hosts:
    def __init__(self, hosts):
        self.hosts = hosts
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(self, source, target, operation, arguments=None):
        headers = {"Content-Type": "application/json", "Authorization": "Bearer " + source["admin_session"]}
        if source.get("host_header"):
            headers["Host"] = source["host_header"]
        body = {"target": target["device_id"], "operation": operation, "arguments": arguments or {}}
        request = urllib.request.Request(source["endpoint"].rstrip("/") + "/v2/admin/remote/invoke",
                                         json.dumps(body).encode(), headers)
        try:
            with self.http.open(request, timeout=120) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{operation}: HTTP {error.code}: {error.read().decode()}") from None

    def execution(self, target, command):
        identity = {"privilege_level": "current-user"} if target["platform"] == "windows" else {"sudo": False}
        return {"command": command, **identity}

    def job_finished(self, source, target, job_id):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            job = self.call(source, target, "job_get", {"job_id": job_id})
            if job.get("finished_at"):
                assert job["status"] == "succeeded", job
                return job
            time.sleep(.25)
        raise AssertionError("test job did not finish")

    def turn_finished(self, source, target, agent_id, expected):
        deadline = time.monotonic() + 240
        while time.monotonic() < deadline:
            agent = self.call(source, target, "agent_get", {"agent_id": agent_id})
            if agent["status"] in ("idle", "failed", "stopped", "waiting_user"):
                assert agent["status"] == "idle", agent
                assert expected in agent["last_message"], agent
                return
            time.sleep(1)
        raise AssertionError("test Codex turn did not finish")

    def execution_and_files(self):
        for source in self.hosts:
            for target in self.hosts:
                if source is target:
                    continue
                session = self.call(source, target, "session_open", {"cwd": target["cwd"]})["session_ref"]
                try:
                    command = "Write-Output 'TRUSTD_REMOTE_OK'" if target["platform"] == "windows" else "printf TRUSTD_REMOTE_OK"
                    result = self.call(source, target, "run", {"session_ref": session, **self.execution(target, command)})
                    assert result["success"] and "TRUSTD_REMOTE_OK" in result["stdout"], result
                finally:
                    self.call(source, target, "session_close", {"session_ref": session})
                print(f"PASS {source['name']} -> {target['name']} execution", flush=True)
        for target in self.hosts:
            source = next(host for host in self.hosts if host is not target)
            session = self.call(source, target, "session_open", {"cwd": target["cwd"]})["session_ref"]
            command = "Start-Sleep -Seconds 2; 'x' * 150000; 'DURABLE_OK'" if target["platform"] == "windows" else "sleep 2; head -c 150000 /dev/zero | tr '\\0' x; printf DURABLE_OK"
            args = {"session_ref": session, "idempotency_key": str(uuid.uuid4()), **self.execution(target, command)}
            job = self.call(source, target, "job_start", args)
            assert self.call(source, target, "job_start", args)["id"] == job["id"]
            self.call(source, target, "session_close", {"session_ref": session})
            self.job_finished(source, target, job["id"])
            output = bytearray()
            while True:
                chunk = self.call(source, target, "job_output", {"job_id": job["id"], "offset": len(output)})
                output.extend(base64.b64decode(chunk["data"]))
                if chunk["eof"]:
                    break
            assert len(output) > 150000 and b"DURABLE_OK" in output
            session = self.call(source, target, "session_open", {"cwd": target["cwd"]})["session_ref"]
            try:
                payload = bytes(range(256)) * 300
                path = target["cwd"].rstrip("/\\") + "/trustd-test-" + str(uuid.uuid4()) + ".bin"
                transfer = str(uuid.uuid4())
                for offset in range(0, len(payload), 32768):
                    part = payload[offset:offset + 32768]
                    args = {"session_ref": session, "path": path, "transfer_id": transfer, "offset": offset,
                            "data": base64.b64encode(part).decode(), "complete": offset + len(part) == len(payload)}
                    result = self.call(source, target, "file_write", args)
                    assert self.call(source, target, "file_write", args) == result
                assert result["sha256"] == hashlib.sha256(payload).hexdigest()
                output = bytearray()
                while True:
                    chunk = self.call(source, target, "file_read", {"session_ref": session, "path": path, "offset": len(output)})
                    output.extend(base64.b64decode(chunk["data"]))
                    if chunk["eof"]:
                        break
                assert output == payload
            finally:
                self.call(source, target, "session_close", {"session_ref": session})
            print(f"PASS {target['name']} independent job, idempotency, paged output, upload retry and checksum", flush=True)

    def codex(self):
        for target in self.hosts:
            source = next(host for host in self.hosts if host is not target)
            session = self.call(source, target, "session_open", {"cwd": target["cwd"]})["session_ref"]
            agent_id = None
            try:
                workspace = target["cwd"].rstrip("/\\") + "/codex-work-" + str(uuid.uuid4())
                quoted = "'" + workspace.replace("'", "''") + "'" if target["platform"] == "windows" else shlex.quote(workspace)
                mkdir = f"[IO.Directory]::CreateDirectory({quoted}) | Out-Null" if target["platform"] == "windows" else f"mkdir -p -- {quoted}"
                assert self.call(source, target, "run", {"session_ref": session, **self.execution(target, mkdir)})["success"]
                agent = self.call(source, target, "agent_start", {"session_ref": session, "cwd": workspace,
                                  "approval_policy": "never", "sandbox": "danger-full-access"})
                agent_id = agent["id"]
                self.call(source, target, "agent_send", {"agent_id": agent_id, "message": "Reply exactly TRUSTD_CODEX_OK. Do not call tools."})
                self.turn_finished(source, target, agent_id, "TRUSTD_CODEX_OK")
                command = f"if ((Get-Location).Path.Replace('\\','/') -ne {quoted}.Replace('\\','/')) {{ throw 'wrong delegated cwd' }}; Start-Sleep -Seconds 15; Write-Output 'CODEX_DURABLE_OK'" if target["platform"] == "windows" else f"test \"$PWD\" = {quoted} && sleep 15 && printf CODEX_DURABLE_OK"
                args = {"idempotency_key": str(uuid.uuid4()), **self.execution(target, command)}
                self.call(source, target, "agent_send", {"agent_id": agent_id, "message":
                    "This is an authorized integration test. Call trustd_remote job_start exactly once with these arguments: "
                    + json.dumps(args) + ". Return the job ID immediately without waiting or using other tools."})
                deadline = time.monotonic() + 240
                job = None
                while time.monotonic() < deadline:
                    jobs = self.call(source, target, "job_list")["jobs"]
                    job = next((j for j in jobs if j.get("agent_id") == agent_id and "CODEX_DURABLE_OK" in j["command"]), None)
                    if job:
                        break
                    time.sleep(1)
                assert job, "Codex did not submit the test job"
                self.call(source, target, "agent_interrupt", {"agent_id": agent_id})
                self.call(source, target, "agent_close", {"agent_id": agent_id})
                self.call(source, target, "session_close", {"session_ref": session})
                session = self.call(source, target, "session_open", {"cwd": target["cwd"]})["session_ref"]
                self.job_finished(source, target, job["id"])
                self.call(source, target, "agent_resume", {"agent_id": agent_id, "session_ref": session, "cwd": workspace})
                self.call(source, target, "agent_send", {"agent_id": agent_id, "message": "Use trustd_remote job_get to check job " + job["id"] + ". If it succeeded reply exactly TRUSTD_RESUME_OK."})
                self.turn_finished(source, target, agent_id, "TRUSTD_RESUME_OK")
                print(f"PASS {target['name']} Codex conversation, delegated job, interrupt, close and resume", flush=True)
            finally:
                if agent_id:
                    self.call(source, target, "agent_close", {"agent_id": agent_id})
                self.call(source, target, "session_close", {"session_ref": session})


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hosts", type=Path, required=True)
    parser.add_argument("--codex", action="store_true")
    args = parser.parse_args()
    hosts = json.loads(args.hosts.read_text(encoding="utf-8"))
    assert len(hosts) >= 2, "at least two paired hosts are required"
    suite = Hosts(hosts)
    suite.execution_and_files()
    if args.codex:
        suite.codex()
