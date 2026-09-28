$ErrorActionPreference = "Stop"
$src = "$env:TEMP\pv\plasmo-voice\protocol\src\main\java\su\plo\voice\proto"
$out = "D:\PlasmoPumpkin\.recon\serializers.txt"
$sb = New-Object System.Text.StringBuilder

Get-ChildItem $src -Recurse -File -Filter *.java | Where-Object { $_.Name -match "Serializer" } | ForEach-Object {
  $rel = $_.FullName.Substring($src.Length + 1)
  [void]$sb.AppendLine("===== $rel =====")
  $lines = Get-Content $_.FullName
  foreach ($l in $lines) {
    $t = $l.Trim()
    if ($t -eq "") { continue }
    if ($t.StartsWith("*") -or $t.StartsWith("/*") -or $t.StartsWith("//")) { continue }
    if ($t.StartsWith("import ") -or $t.StartsWith("package ")) { continue }
    if ($t.StartsWith("@")) { continue }
    [void]$sb.AppendLine($l.TrimEnd())
  }
  [void]$sb.AppendLine("")
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
