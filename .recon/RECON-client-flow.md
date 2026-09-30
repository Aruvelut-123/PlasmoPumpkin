# RECON — Plasmo Voice **client-side** flow

Checkout (authoritative): `C:\Users\Baymaxawa\AppData\Local\Temp\pv\plasmo-voice`, commit `c0fcec7`
("chore(api): update abi"). Every `path:line` below is relative to that root and was read from the
real file. Goal: everything a from-scratch Rust server must know so a **real, unmodified client**
can both hear and transmit.

---

## 0. Fixed transport facts

* Mod channel name: `plasmo:voice/v2` (`server/common/src/main/java/su/plo/voice/BaseVoiceServer.java:77`),
  flag channel `plasmo:voice/v2/installed` (`:78`), service channel `plasmo:voice/v2/service` (`:79`).
  All TCP control packets travel as raw byte arrays in that plugin-message channel; the channel
  itself carries the length, the PV payload is `typeByte || body`
  (`protocol/.../tcp/PacketTcpCodec.java:83-95`).
* The receiver for that channel is registered unconditionally
  (`client/.../ModVoiceClient.java:239-242`) and **the client never sends anything until the
  server sends something first** (§3 lists every send).
* Receiving any packet on the channel lazily creates the per-server connection object and an RSA
  key pair (`client/.../connection/ModClientChannelHandler.java:81-96`, key pair at `:86`), then
  decodes with `PacketDirection.CLIENT` (`:102-109`).
* TCP packet ids (`protocol/.../tcp/PacketTcpCodec.java:46-81`; registry semantics in
  `PacketRegistry.java:31-48`, `PacketDirection.kt:8-9`): 1 Connection, 2 PlayerInfoRequest,
  3 Config, 4 ConfigPlayerInfo, 5 LanguageRequest (serverbound), 6 Language, 7 PlayerList,
  8 PlayerInfoUpdate, 9 PlayerDisconnect, 10 PlayerInfo, 11 PlayerState, 12 PlayerAudioEnd,
  13 PlayerActivationDistances, 14 DistanceVisualize, 15 SourceInfoRequest, 16 SourceInfo,
  17 SelfSourceInfo, 18 SourceAudioEnd, 19 ActivationRegister, 20 ActivationUnregister,
  21..25 SourceLine{Register,Unregister,PlayerAdd,PlayerRemove,PlayersList}, 26 AnimatedActionBar.
* UDP framing (`protocol/.../udp/PacketUdpCodec.java:45-59,78-91`):
  `u32 magic = 0x4e9004e9` (`:24`), `u8 type`, `16-byte secret` (raw two longs,
  `PacketUtil.java:44-51`), `i64 timestamp`, then body. UDP ids (`:27-35`): 1 Ping (ANY),
  2 PlayerAudio (serverbound), 3 SourceAudio (clientbound), 4 SelfAudioInfo (clientbound),
  0x100 Custom (ANY; unusable — `writeByte` truncates it to 0x00 and 0x00 is not registered,
  see `AGENTS.md` §4). The client's decoder is built with `PacketDirection.CLIENT`
  (`client/.../socket/NettyUdpClient.java:78`), so it accepts only 1, 3, 4.
* The client's UDP socket is a Netty **connected** `NioDatagramChannel`
  (`NettyUdpClient.java:73-88`): it only accepts datagrams from the exact `ip:port` it connected
  to, and it always sends to that peer (`:131`). A server must answer from the same address it is
  pinged on.

---

## 1. Whole-session order (upstream reference behaviour)

```
server → client : PlayerInfoRequest        PaperVoiceServer.kt:29 / MinestomVoiceServer.kt:23
client → server : PlayerInfo               ModServerConnection.java:352-361
server → client : ConnectionPacket         PlayerChannelHandler.java:103 → VoiceTcpServerConnectionManager.java:75-79
client          : UDP socket connect()     ModServerConnection.java:246-250
client → server : UDP Ping(serverIp,port)  every 1 s while !connected; NettyUdpClientHandler.java:111-117
server          : first datagram w/ known secret → create UDP connection; NettyPacketHandler.java:47-66
server → client : UDP Ping                 keep-alive, first within ~100 ms; NettyUdpKeepAlive.java:40-53
client          : marks itself "connected" NettyUdpClient.java:150-161
server → client : ConfigPacket, PlayerListPacket, PlayerInfoUpdate; NettyPacketHandler.java:68-71
client → server : PlayerActivationDistances (one packet **per activation**), then LanguageRequest;
                  ModServerConnection.java:314-320, :349
client → server : UDP PlayerAudio          once an activation is active and UDP is connected
server → client : TCP SourceInfo (first time) + UDP SourceAudio (relay)
```

Two orderings are guaranteed upstream: (1) `PlayerInfo` precedes `ConnectionPacket`, because the
secret is only revealed after the client's public key arrives (`PlayerChannelHandler.java:87-104`);
(2) `ConfigPacket` comes only **after** the server has seen a UDP datagram, i.e. after the client's
UDP client exists — sent earlier it is silently dropped (§4).

