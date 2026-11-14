#requires -Version 7.0
param([switch]$Release)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Push-Location (Join-Path $PSScriptRoot '..')
try {
    # Build libraries without linking an executable. Selecting the root library
    # also reports OpenSSL's actual dependency rlib, rather than a top-level copy.
    $cargoArgs = @('build', '--locked', '--lib', '--no-default-features', '--message-format=json')
    if ($Release) { $cargoArgs += '--release' }
    $messages = @(& cargo @cargoArgs | ForEach-Object { $_ | ConvertFrom-Json })
    if ($LASTEXITCODE -ne 0) { throw "Building OpenSSL failed with exit code $LASTEXITCODE" }

    $artifact = $messages | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'openssl_sys'
    } | Select-Object -Last 1
    if (-not $artifact) { throw 'Cargo did not report the OpenSSL library artifact' }
    $build = $messages | Where-Object {
        $_.reason -eq 'build-script-executed' -and $_.package_id -eq $artifact.package_id
    } | Select-Object -Last 1
    if (-not $build) { throw 'Cargo did not report the OpenSSL build directory' }

    # Rust bundles OpenSSL's object files into an rlib, but leaves their PDB in
    # the native install directory. MSVC needs the matching PDB beside the rlib.
    # Use Cargo's artifact paths so custom target directories and layouts work.
    $pdb = Join-Path $build.out_dir 'openssl-build/install/lib/ossl_static.pdb'
    $libraries = @($artifact.filenames | Where-Object { [IO.Path]::GetExtension($_) -eq '.rlib' })
    if ($libraries.Count -eq 0) { throw 'Cargo did not report an OpenSSL rlib' }
    foreach ($library in $libraries) {
        Copy-Item -LiteralPath $pdb -Destination (Join-Path (Split-Path -Parent $library) 'ossl_static.pdb')
    }
    Write-Host "Prepared OpenSSL debug symbols for $(if ($Release) { 'release' } else { 'debug' }) linking."
} finally {
    Pop-Location
}
