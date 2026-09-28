$ErrorActionPreference = "Stop"
$src = "$env:TEMP\pv\plasmo-voice\protocol\src\main\java\su\plo\voice\proto"
$out = "D:\PlasmoPumpkin\.recon\data2.txt"
$sb = New-Object System.Text.StringBuilder

Get-ChildItem "$src\data" -Recurse -File -Filter *.java | ForEach-Object {
  $rel = $_.FullName.Replace("$src\", "")
  [void]$sb.AppendLine("===== $rel =====")
  $lines = Get-Content $_.FullName
  $keep = $false
  $depth = 0
  foreach ($l in $lines) {
    $t = $l.Trim()
    if ($t -eq "") { continue }
    if ($t.StartsWith("*") -or $t.StartsWith("/*") -or $t.StartsWith("//")) { continue }
    if ($t.StartsWith("import ") -or $t.StartsWith("package ")) { continue }
    if ($t.StartsWith("@")) { continue }
    if ($t.StartsWith("private ") -or $t.StartsWith("protected ") -or $t.StartsWith("public final ") -or $t.StartsWith("public static ")) {
      if (-not $keep) { [void]$sb.AppendLine("  F  " + $t) }
      continue
    }
    if ($t -match "void (serialize|deserialize|read|write)\(") {
      $keep = $true; $depth = 0
      [void]$sb.AppendLine("  >> " + $t)
      $depth += ([regex]::Matches($t, "\{")).Count - ([regex]::Matches($t, "\}")).Count
      continue
    }
    if ($keep) {
      [void]$sb.AppendLine("     " + $t)
      $depth += ([regex]::Matches($t, "\{")).Count - ([regex]::Matches($t, "\}")).Count
      if ($depth -le 0) { $keep = $false }
    }
  }
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
