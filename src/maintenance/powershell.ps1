param([string]$InstallerScript, [string]$Downloader, [string]$ExpectedShell)

# The upstream script owns release selection. Its downloads go through trustd
# in the login user's context so all requests share the existing proxy policy.
function Invoke-TrustdDownload {
    param([string]$Uri, [string]$OutFile)
    & $script:TrustdDownloader maintenance-powershell-download $Uri $OutFile
    if ($LASTEXITCODE -ne 0) { throw 'PowerShell download failed. Check network and proxy settings.' }
}

function Invoke-RestMethod {
    param([string]$Uri)
    $metadataFile = Join-Path $env:TEMP ([guid]::NewGuid().ToString() + '.json')
    try {
        Invoke-TrustdDownload $Uri $metadataFile
        Get-Content -LiteralPath $metadataFile -Raw -Encoding UTF8 | ConvertFrom-Json
    } finally { Remove-Item -LiteralPath $metadataFile -Force -ErrorAction SilentlyContinue }
}

function Invoke-WebRequest {
    param([string]$Uri, [string]$OutFile)
    Invoke-TrustdDownload $Uri $OutFile
    $script:TrustdPackage = $OutFile
}

function Invoke-TrustdMsi {
    param([string]$Package)
    $signature = Get-AuthenticodeSignature -LiteralPath $Package
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
        throw 'The PowerShell MSI does not have a valid Microsoft signature.'
    }
    $systemDirectory = [Environment]::GetFolderPath('System')
    # Downloads and signature verification finish before this UAC request.
    Microsoft.PowerShell.Management\Start-Process -FilePath (Join-Path $systemDirectory 'msiexec.exe') `
        -ArgumentList @('/i', ('"{0}"' -f $Package), '/quiet', '/norestart') `
        -Verb RunAs -WindowStyle Hidden -Wait -PassThru
}

function Start-Process {
    param([string]$FilePath, [string[]]$ArgumentList, [switch]$Wait, [switch]$PassThru)
    if ($FilePath -ne 'msiexec' -or $ArgumentList.Count -lt 2 -or
        $ArgumentList[0] -ne '/i' -or $ArgumentList[1] -ne $script:TrustdPackage -or
        [IO.Path]::GetExtension($script:TrustdPackage) -ne '.msi' -or -not $Wait -or -not $PassThru) {
        throw 'The official PowerShell installer requested an unexpected operation.'
    }
    $process = Invoke-TrustdMsi $script:TrustdPackage
    if ($process.ExitCode -notin @(0, 1641, 3010)) {
        throw "PowerShell MSI installation failed (exit code $($process.ExitCode))."
    }
    # Windows Installer can report success with a restart required. /norestart
    # prevents an automatic reboot; trustd still verifies pwsh can start now.
    [pscustomobject]@{ ExitCode = 0 }
}

function Install-TrustdPowerShell {
    param([string]$InstallerScript, [string]$Downloader, [string]$ExpectedShell)
    $ErrorActionPreference = 'Stop'
    $script:TrustdDownloader = $Downloader
    $script:TrustdPackage = $null
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new()
    # No pinned release and no MSIX fallback. Upstream selects the MSI release.
    & ([scriptblock]::Create((Get-Content -LiteralPath $InstallerScript -Raw -Encoding UTF8))) -UseMSI -Quiet
    if (-not (Test-Path -LiteralPath $ExpectedShell -PathType Leaf)) {
        throw "System PowerShell was not found at $ExpectedShell after installation."
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    try { Install-TrustdPowerShell -InstallerScript $InstallerScript -Downloader $Downloader -ExpectedShell $ExpectedShell }
    catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
}