---

## 2. On receiving a clientbound `ConnectionPacket`

`ConnectionPacket` = `UUID secret`, `UTF ip`, `i32 port`
(`protocol/.../tcp/clientbound/ConnectionPacket.java:30,37`).
Handler: `client/.../connection/ModServerConnection.java:228-251`:

1. `voiceClient.getUdpClientManager().removeClient(RECONNECT)` — tears down any previous UDP
   client and fires `UdpClientClosedEvent(RECONNECT)` (`:230`; manager at
   `VoiceUdpClientManager.java:19-23`). That event closes the whole server connection
   (`ModServerConnection.java:499-502`), so a reconnect is a full reset.
2. `new NettyUdpClient(voiceClient, config, packet.getSecret())` (`:232`). The constructor builds
   the handler and **starts a 1 Hz ticker immediately** (`NettyUdpClientHandler.java:45-50`).
   The first tick usually runs before `connect()` and is a no-op because there is no remote
   address yet (`:112`).
3. `UdpClientConnectEvent` is fired (`:234-236`); cancelling it aborts. The event's client is
   used (`:238`).
4. `udpClientManager.setClient(client)` + register for events (`:240-241`).
5. IP substitution: `if (ip.equals("0.0.0.0")) ip = getRemoteIp();` (`:243-244`). `getRemoteIp()`
   returns the Minecraft TCP peer host (`:138-152`, falling back to `127.0.0.1` for local
   addresses). So a server may advertise `0.0.0.0` and the client will dial the address it
   already connected to with the advertised port.
6. `client.connect(ip, port)` (`:246-250`) → Netty bootstrap connect (`NettyUdpClient.java:85-88`).

**No packet is sent in response to `ConnectionPacket`.** The first thing the client emits is the
UDP ping from the ticker.

### What the client sends first, over UDP

`NettyUdpClientHandler.tick()` (`:111-126`), every 1 000 ms:

```java
if (!client.getRemoteAddress().isPresent()) return;
if (!client.isConnected()) {                       // <- "connected" == a Ping was received
    client.sendPacket(new PingPacket(remoteAddress.getHostString(), remoteAddress.getPort()));
}
long diff = now - client.getKeepAlive();
if (diff > 30_000) close(TIMED_OUT); else if (diff > 7_000) setTimedOut(true);
```

`PingPacket` is `i64 time` followed by an **optional** `UTF serverIp(≤255)` + `u16 serverPort` with
**no flag byte** — the reader discovers their absence by catching the end-of-buffer exception
(`protocol/.../udp/bothbound/PingPacket.java:33-51`). The client's startup ping includes both
fields, which is what upstream uses to learn the public address
(`NettyPacketHandler.java:60-65`: `connection.setConnectionAddress(...)`), while its keep-alive
reply is `i64 time` only (`PingPacket()` leaves `serverIp == null`, `:48`).
`client.isConnected()` is the Netty flag only (`NettyUdpClient.java:145-148`) and is *not*
affected by `timedOut`, so after a soft timeout the client does **not** resume pinging.

---

## 3. Control-plane packets the client sends, in order, and what gates them

`ModServerConnection.sendPacket(packet, checkUdpConnection)` (`:108-136`) silently drops unless:
* the Minecraft TCP connection exists (`:110-111`), and
* `udpClientManager.isConnected()` if `checkUdpConnection == true` (`:113-114`)
  → `client.isConnected() && !client.isTimedOut()` (`VoiceUdpClientManager.java:30-34`).

Exact list (all sends under `client/src/main`):

| # | Packet | Trigger | `checkUdp` | Citation |
|---|--------|---------|-----------|----------|
| 1 | `PlayerInfoPacket` | server sent `PlayerInfoRequestPacket` | false | `ModServerConnection.java:352-361` |
| 2 | UDP `PingPacket(serverIp, port)` | every tick while `!connected` | n/a | `NettyUdpClientHandler.java:114-117` |
| 3 | `PlayerActivationDistancesPacket` × N | each activation registered while handling `ConfigPacket`; also on distance hotkeys | **false** | `ModServerConnection.java:314-320` → `VoiceClientActivationManager.java:109-116`; `VoiceClientActivation.java:353-361` |
| 4 | `LanguageRequestPacket` | end of `ConfigPacket` handling; and on client language change | false / true | `ModServerConnection.java:349`; `:504-507` |
| 5 | `PlayerStatePacket` | mic-mute or voice-disable toggled | **true** | `HotkeyActions.java:26,32,69-77` |
| 6 | `SourceInfoRequestPacket` | source unknown when audio arrives, or state mismatch; rate-limited to 1/s per source | true | `VoiceClientSourceManager.kt:162-170`; `NettyUdpClientHandler.java:89-91`; limit `VoiceClientSourceManager.kt:70-73` |
| 7 | `PlayerAudioEndPacket` | activation ended | true | `VoiceAudioCapture.java:415-428` |
| 8 | UDP `PlayerAudioPacket` | activation activated | n/a | `VoiceAudioCapture.java:397-413` |
| 9 | UDP `PingPacket()` (no ip/port) | reply to every inbound ping | n/a | `NettyUdpClientHandler.java:69-73` |

