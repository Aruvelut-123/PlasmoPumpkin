$ErrorActionPreference = "Stop"
$p = "$env:TEMP\pumpkin"
$out = "D:\PlasmoPumpkin\.recon\udp-capability.txt"
$sb = New-Object System.Text.StringBuilder

function Section([string]$t) { [void]$sb.AppendLine(""); [void]$sb.AppendLine("########## $t ##########") }

Section "plugin.wit (the world)"
$f = "$p\crates\pumpkin-plugin-wit\v0.1\plugin.wit"
if (Test-Path $f) { Get-Content $f | ForEach-Object { [void]$sb.AppendLine($_) } } else { [void]$sb.AppendLine("MISSING") }

Section "any wit mentioning sockets/network imports"
$hits = Get-ChildItem "$p\crates\pumpkin-plugin-wit\v0.1" -File -Filter *.wit |
  Select-String -Pattern "wasi|socket|network" -CaseSensitive:$false
foreach ($h in $hits) { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $h.Filename, $h.LineNumber, $h.Line.Trim())) }
if (-not $hits) { [void]$sb.AppendLine("(no matches)") }

Section "plugin-runtime Cargo.toml"
$c = "$p\crates\pumpkin-plugin-runtime\Cargo.toml"
if (Test-Path $c) { Get-Content $c | ForEach-Object { [void]$sb.AppendLine($_) } } else { [void]$sb.AppendLine("MISSING") }

Section "runtime rs files"
Get-ChildItem "$p\crates\pumpkin-plugin-runtime" -Recurse -File -Filter *.rs |
  ForEach-Object { [void]$sb.AppendLine($_.FullName.Replace("$p\","")) }

Section "runtime: wasi / socket / network config"
$h2 = Get-ChildItem "$p\crates\pumpkin-plugin-runtime" -Recurse -File -Filter *.rs |
  Select-String -Pattern "inherit_network|allow_tcp|allow_udp|WasiCtx|WasiCtxBuilder|wasi" -CaseSensitive:$false
foreach ($h in $h2) { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $h.Path.Replace("$p\",""), $h.LineNumber, $h.Line.Trim())) }
if (-not $h2) { [void]$sb.AppendLine("(no matches)") }

Section "host-bindings crate: socket/udp/network"
$hb = "$p\crates\pumpkin-host-bindings"
if (Test-Path $hb) {
  $h3 = Get-ChildItem $hb -Recurse -File | Select-String -Pattern "udp|socket|network" -CaseSensitive:$false
  foreach ($h in $h3) { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $h.Path.Replace("$p\",""), $h.LineNumber, $h.Line.Trim())) }
  if (-not $h3) { [void]$sb.AppendLine("(no matches)") }
} else { [void]$sb.AppendLine("MISSING crate") }

[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
