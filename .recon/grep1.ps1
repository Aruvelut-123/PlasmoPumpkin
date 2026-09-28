$ErrorActionPreference = "Stop"
$r = "$env:TEMP\pv\plasmo-voice"
$prefix = $r + [System.IO.Path]::DirectorySeparatorChar

function Grep-Repo([string]$pattern, [string]$sub, [int]$max) {
  Write-Host "=== $pattern (in $sub) ==="
  Get-ChildItem (Join-Path $r $sub) -Recurse -File -Filter *.java -ErrorAction SilentlyContinue |
    Select-String -Pattern $pattern |
    Select-Object -First $max |
    ForEach-Object {
      $p = $_.Path
      if ($p.StartsWith($prefix)) { $p = $p.Substring($prefix.Length) }
      Write-Host ("{0}:{1}: {2}" -f $p, $_.LineNumber, $_.Line.Trim())
    }
  Write-Host ""
}

Grep-Repo "plasmo:voice|ChannelName|channelName|CHANNEL_NAME" "" 25
Grep-Repo "sendPluginMessage|pluginMessage|sendCustomPayload" "" 25
Grep-Repo "bind\(|UdpServer|DatagramChannel|new DatagramSocket|allocator" "server" 30