Wire shapes (for the Rust side; `read` = what the server receives, `write` = what it emits):
`PlayerInfoPacket` = `bool voiceDisabled, bool microphoneMuted, UTF mcVersion(≤64), UTF version(≤64),
i32 len(1..2048) + pubkey` (`PlayerInfoPacket.java:39,51`); `PlayerActivationDistancesPacket` =
`i32 count, (UUID activationId, i32 distance)*` (`PlayerActivationDistancesPacket.java:28,37`) — the
client sends **one packet per activation with exactly one entry**
(`VoiceClientActivationManager.java:112-114`) and iterates `ConfigPacket.activations` (a `HashSet`),
so per-activation order is unspecified; `LanguageRequestPacket` = `UTF language(≤32)`
(`LanguageRequestPacket.java:25,30`); `PlayerStatePacket` = `bool voiceDisabled, bool
microphoneMuted` (`PlayerStatePacket.java:24,30`); `SourceInfoRequestPacket` = `UUID sourceId`
(`SourceInfoRequestPacket.java:26,31`); `PlayerAudioEndPacket` = `i64 seq, UUID activationId,
i16 distance` (`PlayerAudioEndPacket.java:30,37`); `PlayerAudioPacket` = `i64 seq, i32
len(1..2048)+data, UUID activationId, i16 distance, bool stereo` (`BaseAudioPacket.java:33,43`,
`PlayerAudioPacket.java:37,46`).

**Preconditions for transmitting** (`VoiceAudioCapture.run()`, `:196-210`):

```java
if (!device.isPresent() || !device.get().isOpen()
    || !udpClientManager.isConnected()          // <- needs an inbound Ping from the server
    || !serverInfo.isPresent()                  // <- needs ConfigPacket
    || !activations.getParentActivation().isPresent()) { sleep(1000); continue; }
```

plus a per-activation "activated" result (`:255-304, 337-370`). Defaults on a fresh client:
activation type = `PUSH_TO_TALK` (`ConfigClientActivation.java:14-18`) and the proximity PTT key =
**Left Alt** (`ConfigHotkeys.java:39-45`), so out of the box the client only speaks while LALT is
held. Muting (`microphoneDisabled`) or disabling voice also flush/stop transmission
(`VoiceAudioCapture.java:229-253`).

**Preconditions for playing** audio: `serverInfo` present + a registered source line matching the
source's `lineId` + (optional) decoder (`BaseClientAudioSource.kt:114-144`).

---

## 4. Clientbound packet-by-packet

Handler entry: `ModServerConnection.handle(packet)` (`:215-226`) fires `TcpClientPacketReceivedEvent`
(cancellable) and wraps `packet.handle(this)` in try/catch — a throw is only logged, so a
half-applied handler leaves the client in a stale state.

### `ConnectionPacket` (id 1) — REQUIRED to start anything
§2. Without it the client never opens a UDP socket and never talks.

### `PlayerInfoRequestPacket` (id 2) — the normal conversation starter
Body is empty (`PlayerInfoRequestPacket.java:23,27`). The client replies with `PlayerInfoPacket`
(`ModServerConnection.java:352-361`) using the generated RSA-2048 public key
(`:204-213`, `ModClientChannelHandler.java:86`). Upstream requires this to learn the public key
before it will hand out a secret (`PlayerChannelHandler.java:87-104`). A Rust server that does
not use encryption may skip it and send `ConnectionPacket` unprompted — the client accepts any
first packet.
*If missing*: nothing breaks for an unencrypted server; with encryption on, `ConfigPacket`
cannot be decrypted.

### `ConfigPacket` (id 3) — REQUIRED; the single most important packet
Handler `ModServerConnection.java:253-350`. Steps in order, with failure modes:

1. `udpClientManager.getClient()` absent → **warn + return, packet dropped** (`:255-259`).
   Same for a client without a remote address (`:261-265`). This is why the server must send
   `ConfigPacket` only after seeing the client's first UDP datagram.
2. Encryption (`:267-285`): if `packet.encryption != null`, the client RSA-**decrypts**
   `EncryptionInfo.data` with its private key and creates the cipher via `EncryptionManager`. On
   failure it calls `removeClient(DISCONNECT)` (`:282`) → `onUdpClosed` → `close()` → the whole
   connection is reset. `EncryptionInfo` = `UTF algorithm, i32 len(1..2048)+data`
   (`EncryptionInfo.java:28,38`).
