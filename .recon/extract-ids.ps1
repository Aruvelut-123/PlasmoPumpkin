$ErrorActionPreference = "Stop"
$r = "$env:TEMP\pv\plasmo-voice"
$prefix = $r + [System.IO.Path]::DirectorySeparatorChar
$out = "D:\PlasmoPumpkin\.recon\packetids.txt"
$sb = New-Object System.Text.StringBuilder

Get-ChildItem $r -Recurse -File -Filter *.java | Select-String -Pattern "\.register\(" | ForEach-Object {
  $line = $_.Line.Trim()
  if ($line -match "register\s*\(\s*[0-9]") {
    $rel = $_.Path
    if ($rel.StartsWith($prefix)) { $rel = $rel.Substring($prefix.Length) }
    [void]$sb.AppendLine("$rel`:$($_.LineNumber): $line")
  }
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("matches: " + (Get-Content $out).Count)
Get-Content $out
