$ErrorActionPreference = "Stop"
$out = "D:\PlasmoPumpkin\.recon\misc.txt"
$sb = New-Object System.Text.StringBuilder

$roots = @("$env:TEMP\pv\plasmo-voice", "$env:TEMP\pv")

foreach ($root in $roots) {
  if (-not (Test-Path $root)) { continue }
  Get-ChildItem $root -Recurse -File -Filter *.java -ErrorAction SilentlyContinue | Where-Object {
    $_.Name -match "^(Pos3dSerializer|McGameProfileSerializer|PlayerIconConfig|SourceInfo|SourceType|Pos3d)\.java$"
  } | ForEach-Object {
    [void]$sb.AppendLine("===== " + $_.FullName.Replace($root, "") + " =====")
    foreach ($l in (Get-Content $_.FullName)) {
      $t = $l.Trim()
      if ($t -eq "") { continue }
      if ($t.StartsWith("*") -or $t.StartsWith("/*") -or $t.StartsWith("//")) { continue }
      if ($t.StartsWith("import ") -or $t.StartsWith("package ")) { continue }
      [void]$sb.AppendLine($l.TrimEnd())
    }
    [void]$sb.AppendLine("")
  }
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