3. `new VoiceServerInfo(...)` (`:287-294`). The constructor dereferences
   `config.getPlayerIconConfig()` **unconditionally** (`VoiceServerInfo.java:89-95`). See §8 trap.
4. `voiceClient.setServerInfo(serverInfo)` (`:296`) — this is the flag the capture loop and every
   audio source check.
5. Per-server client config entry created if absent (`:298-304`); `serverId` is the key
   (`VoiceServerInfo.java:42-43`). The activation-distance entries live under it
   (`VoiceClientActivationManager.java:185-191` `getServerConfig()` throws
   `"Server config is empty"` if step 5 did not run).
6. Register every `sourceLines` entry (`:306-312`).
7. Register every `activations` entry (`:314-320`) → **each one immediately sends a
   `PlayerActivationDistancesPacket`** (`VoiceClientActivationManager.java:109-116`).
8. `audioCapture.start()` then `audioCapture.initialize(serverInfo)` (`:323-325`) — opens the input
   device and builds Opus encoders `iff captureInfo.encoderInfo != null`
   (`VoiceAudioCapture.java:137-142, 121-135`).
9. Opens the OpenAL output device with `AudioFormat(sampleRate, 16, 1, signed, little)`
   (`:328-341`), starts the device job (`:343`), fires `ServerInfoInitializedEvent` (`:345-346`),
   then sends `LanguageRequestPacket` (`:349`).

ConfigPacket wire order (`ConfigPacket.java:69-109` read / `:110-133` write):
`UUID serverId` → `CaptureInfo{ i32 sampleRate, i32 mtuSize, bool hasEncoderInfo,
[CodecInfo{ UTF name, i32 n, (UTF key, UTF value)* }] }` (`CaptureInfo.java:27,37`,
`CodecInfo.java:29,40`) → `bool hasEncryption, [EncryptionInfo]` → `i32 lineCount` then per line
`UTF name, UTF translation, UTF icon, f64 defaultVolume, i32 weight, bool hasPlayers,
[i32 n, n*(UUID, UTF name, i32 propCount, (UTF,UTF,UTF)*)]` (`VoiceSourceLine.java:68,85`,
`McGameProfileSerializer.kt:13,33`) → `i32 activationCount` then per activation
`UTF name, UTF translation, UTF icon, i32 distanceCount + i32*, i32 defaultDistance,
bool proximity, bool transitive, bool stereoSupported, bool hasEncoderInfo, [CodecInfo],
i32 weight` (`VoiceActivation.java:104,122`) → `ConfigPlayerInfoPacket` body: `i32 n,
(UTF permission, bool)*` (`ConfigPlayerInfoPacket.java:30,41`) → **PlayerIconConfig trailer**:
`i32 n, UTF enumName*`, `f64 x, f64 y, f64 z` (`PlayerIconConfig.kt:41,53`, `Pos3dSerializer.kt:11,19`).
Neither the line id nor the activation id is on the wire: both are derived from `name`
(`VoiceSourceLine.java:70` `generateId(name)`, `VoiceActivation.java:108`
`VoiceActivation.generateId(name)`), so name and id must agree with what the client sends back.

*If missing*: no `ServerInfo` → the capture loop parks forever (`VoiceAudioCapture.java:202-210`),
`BaseClientAudioSource` cannot even be constructed (`IllegalStateException("Not connected")`,
`BaseClientAudioSource.kt:115-116`), and `sendSourceInfoRequest` throws
(`VoiceClientSourceManager.kt:165-166`). The client becomes a ping-only client.

### `ConfigPlayerInfoPacket` (id 4) — optional
Updates the permission map (`ModServerConnection.java:368-375`). Only `pv.allow_freecam` is read
by the client (`BaseClientAudioSource.kt:436-442`; key list `Permissions.kt:20`).
*If missing*: freecam sound-listener permission defaults to `true`
(`BaseClientAudioSource.kt:441` `.orElse(true)`). No audio impact.

### `PlayerListPacket` (id 7) — optional
Fills `playerById` (`:377-381`). *If missing*: `getPlayers()`/`getLocalPlayer()` are empty
(`:154-168`), so `isServerMuted()` always returns false (`VoiceAudioCapture.java:187-194`) and
voice icons/overlay lack state. Player positions do **not** come from this map — the client looks
players up in the Minecraft world by UUID (`ClientPlayerSource.kt:70-73`). Audio still works.

### `PlayerInfoUpdatePacket` (id 8) — optional (recommended)
`playerById.put(...)` + `VoicePlayerConnectedEvent` / `VoicePlayerUpdateEvent`
(`:383-390`). Same consequences as `PlayerListPacket`; this is how the server tells the client
that a player is server-muted (`muted`), has voice disabled, or is mic-muted
(`VoicePlayerInfo.java:30,39`). Upstream broadcasts it for the joining player right after the UDP
connection opens (`NettyPacketHandler.java:71`).

