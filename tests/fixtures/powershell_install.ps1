param([string]$Wrapper, [string]$Directory, [string]$Mode)
$ErrorActionPreference = 'Stop'
. $Wrapper
$env:TEMP = $Directory
$script:Mode = $Mode
$script:Expected = Join-Path $Directory 'pwsh.exe'
$script:Downloads = @()
$script:Launched = $false

# No network, elevation or real installer is used by this fixture.
function Invoke-TrustdDownload {
    param([string]$Uri, [string]$OutFile)
    if ($script:Mode -eq 'download') { throw 'fixture download failure' }
    $script:Downloads += $Uri
    if ($Uri.EndsWith('metadata.json')) {
        Set-Content -LiteralPath $OutFile -Value '{"ReleaseTag":"v99.2.3"}'
    } else { Set-Content -LiteralPath $OutFile -Value 'unsigned fixture MSI' }
}
if ($Mode -ne 'unsigned') {
    function Invoke-TrustdMsi {
        param([string]$Package)
        if (-not (Test-Path -LiteralPath $Package)) { throw 'MSI launch happened before download' }
        if ($script:Downloads.Count -ne 2) { throw 'missing download' }
        $script:Launched = $true
        if ($script:Mode -eq 'cancel') { throw 'fixture UAC cancelled' }
        if ($script:Mode -eq 'msi_failure') { return [pscustomobject]@{ ExitCode = 1603 } }
        if ($script:Mode -ne 'missing') { Set-Content -LiteralPath $script:Expected -Value 'fixture executable' }
        [pscustomobject]@{ ExitCode = $(if ($script:Mode -eq 'reboot') { 3010 } else { 0 }) }
    }
}
$installer = Join-Path $Directory 'official-fixture.ps1'
@'
param([switch]$UseMSI, [switch]$Quiet)
if (-not $UseMSI -or -not $Quiet) { throw 'expected upstream MSI selection' }
$release = (Invoke-RestMethod 'https://raw.githubusercontent.com/PowerShell/PowerShell/master/tools/metadata.json').ReleaseTag
$package = Join-Path $env:TEMP ('PowerShell-' + $release + '.msi')
Invoke-WebRequest -Uri "https://github.com/PowerShell/PowerShell/releases/download/$release/PowerShell.msi" -OutFile $package
$process = Start-Process msiexec -ArgumentList @('/i', $package, '/quiet') -Wait -PassThru
if ($process.ExitCode -ne 0) { throw 'upstream saw failed MSI' }
'@ | Set-Content -LiteralPath $installer -Encoding UTF8
try {
    Install-TrustdPowerShell -InstallerScript $installer -Downloader 'unused fixture' -ExpectedShell $script:Expected
    if ($Mode -notin @('success', 'reboot')) { throw 'unexpected installation success' }
    if ($script:Downloads[1] -notmatch '/v99.2.3/') { throw 'upstream release selection was not used' }
} catch {
    $expectedError = switch ($Mode) {
        'cancel' { 'fixture UAC cancelled' }
        'msi_failure' { 'exit code 1603' }
        'download' { 'fixture download failure' }
        'missing' { 'System PowerShell was not found' }
        'unsigned' { 'valid Microsoft signature' }
        default { '' }
    }
    if (-not $expectedError -or -not $_.Exception.Message.Contains($expectedError)) { throw }
    if ($Mode -in @('download', 'unsigned') -and $script:Launched) { throw 'installer launched before verification' }
}
