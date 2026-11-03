# Install or update using the same maintenance wizard. Compatible with Windows PowerShell 5.1.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $trustdArchitecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($trustdArchitecture -ne 'AMD64') { throw 'trustd currently provides a Windows x86_64 package only.' }

    $trustdTempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $trustdTemp = Join-Path $trustdTempBase ('trustd-bootstrap-' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $trustdTemp
    try {
        $trustdRelease = Invoke-RestMethod -Uri 'https://api.github.com/repos/hatsune-miku/trustd/releases/latest' -Headers @{ Accept = 'application/vnd.github+json' }
        $trustdTag = [string]$trustdRelease.tag_name
        if ($trustdTag -notmatch '^v[0-9A-Za-z._-]+$') { throw 'The latest release has an invalid version tag.' }
        $trustdBaseUrl = 'https://github.com/hatsune-miku/trustd/releases/download/' + $trustdTag
        $trustdArchiveName = 'trustd-windows-x86_64.zip'
        $trustdArchive = Join-Path $trustdTemp $trustdArchiveName
        $trustdChecksums = Join-Path $trustdTemp 'SHA256SUMS'
        Write-Host ('Downloading trustd ' + $trustdTag + '...')
        Invoke-WebRequest -UseBasicParsing -Uri ($trustdBaseUrl + '/' + $trustdArchiveName) -OutFile $trustdArchive
        Invoke-WebRequest -UseBasicParsing -Uri ($trustdBaseUrl + '/SHA256SUMS') -OutFile $trustdChecksums
        $trustdMatches = @(Get-Content -LiteralPath $trustdChecksums | Where-Object { $_ -match '^([0-9a-fA-F]{64})\s+\*?trustd-windows-x86_64\.zip$' })
        if ($trustdMatches.Count -ne 1) { throw 'The release checksum is missing or ambiguous.' }
        $trustdExpected = ($trustdMatches[0] -split '\s+')[0]
        if ((Get-FileHash -LiteralPath $trustdArchive -Algorithm SHA256).Hash -ne $trustdExpected) { throw 'Release checksum verification failed.' }
        Expand-Archive -LiteralPath $trustdArchive -DestinationPath $trustdTemp
        $trustdBundle = Join-Path $trustdTemp 'trustd-windows-x86_64'
        foreach ($trustdRole in @('trustd', 'trustd-system', 'trustd-user', 'trustd-stdio')) {
            if (-not (Test-Path -LiteralPath (Join-Path $trustdBundle ($trustdRole + '.exe')) -PathType Leaf)) { throw 'The release package is incomplete.' }
        }
        Write-Host 'Opening the maintenance wizard. Select Finish in the wizard to remove the temporary download.'
        $trustdWizard = Start-Process -FilePath (Join-Path $trustdBundle 'trustd.exe') -ArgumentList 'maintenance' -PassThru -WindowStyle Hidden
        $trustdWizard.WaitForExit()
        if ($trustdWizard.ExitCode -ne 0) { throw ('The maintenance wizard exited with code ' + $trustdWizard.ExitCode + '.') }
    } finally {
        # Remove only the unique directory created above; never an inferred installation path.
        $trustdResolved = [IO.Path]::GetFullPath($trustdTemp)
        $trustdParent = [IO.Path]::GetDirectoryName($trustdResolved)
        if ($trustdParent.TrimEnd('\', '/') -ne $trustdTempBase.TrimEnd('\', '/')) { throw 'Refusing to clean an unexpected temporary path.' }
        if (Test-Path -LiteralPath $trustdResolved) {
            $trustdItem = Get-Item -LiteralPath $trustdResolved -Force
            if ($trustdItem.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing to clean a redirected temporary directory.' }
            Remove-Item -LiteralPath $trustdResolved -Recurse -Force
        }
    }
}
