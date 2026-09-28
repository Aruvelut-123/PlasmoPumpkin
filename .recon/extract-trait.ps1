$ErrorActionPreference = "Stop"
$p = "$env:TEMP\pumpkin"
$out = "D:\PlasmoPumpkin\.recon\plugin-trait.txt"
$sb = New-Object System.Text.StringBuilder

function Section([string]$title) {
  [void]$sb.AppendLine("")
  [void]$sb.AppendLine("########## $title ##########")
}

Section "wit files list"
Get-ChildItem "$p\crates\pumpkin-plugin-wit" -Recurse -File -ErrorAction SilentlyContinue |
  ForEach-Object { [void]$sb.AppendLine($_.FullName.Replace("$p\","")) }

Section "lib.rs Plugin trait + register_plugin fn (lib.rs search)"
$lib = "$p\crates\pumpkin-plugin-api\src\lib.rs"
if (Test-Path $lib) {
  $lines = Get-Content $lib
  $hits = Select-String -Path $lib -Pattern "pub trait Plugin|pub fn register_plugin|fn on_load|fn metadata|fn new\("
  foreach ($h in $hits) { [void]$sb.AppendLine(("HIT line {0}: {1}" -f $h.LineNumber, $h.Line.Trim())) }
  [void]$sb.AppendLine("")
  $start = ($hits | Where-Object { $_.Line -match "pub trait Plugin" } | Select-Object -First 1)
  if ($start) {
    $s = $start.LineNumber - 1
    $e = [Math]::Min($lines.Count - 1, $s + 90)
    for ($i = $s; $i -le $e; $i++) { [void]$sb.AppendLine($lines[$i]) }
  }
} else { [void]$sb.AppendLine("MISSING lib.rs") }

Section "grep plugin-type / host / data folder in wit"
Get-ChildItem "$p\crates\pumpkin-plugin-wit" -Recurse -File -ErrorAction SilentlyContinue |
  Select-String -Pattern "plugin-type|host|data-dir|data_folder|get-data" -ErrorAction SilentlyContinue |
  Select-Object -First 40 |
  ForEach-Object { [void]$sb.AppendLine(("{0}:{1}: {2}" -f $_.Path.Replace("$p\",""), $_.LineNumber, $_.Line.Trim())) }

[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
