$ErrorActionPreference = "Stop"
$f = "$env:TEMP\pv\plasmo-voice\protocol\src\main\java\su\plo\voice\proto\packets\tcp\PacketTcpCodec.java"
$out = "D:\PlasmoPumpkin\.recon\tcp-ids.txt"
$c = Get-Content $f
$sb = New-Object System.Text.StringBuilder
$i = 0
$id = 0
foreach ($l in $c) {
  $i++
  if ($l -match "PACKETS\.register\((\+\+lastPacketId|0x[0-9a-fA-F]+),\s*PacketDirection\.(\w+),\s*(\w+)\.class") {
    $tok = $Matches[1]; $dir = $Matches[2]; $cls = $Matches[3]
    if ($tok -eq "++lastPacketId") { $id++ } else { $id = [Convert]::ToInt32($tok, 16) }
    [void]$sb.AppendLine(("{0,3} (0x{0:X2})  {1,-6}  {2}" -f $id, $dir, $cls))
  }
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host "=== TCP packet IDs ==="
Get-Content $out
Write-Host ""
Write-Host "=== UDP packet IDs ==="
$fu = "$env:TEMP\pv\plasmo-voice\protocol\src\main\java\su\plo\voice\proto\packets\udp\PacketUdpCodec.java"
$id = 0
foreach ($l in (Get-Content $fu)) {
  if ($l -match "PACKETS\.register\((\+\+lastPacketId|0x[0-9a-fA-F]+),\s*PacketDirection\.(\w+),\s*(\w+)\.class") {
    $tok = $Matches[1]; $dir = $Matches[2]; $cls = $Matches[3]
    if ($tok -eq "++lastPacketId") { $id++ } else { $id = [Convert]::ToInt32($tok, 16) }
    Write-Host ("{0,3} (0x{0:X2})  {1,-6}  {2}" -f $id, $dir, $cls)
  }
}
