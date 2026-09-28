$ErrorActionPreference = "Stop"
$p = "$env:TEMP\pumpkin"
$out = "D:\PlasmoPumpkin\.recon\plugin-api.txt"
$sb = New-Object System.Text.StringBuilder

function Section([string]$title) {
  [void]$sb.AppendLine("")
  [void]$sb.AppendLine("########## $title ##########")
}

Section "pumpkin-plugin-api tree"
Get-ChildItem "$p\crates\pumpkin-plugin-api" -Recurse -File -ErrorAction SilentlyContinue |
  ForEach-Object { [void]$sb.AppendLine($_.FullName.Replace("$p\","")) }

Section "example plugins / dirs named example"
Get-ChildItem $p -Recurse -Directory -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match "^(example|examples|test-plugin|plugins)$" } |
  ForEach-Object { [void]$sb.AppendLine($_.FullName.Replace("$p\","")) }

Section "register_plugin macro definition"
$macros = Get-ChildItem "$p\crates" -Recurse -File -Filter *.rs -ErrorAction SilentlyContinue |
  Select-String -Pattern "macro_rules! register_plugin" -ErrorAction SilentlyContinue
if ($macros) {
  foreach ($m in $macros) {
    [void]$sb.AppendLine("FILE: " + $m.Path.Replace("$p\","") + " line " + $m.LineNumber)
    $lines = Get-Content $m.Path
    $start = [Math]::Max(0, $m.LineNumber - 1)
    $end = [Math]::Min($lines.Count - 1, $m.LineNumber + 160)
    for ($i = $start; $i -le $end; $i++) { [void]$sb.AppendLine($lines[$i]) }
  }
} else { [void]$sb.AppendLine("NOT FOUND") }

Section "plugin-api Cargo.toml"
$c = "$p\crates\pumpkin-plugin-api\Cargo.toml"
if (Test-Path $c) { Get-Content $c | ForEach-Object { [void]$sb.AppendLine($_) } } else { [void]$sb.AppendLine("MISSING") }

Section "workspace.package version"
Get-Content "$p\Cargo.toml" | Select-String -Pattern "\[workspace.package\]" -Context 0,20 |
  ForEach-Object { [void]$sb.AppendLine($_.Line); $_.Context.PostContext | ForEach-Object { [void]$sb.AppendLine($_) } }

[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
