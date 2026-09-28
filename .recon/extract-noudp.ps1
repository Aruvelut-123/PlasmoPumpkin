$ErrorActionPreference = "Stop"
$p = "$env:TEMP\pumpkin"
$out = "D:\PlasmoPumpkin\.recon\no-udp-proof.txt"
$sb = New-Object System.Text.StringBuilder

function Section([string]$t) { [void]$sb.AppendLine(""); [void]$sb.AppendLine("########## $t ##########") }

Section "repo-wide grep: udp (case-insensitive), *.rs and *.toml and *.wit"
$h = Get-ChildItem $p -Recurse -File -Include *.rs,*.toml,*.wit -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -notmatch "\\target\\" } |
  Select-String -Pattern "udp" -CaseSensitive:$false
foreach ($x in $h) { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $x.Path.Replace("$p\",""), $x.LineNumber, $x.Line.Trim())) }
if (-not $h) { [void]$sb.AppendLine("(NO MATCHES ANYWHERE)") }

Section "repo-wide grep: wasmtime_wasi / WasiCtx / add_to_linker"
$h2 = Get-ChildItem $p -Recurse -File -Include *.rs,*.toml -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -notmatch "\\target\\" } |
  Select-String -Pattern "wasmtime_wasi|WasiCtx|add_to_linker|inherit_network|allow_tcp"
foreach ($x in $h2) { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $x.Path.Replace("$p\",""), $x.LineNumber, $x.Line.Trim())) }
if (-not $h2) { [void]$sb.AppendLine("(NO MATCHES ANYWHERE)") }

Section "repo-wide grep: permission"
$h3 = Get-ChildItem $p -Recurse -File -Include *.rs,*.toml,*.wit,*.json -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -notmatch "\\target\\" } |
  Select-String -Pattern "permission" -CaseSensitive:$false
foreach ($x in $h3) { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $x.Path.Replace("$p\",""), $x.LineNumber, $x.Line.Trim())) }
if (-not $h3) { [void]$sb.AppendLine("(NO MATCHES ANYWHERE)") }

Section "pumpkin plugin dir contents"
Get-ChildItem "$p\crates\pumpkin\src\plugin" -Recurse -File -ErrorAction SilentlyContinue |
  ForEach-Object { [void]$sb.AppendLine($_.FullName.Replace("$p\","")) }

Section "metadata.wit (plugin manifest)"
$mf = "$p\crates\pumpkin-plugin-wit\v0.1\metadata.wit"
if (Test-Path $mf) { Get-Content $mf | ForEach-Object { [void]$sb.AppendLine($_) } } else { [void]$sb.AppendLine("MISSING") }

[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
