# Bumps the app version everywhere it lives and opens a CHANGELOG entry.
#   scripts\bump-version.ps1            -> patch bump (4.1.0 -> 4.1.1)
#   scripts\bump-version.ps1 minor      -> 4.1.0 -> 4.2.0
#   scripts\bump-version.ps1 5.0.0      -> explicit version
# [workspace.package].version in the root Cargo.toml is the source of truth (Tauri and both crates read it);
# Cargo.lock is kept in sync so CI's --locked builds pass.
param([string]$Part = "patch")
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$cargoPath = Join-Path $root "Cargo.toml"
$cargo = [IO.File]::ReadAllText($cargoPath)
$current = [regex]::Match($cargo, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
$v = [version]($current -replace '-.*$', '')

switch -regex ($Part) {
    '^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$' { $new = $Part }
    '^major$' { $new = "$($v.Major + 1).0.0" }
    '^minor$' { $new = "$($v.Major).$($v.Minor + 1).0" }
    default   { $new = "$($v.Major).$($v.Minor).$($v.Build + 1)" }
}

# note: a Regex *instance* is needed for "replace only the first match" ([regex]::Replace has no count)
$cargo = ([regex]'(?m)^(version\s*=\s*")[^"]+(")').Replace($cargo, "`${1}$new`${2}", 1)
[IO.File]::WriteAllText($cargoPath, $cargo)

# Cargo.lock records our own crates' versions too
$lockPath = Join-Path $root "Cargo.lock"
if (Test-Path $lockPath) {
    $lock = [IO.File]::ReadAllText($lockPath)
    foreach ($crate in "tsukimi", "tsukimi-core") {
        $lock = ([regex]"(name = `"$crate`"\r?\nversion = `")[^`"]+(`")").Replace($lock, "`${1}$new`${2}", 1)
    }
    [IO.File]::WriteAllText($lockPath, $lock)
}

$clPath = Join-Path $root "CHANGELOG.md"
$cl = [IO.File]::ReadAllText($clPath)
$date = Get-Date -Format "yyyy-MM-dd"
if ($cl -match '(?m)^## \[Unreleased\]') {
    # notes collected under [Unreleased] become this version; a fresh empty [Unreleased] goes on top
    $cl = ([regex]'(?m)^## \[Unreleased\][^\r\n]*').Replace($cl, "## [Unreleased]`n`n## [$new] - $date", 1)
} else {
    $entry = "## [$new] - $date`n### Changed`n- `n`n"
    $cl = ([regex]'## \[').Replace($cl, "$entry## [", 1)
}
[IO.File]::WriteAllText($clPath, $cl)

Write-Host "Version $current -> $new (Cargo.toml, Cargo.lock, CHANGELOG.md)"
Write-Host "Check the CHANGELOG.md entry, commit (git commit -am `"Release v$new`"), then push main (scripts\release.ps1, or GitHub Desktop) - the Release workflow tags and publishes"