### `PlayerDisconnectPacket` (id 9)
If the id is the local player → `removeClient(DISCONNECT)` and return, i.e. full teardown
(`:392-400`). Otherwise removed from the map + `VoicePlayerDisconnectedEvent` (`:402-403`).
The server's UDP timeout path also broadcasts this (`NettyUdpServerConnection.java:108-113`;
timeout `NettyUdpKeepAlive.java:45-48`).

### `SourceInfoPacket` (id 16) — REQUIRED to hear anything
Ignored while voice is disabled in the client config (`:419-420`). Then, for a
`PlayerSourceInfo`, the embedded `VoicePlayerInfo` is put into `playerById` (`:422-425`), and
`sources.createOrUpdateSource(sourceInfo)` creates or updates the source
(`:427`; `VoiceClientSourceManager.kt:110-160`).
Source creation constructs `BaseClientAudioSource`, whose `init` **throws** if there is no
`ServerInfo` (`BaseClientAudioSource.kt:115-116`) or if `sourceInfo.lineId` is not a line the
client registered (`:131-132` `"Source line not found"`). The throw is swallowed by the outer
try/catch (`ModServerConnection.java:220-225`), leaving no source.
Wire order (`SourceInfo.java:25-30,52` + `PlayerSourceInfo.java:40`):
`UTF typeName("PLAYER"), UTF addonId, UUID id, bool hasName + UTF name, i8 state,
bool hasDecoderInfo + [CodecInfo], bool stereo, UUID lineId, bool iconVisible, i32 angle,
VoicePlayerInfo{ UUID playerId, UTF nick, bool muted, bool voiceDisabled, bool microphoneMuted }`.

### `SelfSourceInfoPacket` (id 17, TCP) — optional
`updateSelfSourceInfo` (`:430-433` → `VoiceClientSourceManager.kt:172-182`) records the client's own
source (and its `sequenceNumber`) and re-creates that source if it exists locally; used for the
overlay of one's own activation (`OverlayRenderer.kt:76`). Not needed to hear or transmit.

### `ActivationRegisterPacket` (id 19) / `ActivationUnregisterPacket` (id 20) — optional
Register is ignored when no `ServerInfo` yet (`:466-469`), then delegates to
`ClientActivationManager.register` which also sends a `PlayerActivationDistancesPacket`
(`VoiceClientActivationManager.java:83-123`). Unregister removes it (`:474-477`); unregistering
the proximity activation re-installs the silent fallback parent
(`VoiceClientActivationManager.java:148-163, 193-204`).
The normal path is to declare activations inside `ConfigPacket` instead.

### `LanguagePacket` (id 6) — optional, cosmetic
Stores the translation map (`:363-366`), used as a fallback language supplier
(`BaseVoiceClient.java:225-227`).

### `SourceAudioEndPacket` (id 18, TCP) — optional but nice
`UUID sourceId, i64 seq` (`SourceAudioEndPacket.java:29,35`). If the source exists it is fed to
the jitter buffer, otherwise buffered as a pending packet
(`ModServerConnection.java:406-416`; `VoiceClientSourceManager.kt:215-224`).
Effect: the last frame is faded out and the source is reset
(`BaseClientAudioSource.kt:220-229, 398-401, 490-491`). Without it, the client simply times the
source out after `closeTimeoutMs = 500 ms` of silence
(`BaseClientAudioSource.kt:88, 277-284`).

### `SourceLine*` (21-25) / `DistanceVisualizePacket` (14) / `AnimatedActionBarPacket` (26) — optional
Register/unregister lines (`:435-443`) and maintain per-line player sets (`:445-464`); lines needed
by audio sources normally arrive in `ConfigPacket`, these exist for dynamic addon lines.
`DistanceVisualize`/`AnimatedActionBar` are cosmetic (`ModServerConnection.java:479-497`).

---

## 5. Keep-alive, timeouts, and what "connected" means

Constants: `MAX_KEEP_ALIVE_TIMEOUT = 30_000`, `MAX_SOFT_KEEP_ALIVE_TIMEOUT = 7_000`
(`NettyUdpClientHandler.java:30-31`). Tick every 1 s (`:45-50`), logic at `:111-126`.

* `keepAlive` is refreshed **only** by an inbound `PingPacket` (`:69-73`). Audio
  (`SourceAudioPacket`) and `SelfAudioInfoPacket` do **not** refresh it.
* On the first inbound ping, `NettyUdpClient.setKeepAlive(now)` sets `timedOut = false`,
  `connected = true`, and fires `UdpClientConnectedEvent` (`NettyUdpClient.java:150-161`).
  **So the server must send `PingPacket`s; nothing else makes the client "connected".**
* 7 s without an inbound ping → `setTimedOut(true)` (`:123-124`) → `UdpClientManager.isConnected()`
  becomes false (`VoiceUdpClientManager.java:30-34`), so the client stops sending every packet
  that uses the UDP check **and stops transmitting audio** (`VoiceAudioCapture.java:204`) — but
  the socket stays open and it keeps replying to pings.
