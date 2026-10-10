# Bring up a local Postgres for the database tests (D43) and print the
# connection string the tests read. Mirrors db-up.sh; change both together.
#
#   .\scripts\db-up.ps1          # start, wait until it answers, print the URL
#   .\scripts\db-up.ps1 -Quiet   # print only the URL
#
# Under -Quiet the URL is the only thing on the success stream, so
# `$env:HIVE_SANDBOX_TEST_DATABASE_URL = .\scripts\db-up.ps1 -Quiet` works.
#
# One `podman run`, no compose: the previous version picked `podman compose`
# whenever podman existed and never noticed there was no provider behind it
# on Windows (#106). The password is generated once at creation and lives only
# in the container's environment; later runs read it back from there.
param([switch]$Quiet)

$ErrorActionPreference = "Stop"
Set-Location (Split-Path $PSScriptRoot -Parent)

$Name = "hive-mind-pg-rust"
$Image = "docker.io/pgvector/pgvector:pg17"
$Port = if ($env:HIVE_SANDBOX_PG_PORT) { $env:HIVE_SANDBOX_PG_PORT } else { "55434" }
$User = "hive"
$Db = "hive_test"

function Say([string]$Text) { if (-not $Quiet) { Write-Host $Text -ForegroundColor Cyan } }

if (-not (Get-Command podman -ErrorAction SilentlyContinue)) {
    Write-Host "podman not found. The test database runs under Podman; docs/development.md installs it." -ForegroundColor Red
    exit 1
}

# `podman container exists` answers with the exit code and nothing else.
& podman container exists $Name 2>$null
if ($LASTEXITCODE -ne 0) {
    Say "==> creating $Name from $Image on 127.0.0.1:$Port"
    $bytes = New-Object byte[] 24
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $password = ([Convert]::ToBase64String($bytes) -replace '[/+=]', '').Substring(0, 24)
    & podman run --detach --name $Name `
        --publish "127.0.0.1:${Port}:5432" `
        --env "POSTGRES_USER=$User" `
        --env "POSTGRES_PASSWORD=$password" `
        --env "POSTGRES_DB=$Db" `
        $Image | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Host "podman run failed" -ForegroundColor Red; exit 1 }
} elseif ((& podman inspect $Name --format '{{.State.Status}}') -ne "running") {
    Say "==> starting $Name"
    & podman start $Name | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Host "podman start failed" -ForegroundColor Red; exit 1 }
}

$envLines = & podman inspect $Name --format '{{range .Config.Env}}{{println .}}{{end}}'
$password = ($envLines | Where-Object { $_ -like 'POSTGRES_PASSWORD=*' } | Select-Object -First 1)
if (-not $password) {
    Write-Host "$Name has no POSTGRES_PASSWORD in its environment; remove it and run again" -ForegroundColor Red
    exit 1
}
$password = $password.Substring('POSTGRES_PASSWORD='.Length)
# `podman port` rather than an inspect template: Windows PowerShell strips the
# double quotes a template key needs out of a native command's arguments.
$Port = ((& podman port $Name 5432 | Select-Object -First 1) -split ':')[-1].Trim()
if (-not $Port) { Write-Host "$Name publishes no port" -ForegroundColor Red; exit 1 }

# A listening port is not readiness: during initdb the server accepts local
# connections and then restarts. Poll with a real query, as the host would.
Say "==> waiting for Postgres to answer a query"
$deadline = (Get-Date).AddSeconds(120)
$ready = $false
while ((Get-Date) -lt $deadline) {
    $ErrorActionPreference = "Continue"
    $out = & podman exec $Name psql -U $User -d $Db -tAc 'select 1' 2>$null
    $ErrorActionPreference = "Stop"
    if ($LASTEXITCODE -eq 0 -and (($out | Out-String).Trim() -eq "1")) { $ready = $true; break }
    Start-Sleep -Milliseconds 500
}
if (-not $ready) {
    Write-Host "Postgres did not answer within 120s. Last log lines:" -ForegroundColor Red
    & podman logs --tail 30 $Name
    exit 1
}

# pgvector in template1, as on the cluster, so every database a test creates
# inherits it (an org role cannot create the extension itself).
$ErrorActionPreference = "Continue"
& podman exec $Name psql -U $User -d template1 -tAc 'create extension if not exists vector' 2>$null | Out-Null
$ErrorActionPreference = "Stop"

$url = "postgres://${User}:${password}@127.0.0.1:${Port}/${Db}?sslmode=disable"
if ($Quiet) {
    Write-Output $url
    exit 0
}
Write-Host ""
Write-Host "POSTGRES READY on 127.0.0.1:$Port ($Name)" -ForegroundColor Green
Write-Host ""
Write-Host "Point the database tests at it for this shell:"
Write-Host "  `$env:HIVE_SANDBOX_TEST_DATABASE_URL = '$url'"
Write-Host "  cargo test -p hive-db -p hive-schema -p hive-testdb"
Write-Host ""
Write-Host "Stop it when you are done (it holds ~250 MB of somebody's video otherwise):"
Write-Host "  podman stop $Name"
