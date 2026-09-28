$ErrorActionPreference = "Stop"
$src = "$env:TEMP\pv\plasmo-voice\protocol\src\main\java\su\plo\voice\proto"
$out = "D:\PlasmoPumpkin\.recon\spec2.txt"
$sb = New-Object System.Text.StringBuilder

function Emit-File($f, $label) {
  [void]$sb.AppendLine("===== $label =====")
  $lines = Get-Content $f.FullName
  $keep = $false
  foreach ($l in $lines) {
    $t = $l.Trim()
    if ($t -eq "") { continue }
    if ($t.StartsWith("*") -or $t.StartsWith("/*") -or $t.StartsWith("//")) { continue }
    if ($t.StartsWith("import ") -or $t.StartsWith("package ")) { continue }
    if ($t.StartsWith("@")) { continue }
    if ($t.StartsWith("private ") -or $t.StartsWith("protected ")) {
      [void]$sb.AppendLine("  F  " + $t); continue
    }
    if ($t.Contains("void serialize(") -or $t.Contains("void deserialize(") -or $t.Contains("void read(") -or $t.Contains("void write(")) {
      $keep = $true
      [void]$sb.AppendLine("  >> " + $t); continue
    }
    if ($keep) {
      [void]$sb.AppendLine("     " + $t)
      if ($t -eq "}") { $keep = $false }
    }
  }
}

$dirs = @("data", "packets\tcp\clientbound", "packets\tcp\serverbound", "packets\udp\clientbound", "packets\udp\serverbound", "packets\udp\bothbound", "packets")
foreach ($d in $dirs) {
  $full = Join-Path $src $d
  if (-not (Test-Path $full)) { continue }
  Get-ChildItem $full -File -Filter *.java | Where-Object { $_.Name -match "Packet\.java" -and $_.Name -notmatch "Handler" } | ForEach-Object {
    $rel = $_.FullName.Replace("$src\", "")
    Emit-File $_ $rel
  }
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
