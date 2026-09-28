$ErrorActionPreference = "Stop"
$r = "$env:TEMP\pv\plasmo-voice"
$out = "D:\PlasmoPumpkin\.recon\server-flow.txt"
$sb = New-Object System.Text.StringBuilder

$files = @(
  "server\common\src\main\java\su\plo\voice\server\socket\NettyUdpServer.java",
  "server\common\src\main\java\su\plo\voice\server\socket\NettyUdpServerConnection.java",
  "api\server\src\main\java\su\plo\voice\api\server\socket\UdpServer.java",
  "api\server\src\main\java\su\plo\voice\api\server\socket\UdpServerConnection.java",
  "server\common\src\main\java\su\plo\voice\server\connection\VoiceUdpServerConnectionManager.java",
  "protocol\src\main\java\su\plo\voice\proto\packets\udp\serverbound\PlayerAudioPacket.java",
  "protocol\src\main\java\su\plo\voice\proto\packets\udp\clientbound\SourceAudioPacket.java",
  "protocol\src\main\java\su\plo\voice\proto\packets\udp\clientbound\SelfAudioInfoPacket.java",
  "protocol\src\main\java\su\plo\voice\proto\packets\udp\bothbound\CustomPacket.java",
  "protocol\src\main\java\su\plo\voice\proto\packets\tcp\clientbound\ConfigPacket.java",
  "protocol\src\main\java\su\plo\voice\proto\data\audio\capture\CodecInfo.java",
  "protocol\src\main\java\su\plo\voice\proto\data\audio\capture\CaptureInfo.java"
)

foreach ($rel in $files) {
  $full = Join-Path $r $rel
  [void]$sb.AppendLine("===== $rel =====")
  if (-not (Test-Path $full)) { [void]$sb.AppendLine("(MISSING)"); [void]$sb.AppendLine(""); continue }
  foreach ($l in (Get-Content $full)) {
    $t = $l.Trim()
    if ($t.StartsWith("import ") -or $t.StartsWith("package ")) { continue }
    if ($t -eq "") { continue }
    [void]$sb.AppendLine($l.TrimEnd())
  }
  [void]$sb.AppendLine("")
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
Write-Host ("lines: " + (Get-Content $out).Count)