* 30 s without an inbound ping → `close(TIMED_OUT)` (`:120-122`) → `UdpClientClosedEvent` →
  `ModServerConnection.close()` (`:499-502, 181-202`) which drops `ServerInfo`, clears players,
  stops capture, clears sources/lines/activations and stops the device manager.
  Only a new `ConnectionPacket` revives the client.
* The client answers **every** inbound ping with `new PingPacket()` (`:72` — `i64 time` only,
  `PingPacket.java:45-51`), keeping upstream's `lastReceivedPacketTimestamp` fresh
  (`NettyUdpServerConnection.java:98-118`). Never echo the client's ping back — a real client
  replies to *any* ping, so echoing would ping-pong without bound; ping on your own schedule
  (`NettyUdpKeepAlive.java:49-53`, next ping 1.5–3.0 s later, timeout `voice.keepAliveTimeoutMs`
  = 15 000, `VoiceServerConfig.java:152`).
* Client-side "connected" is therefore: **received ≥1 UDP packet and it was a Ping**, plus
  `!timedOut`.

---

## 6. Receiving audio: `SourceAudioPacket` / `SelfAudioInfoPacket`

`SourceAudioPacket` = `i64 seq, i32 len(1..2048)+payload, UUID sourceId, i8 sourceState,
i16 distance` (`SourceAudioPacket.java:39,48`, `BaseAudioPacket.java:33,43`).

`NettyUdpClientHandler.handle(SourceAudioPacket)` (`:80-96`):
1. Drop if the client config has voice disabled (`:82`).
2. `sourceManager.getSourceById(sourceId)` — with `request = true`, so an **unknown** source
   triggers a rate-limited `SourceInfoRequestPacket` (`:84-85`;
   `VoiceClientSourceManager.kt:64-76`, min 1 000 ms between requests per source id, `:71-73`).
3. Unknown source → `bufferPacket` (max 8 packets per source, buffers expire after 5 000 ms)
   (`:93-95`; `VoiceClientSourceManager.kt:202-213, 226-231, 258`; `PendingSourceBuffer.kt:9,17-31`).
   When the `SourceInfoPacket` finally arrives the buffer is drained into the new source
   (`VoiceClientSourceManager.kt:125, 155`).
4. Known source but `source.getSourceInfo().getState() != packet.getSourceState()`
   → send `SourceInfoRequestPacket(sourceId, requestIfExist = true)` (`:89-91`), then still
   `source.process(packet)`.
5. `source.process` → `StaticJitterBuffer.offer` (`BaseClientAudioSource.kt:210-218`).

Playback rules that constrain the server (`BaseClientAudioSource.kt`):
* `isAudioPacketValid`: **drop if `abs(sourceInfo.state - packet.sourceState) >= 10`** (`:300-309`),
  and drop non-increasing sequence numbers unless the backward jump is ≥ 10
  (`SEQUENCE_RESTART_THRESHOLD`, `:311-326, 627`).
* The jitter buffer schedules one 20 ms frame per sequence step
  (`StaticJitterBuffer.kt:19, 146-147`), drops frames that are ≥ 500 ms late (`:13, 77-80`),
  caps the queue at 100 (`:14, 37-44`), re-anchors after a ≥ 200 ms gap (`:17, 52-59`), and
  synthesises PLC for holes (`:84-110`, `BaseClientAudioSource.kt:329-355`).
* Payload: `decryption?.decrypt(data) ?: data`, then `decoder?.decode(x) ?:
  AudioUtil.bytesToShorts(x)` (`BaseClientAudioSource.kt:373-374`). **`decoderInfo == null` in the
  `SourceInfoPacket` means raw little-endian 16-bit PCM** (no Opus).
* Stereo is taken from `sourceInfo.isStereo`, **not** from the audio packet
  (`BaseClientAudioSource.kt:376-380, 612-614`; `PlayerAudioPacket.stereo` only goes the other way).
* `distance` (the `i16` in the packet) is used for gain and for `canHear`: volume scales with
  `1 - pos/ distance` (`:456-462, 497-506, 546`) and `canHear` is set only when `distance > 0`
  (`:571`). So the relayed `distance` must be the intended audible radius, non-zero.
* Sources time out after 500 ms of silence (`:88, 277-284`), so a continuous talker needs a
  continuous frame stream (20 ms cadence).

`SelfAudioInfoPacket` = `UUID sourceId, i64 sequenceNumber, bool hasData + [i32 len + data],
i16 distance` (`SelfAudioInfoPacket.java:39,52`). Handler `NettyUdpClientHandler.java:98-109`
only stores `sequenceNumber`/`distance` on the client's own `VoiceClientSelfSourceInfo`
(`VoiceClientSelfSourceInfo.java:25-33`). It is informational (overlay/self source); voice can be
heard and sent without it.

TCP `SourceAudioEndPacket` semantics — see §4.

---

## 7. MINIMUM viable server for a real client

