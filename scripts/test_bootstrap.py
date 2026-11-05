"""Bootstrap download/verification/cleanup fixtures. Never install real services."""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parent.parent


class BootstrapTests(unittest.TestCase):
    @unittest.skipIf(os.name == "nt", "POSIX shell fixture")
    def test_shell_download_verification_launch_and_cleanup(self):
        cases = [
            ("linux-x86_64", [], ":0", ""),
            ("linux-x86_64", [], "", "wayland-0"),
            ("linux-x86_64", [], "", ""),
            ("linux-x86_64", ["--headless"], ":0", ""),
            ("linux-x86_64", ["--desktop"], "", ""),
            ("macos-aarch64", [], "", ""),
            ("macos-aarch64", ["--headless", "--no-open"], "", ""),
        ]
        for platform, selectors, display, wayland in cases:
            for mode in ("success", "download", "checksum", "missing", "wizard"):
                with self.subTest(platform=platform, mode=mode), tempfile.TemporaryDirectory(prefix="trustd-bootstrap-test-") as temp:
                    base = Path(temp)
                    mock = base / "mock tools"
                    mock.mkdir()
                    tmp = base / "download space"
                    tmp.mkdir()
                    sentinel = tmp / "keep.txt"
                    sentinel.write_text("unrelated", encoding="utf-8")
                    (base / "release.json").write_text(json.dumps({"tag_name": "v0.1.0-test"}), encoding="utf-8")
                    asset = f"trustd-{platform}.tar.gz"
                    with tarfile.open(base / asset, "w:gz") as archive:
                        for role in ("trustd", "trustd-root", "trustd-user", "trustd-user-headless", "trustd-stdio"):
                            if mode == "missing" and role == "trustd-user-headless":
                                continue
                            content = b'#!/bin/sh\nprintf "%s\\n" "$@" > "$TRUSTD_FIXTURE/launched"\n[ "$TRUSTD_FIXTURE_MODE" != wizard ]\n'
                            info = tarfile.TarInfo(f"trustd-{platform}/{role}")
                            info.size = len(content)
                            info.mode = 0o755
                            archive.addfile(info, io.BytesIO(content))
                    digest = hashlib.sha256((base / asset).read_bytes()).hexdigest()
                    (base / "SHA256SUMS").write_text(f"{'0' * 64 if mode == 'checksum' else digest}  {asset}\n", encoding="utf-8")
                    fixtures = {
                        "id": "#!/bin/sh\nprintf '1000\\n'\n",
                        "uname": f"#!/bin/sh\ncase \"$1\" in -s) echo {'Linux' if platform.startswith('linux') else 'Darwin'};; *) echo {'x86_64' if platform.startswith('linux') else 'arm64'};; esac\n",
                        "curl": '''#!/bin/sh
[ "$TRUSTD_FIXTURE_MODE" != download ] || exit 22
while [ "$#" -gt 0 ]; do
  case "$1" in https://*) url=$1;; -o) shift; output=$1;; esac
  shift
done
case "$url" in */latest) file=release.json;; *) file=${url##*/};; esac
cp "$TRUSTD_FIXTURE/$file" "$output"
''',
                    }
                    for name, content in fixtures.items():
                        path = mock / name
                        path.write_text(content, encoding="utf-8")
                        path.chmod(0o755)
                    arguments = ["--cli"] if platform.startswith("linux") and mode == "success" else []
                    result = subprocess.run(["sh", str(ROOT / "install.sh"), *arguments, *selectors], capture_output=True, text=True, timeout=20,
                                            env={**os.environ, "PATH": str(mock) + os.pathsep + os.environ["PATH"],
                                                 "DISPLAY": display, "WAYLAND_DISPLAY": wayland,
                                                 "TMPDIR": str(tmp), "TRUSTD_FIXTURE": str(base), "TRUSTD_FIXTURE_MODE": mode})
                    self.assertEqual(result.returncode == 0, mode == "success", result.stderr)
                    self.assertEqual((base / "launched").exists(), mode in ("success", "wizard"))
                    if (base / "launched").exists():
                        self.assertEqual((base / "launched").read_text(), "maintenance\n" + "".join(arg + "\n" for arg in [*arguments, *selectors]))
                    self.assertEqual(list(tmp.iterdir()), [sentinel])

    @unittest.skipUnless(os.name == "nt", "Windows PowerShell fixture")
    def test_powershell_download_verification_hidden_launch_and_cleanup(self):
        shells = [shutil.which("powershell"), shutil.which("pwsh")]
        for shell in filter(None, shells):
            for mode in ("success", "download", "checksum", "missing", "wizard"):
                with self.subTest(shell=shell, mode=mode), tempfile.TemporaryDirectory(prefix="trustd-bootstrap-test-") as temp:
                    base = Path(temp)
                    tmp = base / "download space"
                    tmp.mkdir()
                    sentinel = tmp / "keep.txt"
                    sentinel.write_text("unrelated", encoding="utf-8")
                    asset = "trustd-windows-x86_64.zip"
                    with zipfile.ZipFile(base / asset, "w") as archive:
                        for role in ("trustd", "trustd-system", "trustd-user", "trustd-user-headless", "trustd-stdio"):
                            if mode == "missing" and role == "trustd-user-headless":
                                continue
                            archive.writestr(f"trustd-windows-x86_64/{role}.exe", b"never executed")
                    digest = hashlib.sha256((base / asset).read_bytes()).hexdigest()
                    (base / "SHA256SUMS").write_text(f"{'0' * 64 if mode == 'checksum' else digest}  {asset}\n", encoding="utf-8")
                    fixture = base / "fixture.ps1"
                    fixture.write_text(r'''
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
function Invoke-RestMethod { param($Uri, $Headers) return @{ tag_name = 'v0.1.0-test' } }
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, $Uri, $OutFile)
    if ($env:TRUSTD_FIXTURE_MODE -eq 'download') { throw 'fixture download failure' }
    Copy-Item -LiteralPath (Join-Path $env:TRUSTD_FIXTURE ($Uri.Split('/')[-1])) -Destination $OutFile
}
function Start-Process {
    param($FilePath, $ArgumentList, [switch]$PassThru, $WindowStyle)
    if ($WindowStyle -ne 'Hidden' -or $ArgumentList -ne 'maintenance' -or -not $PassThru) { throw 'invalid launch' }
    if (-not (Test-Path -LiteralPath $FilePath -PathType Leaf)) { throw 'missing executable' }
    Set-Content -LiteralPath (Join-Path $env:TRUSTD_FIXTURE 'launched') -Value $FilePath
    $fixtureProcess = [pscustomobject]@{ ExitCode = $(if ($env:TRUSTD_FIXTURE_MODE -eq 'wizard') { 9 } else { 0 }) }
    $fixtureProcess | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {}
    return $fixtureProcess
}
& $env:TRUSTD_BOOTSTRAP_SOURCE
''', encoding="utf-8")
                    result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(fixture)],
                                            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=30,
                                            env={**{key: value for key, value in os.environ.items() if key.upper() != "PSMODULEPATH"}, "TEMP": str(tmp), "TMP": str(tmp),
                                                 "TRUSTD_FIXTURE": str(base), "TRUSTD_FIXTURE_MODE": mode,
                                                 "TRUSTD_BOOTSTRAP_SOURCE": str(ROOT / "install.ps1")})
                    self.assertEqual(result.returncode == 0, mode == "success", result.stderr)
                    self.assertEqual((base / "launched").exists(), mode in ("success", "wizard"))
                    self.assertEqual(list(tmp.iterdir()), [sentinel])


if __name__ == "__main__":
    unittest.main()
