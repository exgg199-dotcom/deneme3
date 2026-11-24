# Download CI artifacts and refresh the latest local copy.
param(
  [string]$Branch = (git rev-parse --abbrev-ref HEAD),
  [string[]]$Workflows = @("ios"),
  [string]$Repo = "Kelly-Inc/horizon-ios"
)
$ErrorActionPreference = "Stop"
Set-Location (git rev-parse --show-toplevel)
New-Item -ItemType Directory -Force ci-artifacts\latest | Out-Null

function Test-NonemptyIpa($Root, $Artifact) {
  $path = Join-Path (Join-Path $Root $Artifact) 'Horizon-game-unsigned.ipa'
  $file = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
  return ($null -ne $file -and -not $file.PSIsContainer -and $file.Length -gt 0)
}

function Test-ArtifactCache($Root, $Artifact) {
  if (-not (Test-Path -LiteralPath (Join-Path $Root '.complete') -PathType Leaf)) { return $false }
  return (-not $Artifact -or (Test-NonemptyIpa $Root $Artifact))
}

$missing = $false
foreach ($wf in $Workflows) {
  $runs = gh run list -R $Repo --workflow "$wf.yml" --branch $Branch --status success --limit 1 --json databaseId,attempt
  if ($LASTEXITCODE -ne 0) { throw "Could not list $wf runs for $Branch" }
  $run = ConvertFrom-Json -InputObject ($runs -join "`n") | Select-Object -First 1
  if (-not $run) {
    [Console]::Error.WriteLine("${wf}: no successful run on $Branch")
    $missing = $true
    continue
  }
  $id = $run.databaseId
  $cache = "$id.$($run.attempt)"
  $artifact = $null
  if ($wf -eq "ios") {
    $response = gh api --paginate --slurp "repos/$Repo/actions/runs/$id/artifacts?per_page=100"
    if ($LASTEXITCODE -ne 0) { throw "Could not list artifacts for run $id" }
    $selected = ($response -join "`n" | ConvertFrom-Json).artifacts |
      Where-Object { -not $_.expired -and $_.name -match '^horizon-ios-game(-[1-9][0-9]*)?$' } |
      Sort-Object -Property id | Select-Object -Last 1
    if (-not $selected) {
      [Console]::Error.WriteLine("ios: no available IPA artifact for run $id")
      $missing = $true
      continue
    }
    $artifact = $selected.name
    $cache = "$id.$($selected.id)"
  }
  $dir = "ci-artifacts\$cache"
  if (-not (Test-ArtifactCache $dir $artifact)) {
    $stage = "ci-artifacts\.download-$cache.$([guid]::NewGuid())"
    try {
      if ($artifact) {
        gh run download $id -R $Repo -D (Join-Path $stage $artifact) --name $artifact
      } else {
        gh run download $id -R $Repo -D $stage
      }
      if ($LASTEXITCODE -ne 0) { throw "Could not download artifacts from run $id" }
      if ($artifact -and -not (Test-NonemptyIpa $stage $artifact)) {
        throw "ios: artifact $artifact has no nonempty Horizon-game-unsigned.ipa"
      }
      Get-ChildItem -LiteralPath $stage -Recurse -File -Filter *.tar.gz | ForEach-Object {
        tar -xzf $_.FullName -C $_.DirectoryName
        if ($LASTEXITCODE -ne 0) { throw "Could not extract $($_.FullName)" }
      }
      New-Item -ItemType File -Path (Join-Path $stage '.complete') | Out-Null
      if (-not (Test-ArtifactCache $dir $artifact)) {
        if (Test-Path -LiteralPath $dir) {
          Move-Item -LiteralPath $dir -Destination "ci-artifacts\.unverified-$cache.$([guid]::NewGuid())"
        }
        try {
          [System.IO.Directory]::Move((Convert-Path -LiteralPath $stage), (Join-Path (Get-Location).Path $dir))
        } catch {
          if (-not (Test-ArtifactCache $dir $artifact)) { throw }
        }
      }
    } finally {
      if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    }
  }
  $latest = Join-Path (Convert-Path ci-artifacts/latest) $wf
  $next = "$latest.$([guid]::NewGuid())"
  $prior = "$next.old"
  try {
    Copy-Item -LiteralPath $dir -Destination $next -Recurse
    if (Test-Path $latest) { [IO.Directory]::Move($latest, $prior) }
    try { [IO.Directory]::Move($next, $latest) }
    catch {
      if (Test-Path $prior) { [IO.Directory]::Move($prior, $latest) }
      throw
    }
  } finally {
    if (Test-Path $next) { Remove-Item $next -Recurse -Force }
  }
  if (Test-Path $prior) { Remove-Item $prior -Recurse -Force }
  Write-Host "${wf}: run $id -> $latest"
}
if ($missing) { exit 1 }