The smallest set that yields **both** directions:

**A. Channel + hello**
1. Register/declare channel `plasmo:voice/v2`.
2. Either (a) send `PlayerInfoRequestPacket` and read the client's `PlayerInfoPacket`, or (b)
   skip it and mint a secret immediately. (b) works as long as `ConfigPacket.encryption == null`.

**B. `ConnectionPacket{ secret, ip, port }`**
* One secret per player (16 raw bytes), `ip` may be `0.0.0.0`. Port must be the UDP port the
  client will dial.

**C. Answer the first UDP datagram**
On the first datagram whose secret is known, remember the sender address and send, in this order
(matches `NettyPacketHandler.java:53-71`):
1. `ConfigPacket` — **must contain** (see §4 for the exact field order):
   * `serverId` — any stable UUID (per session is fine; it keys client-side config).
   * `CaptureInfo`: `sampleRate = 48000`, `mtuSize = 1024`
     (`VoiceServerConfig.java:145,159`), `encoderInfo = CodecInfo("opus", ...)` or `null`
     for raw PCM. With `null` the client sends/expects raw PCM (§4/§6) — simplest, larger, still
     valid.
   * `encryption = null`.
   * `sourceLines`: at least the line that will be referenced by every `PlayerSourceInfo`. Use the
     proximity line: `name = "proximity"` (id = `UUID.nameUUIDFromBytes("proximity_line")`,
     `VoiceSourceLine.java:30-35`), `translation = "pv.activation.proximity"`, `icon` any string
     (upstream uses `plasmovoice:textures/icons/speaker.png`,
     `ProximityServerActivation.kt:39-45`), `defaultVolume = 1.0`, `weight = 1`,
     `hasPlayers = false`.
   * `activations`: **must include** an activation named `proximity`
     (id = `UUID.nameUUIDFromBytes("proximity_activation")`, `VoiceActivation.java:29-34`),
     `translation != "pv.activation.parent"` (use `"pv.activation.proximity"`),
     `distances = [8,16,32]`, `defaultDistance = 16` (`VoiceServerConfig.java:254,257`),
     `proximity = true`, `transitive = true`, `stereoSupported = false`,
     `encoderInfo = null` (inherit capture codec), `weight = 1`
     (mirrors `ProximityServerActivation.kt:23-37`).
     Without it the client installs a fallback parent activation
     (`VoiceClientActivationManager.java:118-120, 193-215`) whose translation is
     `"pv.activation.parent"`, and `sendVoicePacket`/`sendVoiceEndPacket` **return immediately**
     for that translation (`VoiceAudioCapture.java:400, 416`) → the client can hear but never
     speaks.
   * `permissions`: any map (e.g. empty, or `pv.allow_freecam -> true`).
   * `playerIconConfig`: `0` entries + `(0.0, 0.0, 0.0)` — **required trailer**, see §8.
2. A server `PingPacket()` immediately, then at least every ~5 s (soft timeout is 7 s, hard 30 s).
3. Optionally `PlayerListPacket` + `PlayerInfoUpdatePacket(self)` (`NettyPacketHandler.java:69-71`).

**D. Ignore/handle the client's follow-up** (needed only for correct behaviour, not to make
audio flow):
* `PlayerActivationDistancesPacket` → store per activation, use it for relay range (upstream:
  `PlayerChannelHandler.java:128-137`).
* `LanguageRequestPacket` → optionally answer `LanguagePacket` (cosmetic).
* `PlayerStatePacket` → track disabled/muted, stop relaying that player.

**E. To make the client *hear* a speaker**
1. On UDP `PlayerAudioPacket{seq, data, activationId, distance, stereo}` from speaker S: verify
   the sender's connection, then create (once) a stable `PlayerSourceInfo` for S with:
   `type = "PLAYER"`, `addonId` any string, `id` = a stable random UUID per speaker
   (upstream uses `UUID.randomUUID()` per player+line, `VoiceServerPlayerSource.kt:22`),
   `name = null`, `state = 1` initial (`BaseServerAudioSource.java:40`), `decoderInfo` = what you
   will send (Opus, or `null` for raw PCM), `stereo = packet.stereo`,
   `lineId = proximity line id`, `iconVisible = true`, `angle = 0`,
   `playerInfo = {uuid, nick, muted=false, voiceDisabled=false, microphoneMuted=false}`.
2. Send that `SourceInfoPacket` to every listener that has not received it yet (upstream sends it
   when the source is "dirty", i.e. before the first audio packet,
   `VoiceServerProximitySource.kt:50-54`), **and answer every `SourceInfoRequestPacket`** with it
   (`PlayerChannelHandler.java:148-173`). Without a `SourceInfoPacket` the listener only buffers
   8 packets / 5 s and then discards them.
3. Relay as `SourceAudioPacket`: same `sourceId`, `sourceState = source.state` (the value you last
   put in `SourceInfoPacket`; bump it in steps < 10 or the listener drops everything, §6),
   `sequenceNumber = packet.sequenceNumber`, `distance = packet.distance` **clamped to the
   activation's allowed distances** (`VoiceServerActivationManager.kt:132`,
   `Activation.java:157-184`), payload = `packet.data` unchanged (no encryption) — the client
   filters listeners by distance itself only for volume; **the server chooses the listener set**
   (upstream: within `min(distance + maxExtraAudioBroadcastDistance, distance*2)` blocks,
   `VoiceServerProximitySource.kt:85-107`).
4. On `PlayerAudioEndPacket` optionally send `SourceAudioEndPacket{sourceId, seq}`
   (`ProximityServerActivationHelper.kt:99-111`); otherwise the 500 ms source timeout handles it.
5. Send the server's keep-alive `PingPacket` continuously, otherwise nothing above matters.

**F. To make the client *transmit*** — nothing beyond A–C, plus the LALT push-to-talk default.
The client emits `PlayerAudioPacket`s as soon as `ServerInfo` exists, the UDP client is
"connected" (a server ping was received), the input device is open and the proximity activation
is the non-fallback one. A mature server should not blindly relay: it must check that the
`activationId` is a registered activation, that the sender is not muted, and clamp the distance
(upstream `VoiceServerActivationManager.kt:121-169`).

**Not required** for the minimum: `PlayerInfo*` packets, `ActivationRegister/Unregister`,
`SourceLine*`, `SelfSourceInfo`, `LanguagePacket`, `ConfigPlayerInfo`, `DistanceVisualize`,
`AnimatedActionBar`, `CustomPacket`, encryption.

---

## 8. Traps that silently kill voice

1. **`ConfigPacket` before the first UDP datagram** is dropped with
   `"Config packet is received before UDP is connected"` (`ModServerConnection.java:255-265`).
   Order: receive datagram → then send Config.
2. **Missing `playerIconConfig` trailer** → `playerIconConfig` deserialization is wrapped in
   `try { } catch (Exception ignored) {}` (`ConfigPacket.java:100-107`), leaving it `null`, and
   `VoiceServerInfo` immediately NPEs on it (`VoiceServerInfo.java:89-90`). The NPE happens
   *before* `setServerInfo` (`ModServerConnection.java:287-296`), so the whole `ConfigPacket`
   is void. Always write `i32 0` + three `f64` zeros at the end.
3. **No `proximity` activation in `ConfigPacket`** → the fallback parent has translation
   `"pv.activation.parent"` and every outgoing audio frame is discarded
   (`VoiceAudioCapture.java:400, 416`). This is the classic "client hears but is never heard".
4. **`SourceInfo.lineId` not registered** → `IllegalStateException("Source line not found")`
   (`BaseClientAudioSource.kt:131-132`), swallowed at `ModServerConnection.java:220-225`, no
   source, no audio. The line must be present in `ConfigPacket.sourceLines` (or sent via
   `SourceLineRegisterPacket`).
5. **`sourceState` mismatch** → each packet with `|state diff| >= 10` is dropped
   (`BaseClientAudioSource.kt:302`) *and* triggers a `SourceInfoRequestPacket` (rate-limited to
   1/s). Keep `SourceInfoPacket.state` and every `SourceAudioPacket.sourceState` in sync; only
   change state in increments < 10 (upstream uses +1 for name/icon and +10 for stereo,
   `BaseServerAudioSource.java:64-90`).
6. **`distance <= 0`** → `canHear` is never set (`BaseClientAudioSource.kt:571`) and gain math
   collapses; always relay a plausible distance (8/16/32).
7. **Audio alone does not keep the client alive** — only `PingPacket` does
   (`NettyUdpClientHandler.java:69-73`). Ping < 7 s.
8. **Wrong peer address**: the client's UDP socket is `connect()`ed, so datagrams from another
   source address (or a different advertised IP than the one the client dialled, unless the
   packet said `0.0.0.0`) are dropped by the OS/Netty. Send from the port the client pinged.
9. **`SourceInfo` in the same tick as the first audio is not required** — but a listener that
   receives audio for an unknown source buffers only 8 packets / 5 s
   (`VoiceClientSourceManager.kt:202-232, 258`), so answering `SourceInfoRequestPacket` promptly
   is what makes the first syllables audible.
10. **`PlayerDisconnectPacket` for the local player tears down the entire client connection**
    (`ModServerConnection.java:392-400`); never echo it to the player it names by mistake.
11. `ConfigPacket.captureInfo.sampleRate` drives the OpenAL output format and the encoder
    (`ModServerConnection.java:328-334`, `VoiceAudioCapture.java:122-124, 138-142`). The UDP
    `timestamp` is never validated by the client (`PacketUdp.java` carries the field,
    `NettyUdpClientHandler` never reads it), but the secret must match `NettyUdpClient.secret`
    byte-for-byte (`NettyUdpClient.java:41, 124`).
