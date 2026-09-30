# Plasmo Voice — server-side TCP control plane (recon for a Rust reimplementation)

Reference checkout: `C:\Users\Baymaxawa\AppData\Local\Temp\pv\plasmo-voice` @ `c0fcec7`.
All `path:line` citations are relative to that checkout root. Every claim below was read from the
file named; nothing is inferred from a summary. Java is under `src/main/java`, Kotlin under
`src/main/kotlin` — **several load-bearing files are Kotlin** (`PlayerInfoRequestScheduler`,
`ModRequiredKickHandler`, `VoiceServerActivationManager`, source-line classes), so a pure-Java
grep misses the control plane entirely.

---

## 0. Channel, framing, packet ids

**Channels** — `server/common/.../BaseVoiceServer.java:77-79`:
`CHANNEL_STRING = "plasmo:voice/v2"`, `FLAG_CHANNEL_STRING = "plasmo:voice/v2/installed"`,
`SERVICE_CHANNEL_STRING = "plasmo:voice/v2/service"`. Handlers are registered in
`onInitialize` (`BaseVoiceServer.java:138-140`): `CHANNEL_STRING -> ServerChannelHandler`,
`SERVICE_CHANNEL_STRING -> ServerServiceChannelHandler`. `FLAG_CHANNEL_STRING` is *not* registered
server-side; the client registers it as a flag channel
(`client/src/main/java/su/plo/voice/server/ModVoiceServer.java:22`).

**Framing of one voice packet (`PacketTcpCodec.encode`, `protocol/.../packets/tcp/PacketTcpCodec.java:83-95`):**

```
byte[] = [ 1 byte packet id ][ packet body ... ]        // NO length prefix
```

`encode` writes exactly `out.writeByte(type)` then `packet.write(out)` and returns `out.toByteArray()`
(`:87-94`). The Minecraft plugin-message / `ByteArrayPayload` envelope around those bytes is supplied
by the platform layer, not by this codec: server `instance.sendPacket(CHANNEL_STRING, encoded)`
(`VoiceServerPlayerEntity.java:37`), client `ByteArrayPayload` + `ClientPlayNetworking`
(`client/.../ModServerConnection.java:116-124`). On receive the server feeds the raw payload straight
into `PacketTcpCodec.decode(ByteStreams.newDataInput(bytes), PacketDirection.SERVER)`
(`ServerChannelHandler.java:37`) — the first byte of the plugin-message payload *is* the packet id.
`su.plo.slib` is external and **not** in this checkout, so the outer envelope's exact byte layout could
not be verified here (see §8).

**Decode/direction rules** (`PacketTcpCodec.java:101-112`, `PacketRegistry.java:31-48`,
`PacketDirection.kt:8-9`): `PACKETS.byType(buf.readByte(), direction)` returns `null` when the id is
unregistered **or** when the packet's registered direction does not accept the requested direction
(`ANY` accepts everything; a `SERVER`-registered id is refused for `CLIENT` and vice versa). A `null`
packet makes `decode` return `Optional.empty()`; `ServerChannelHandler.receive` then does nothing.
Any `Throwable` during decode is swallowed and debug-logged (`ServerChannelHandler.java:48-50`).

**Packet ids** — assigned by an incrementing counter in registration order
(`PacketTcpCodec.java:47-81`). `C` = CLIENT (clientbound), `S` = SERVER (serverbound):

```
 1 ConnectionPacket (C)              10 PlayerInfoPacket (S)                 19 ActivationRegisterPacket (C)
 2 PlayerInfoRequestPacket (C)       11 PlayerStatePacket (S)                20 ActivationUnregisterPacket (C)
 3 ConfigPacket (C)                  12 PlayerAudioEndPacket (S)             21 SourceLineRegisterPacket (C)
 4 ConfigPlayerInfoPacket (C)        13 PlayerActivationDistancesPacket (S)  22 SourceLineUnregisterPacket (C)
 5 LanguageRequestPacket (S)         14 DistanceVisualizePacket (C)          23 SourceLinePlayerAddPacket (C)
 6 LanguagePacket (C)                15 SourceInfoRequestPacket (S)          24 SourceLinePlayerRemovePacket (C)
 7 PlayerListPacket (C)              16 SourceInfoPacket (C)                 25 SourceLinePlayersListPacket (C)
 8 PlayerInfoUpdatePacket (C)        17 SelfSourceInfoPacket (C)             26 AnimatedActionBarPacket (C)
 9 PlayerDisconnectPacket (C)        18 SourceAudioEndPacket (C)
```

Scalar encoding everywhere: Guava `ByteArrayDataOutput` — big-endian; `writeUTF` = Java modified UTF-8
with a `u16` length prefix; `writeUUID` = two big-endian `long`s (16 bytes) (`PacketUtil.java:44-51`,
`writeUUID`/`readUUID`). `writeInt` = 4 bytes; counts written by producers are *re-validated on read*
by `PacketUtil.readSafeInt(in, min, max)` / `readSafeUTF(in, maxLen)` (`PacketUtil.java:17-31`). The
per-field read limits matter and are listed below; they are part of the protocol.

> **Checkout artefact (not upstream):** `protocol/src/main/java/su/plo/voice/proto/packets/PacketTcpCodec.java`
> is an **untracked** file (`git ls-files` does not know it) byte-identical (SHA-256 equal) to
> `protocol/.../packets/tcp/PacketTcpCodec.java`. It declares `package su.plo.voice.proto.packets.tcp;`
> from the wrong directory and is a duplicate class for the `protocol` source set. Delete it before
> compiling; do not treat it as a second codec.

---

## 1. What is sent to a player, in what order, when they join / finish connecting

Three distinct phases. Only phase 3 is the "burst".

### Phase 1 — Minecraft join: the server asks for player info

Trigger: `McPlayerJoinEvent` → `PlayerInfoRequestScheduler.handlePlayerJoin`
(`server/common/src/main/kotlin/.../connection/PlayerInfoRequestScheduler.kt:39,56-69`).

Guard `udpServer.isPresent && config != null`, else nothing happens at all (`:57`); `:62` records
`pendingPlayers[uuid] = PlayerRequestState(voicePlayer, now, 0)`; `:64-68` hops to the background
executor and then the Minecraft main thread and calls
`voiceServer.tcpPacketManager.requestPlayerInfo(voicePlayer)`, which sends **id 2
`PlayerInfoRequestPacket`, body empty** (`VoiceTcpServerConnectionManager.java:85-94`,
`PlayerInfoRequestPacket.java:22-28`).

Retries, while the player has no public key (`:20-27,31-36,75-101`): a 500 ms tick resends after
`1_000, 3_000, 5_000, 10_000, 15_000` ms; at most 5 resends; the entry is dropped as soon as
`player.publicKey.isPresent` or any TCP packet arrives (`:51-54`), or on quit (`:71-73`).
Concurrently `ModRequiredKickHandler` (same package, `:39-55`) schedules a kick after
`voice.config.voice.clientModRequiredCheckTimeoutMs` (default `3_000`, `VoiceServerConfig.java:165`)
when `clientModRequired` is true and the player lacks `pv.bypass_mod_requirement` (`:61-62`); any
inbound TCP packet cancels it (`:26-29`).

Also sent at join time, before the UDP handshake, by the platforms: on plugin/server start for players
that already registered the channel (`server/paper/.../PaperVoiceServer.kt:25-32`,
`server/minestom/.../MinestomVoiceServer.kt:23`), and on UDP-server restart for previously connected
players (`BaseVoiceServer.java:351-353`).

### Phase 2 — client answers with `PlayerInfoPacket`; server replies `ConnectionPacket`

Trigger: serverbound id 10 → `PlayerChannelHandler.handle(PlayerInfoPacket)`
(`server/common/.../connection/PlayerChannelHandler.java:67-104`). Call order:

1. `:68-69` parse `voiceServer.getVersion()` and `packet.getVersion()` as `SemanticVersion`.
2. `:71-74` **major-version gate**: if `clientVersion.major() != serverVersion.major()` →
   `ServerVersionUtil.suggestSupportedVersion(player, packet.getMinecraftVersion())` (a chat message,
   `:18-31` of that file) and **return**. No `ConnectionPacket`.
3. `:76-85` min-version gate: `minVersion = SemanticVersion.parse(config.voice().clientModMinVersion())`
   (default `"2.0.0"`, `VoiceServerConfig.java:168`), falling back to `"2.0.0"` if unparsable; if
   `clientVersion.asInt() < minVersion.asInt()` → suggest version and return.
4. `:89-97` `KeyFactory.getInstance("RSA")` + `X509EncodedKeySpec(packet.getPublicKey())` →
   `voicePlayer.setPublicKey(...)`; on failure log and return (no reply).
5. `:99-101` `setVoiceDisabled(packet.isVoiceDisabled())`, `setMicrophoneMuted(packet.isMicrophoneMuted())`,
   `setModVersion(packet.getVersion())`.
6. `:103` `tcpConnections.connect(player)` → `VoiceTcpServerConnectionManager.connect` (`:53-82`):
   guard `udpServer.isPresent && config != null` (`:54`);
   `secret = udpConnectionManager.getSecretByPlayerId(uuid)` (`:56-57`) — minted lazily and **sticky**
   for the lifetime of `secretByPlayerId` (`VoiceUdpServerConnectionManager.java:50-60`);
   `ip` = `config.host().public.ip()` if a `[host.public]` block exists, else `config.host().ip()`
   (`:59-63`); `port` = `public.port()` if non-zero, else `host.port()`, else if that is 0 the UDP
   socket's actual bound port (falls back again to `host.port()` at `:65-73`);
   sends **id 1 `ConnectionPacket{secret, ip, port}`** (`:75-79`; body `ConnectionPacket.java:36-44`
   = UUID + UTF `ip` + int `port`).

### Phase 3 — the UDP registration burst

Trigger: the player's **first datagram whose secret is unknown but resolvable to a player** —
`NettyPacketHandler.channelRead0` (`server/common/.../socket/NettyPacketHandler.java:25-72`).

1. `:28-45` if a connection already exists for the secret, optionally `setRemoteAddress(sender)`
   (address following) and handle the packet, then return.
2. `:47-51` `getPlayerIdBySecret(secret)` then `getPlayerById(playerId)`; either absent → return
   silently (an unknown secret is dropped, never registered).
3. `:53-59` build `NettyUdpServerConnection(voiceServer, ctx.channel(), secret, player)` and
   `connection.setRemoteAddress(datagram.sender())`.
4. `:60-65` if the packet is a `PingPacket` with non-null `serverIp`, set
   `connectionAddress = unresolved(serverIp, serverPort)`.
5. `:66` `udpConnectionManager.addConnection(connection)` — fires `UdpClientConnectEvent`
   (cancellable, `VoiceUdpServerConnectionManager.java:63-66`), inserts both indexes, disconnects any
   previous connection for the same secret/player (`:67-71`), logs, fires `UdpClientConnectedEvent`
   (`:74`). `BaseVoicePlayerManager.onClientConnect` (`server-proxy-common/.../BaseVoicePlayerManager.java:79-83`)
   handles that event at `EventPriority.HIGHEST` and sets `connected = true` — which is what
   `VoicePlayer.hasVoiceChat()` returns (`BaseVoicePlayer.java:63-66`).
6. `:68-71` **the burst, in this exact order, to that player only (except c):**
   a. `sendConfigInfo(player)` → **id 3 `ConfigPacket`** (`VoiceTcpServerConnectionManager.java:97-155`)
   b. `sendPlayerList(player)` → **id 7 `PlayerListPacket`** (`:157-168`)
   c. `broadcastPlayerInfoUpdate(player)` → **id 8 `PlayerInfoUpdatePacket`**, broadcast to every
      player with `hasVoiceChat()`, filtered by visibility (`:170-175`, `:184-186`)

Because step 5 runs before step 6, the newly connected player already has `hasVoiceChat() == true`
and therefore **receives its own `PlayerInfoUpdatePacket`** in 6c.

`sendConfigInfo` sends **nothing at all** if (a) the UDP server/config is missing, or (b) the RSA
encryption step throws — in that case it logs and `return`s before constructing the packet
(`:98`, `:109-125`). There is no "ConfigPacket with null encryption" fallback path in practice:
`connect()` in phase 2 already required a public key. A key that parses but fails to encrypt yields
**no config packet**, so the client never configures UDP/audio.

Other paths that (re)send these packets:

| trigger | effect |
|---|---|
| `/pv reload` (`BaseVoiceServer.java:428-437`) | `loadConfig(true)` then `sendConfigInfo` for every player with voice chat |
| UDP server restart (`BaseVoiceServer.java:323-357`) | for players connected before the restart: `requestPlayerInfo` each (`:352`) |
| `/vrc` reconnect (`VoiceReconnectCommand.kt:32-33`) | `removeConnection(player, RECONNECT)` then `requestPlayerInfo(player)` |
| UDP keep-alive timeout (`NettyUdpKeepAlive.java:40-55`, 100 ms tick, timeout = `voice.keepAliveTimeoutMs` default 15 000) | `removeConnection(..., TIMED_OUT)` then `requestPlayerInfo(player)` |
| `UdpClientDisconnectedEvent` (quit, timeout, reconnect, explicit) | `BaseVoicePlayerManager.onClientDisconnect` → `player.reset()` (clears distances/activations, `connected=false`) (`:85-89`, `BaseVoicePlayer.java:128-133`) |

---

## 2. Serverbound control packets — server behaviour and replies

Common prelude for every inbound packet (`PlayerChannelHandler.handlePacket`, `:52-64`):
`if (!udpServer.isPresent()) return;` (`:53`); fire `TcpPacketReceivedEvent` (cancellable — if
cancelled, stop, `:55-57`); then `packet.handle(this)` inside a try/catch that only debug-logs
(`:59-63`). So a malformed body cannot produce an error reply.

**id 10 `PlayerInfoPacket`** — see §1 phase 2. Reply: **yes**, id 1 `ConnectionPacket` (only if both
version gates pass and the RSA key parses). Body layout (`PlayerInfoPacket.java:38-60`, extends
`PlayerStatePacket`): `bool voiceDisabled`, `bool microphoneMuted`, `UTF minecraftVersion`
(read limit 64 chars), `UTF version` (limit 64), `int publicKeyLen` (`readSafeInt(1, 2048)`) + bytes.

**id 11 `PlayerStatePacket`** — `PlayerChannelHandler.java:106-126`. Guard `player.hasVoiceChat()`
(`:108`); `setVoiceDisabled`/`setMicrophoneMuted` (`BaseVoicePlayer.java:135-147`) return whether the
value **changed**; if neither changed → return (`:115`). Otherwise a rate-limited broadcast of the
whole player info: `elapsed = now - lastStateBroadcast`; if `elapsed >= 250` →
`broadcastPlayerState()` immediately (`:117-121`); else if `stateBroadcastScheduled.compareAndSet(false, true)`
succeeds → schedule `flushPlayerState` after `250 - elapsed` ms on the background executor and then the
Minecraft main thread (`:123-125`, `:221-227`); `flushPlayerState` clears the flag, re-checks
`hasVoiceChat` and broadcasts (`:196-201`); `broadcastPlayerState` stamps `lastStateBroadcast = now` and
calls `tcpConnections.broadcastPlayerInfoUpdate(player)` (`:190-194`). **Reply: no direct reply** — the
broadcast (which includes the sender) is the only output. Client trigger is a hotkey
(`client/.../config/hotkey/HotkeyActions.java:69-77`).

**id 12 `PlayerAudioEndPacket`** — `:139-146`. Guards: `hasVoiceChat()` (`:141`), not server-muted
(`:142`), not `isMicrophoneMuted()` (`:143`); then fires `PlayerSpeakEndEvent(player, packet)` (`:145`),
handled by `VoiceServerActivationManager.onPlayerSpeakEnd`
(`server-proxy-common/.../VoiceServerActivationManager.kt:171-212`): resolve the activation by id,
require it to be in `player.activeActivations` and to pass `checkPermissions`, recompute `distance` via
`calculateAllowedDistance` (`:183`), require `checkDistance` and the activation's requirements
(`:190-191`), then remove it from `activeActivations`, store
`lastActivationSequenceNumber[id] = sequenceNumber`, and fire `PlayerServerActivationEndEvent` plus
listeners (`:193-211`). **Reply: no TCP reply.** Body: `long sequenceNumber`, `UUID activationId`,
`short distance` (`PlayerAudioEndPacket.java:29-41`).

**id 13 `PlayerActivationDistancesPacket`** — `:128-137`. No `hasVoiceChat()` guard. Per
`(activationId -> distance)` entry: look up the activation; unknown ids are silently skipped
(`:132-133`); known → `setActivationDistance(activation, distance)`, stored in `distanceByActivationId`
and firing `PlayerActivationDistanceUpdateEvent` (`BaseVoicePlayer.java:108-116`). **Reply: none from
this handler**; side effect: for the proximity activation with a previous distance != -1,
`ProximityServerActivation.onActivationDistanceChange` sends **id 14 `DistanceVisualizePacket`** to
that same player (`ProximityServerActivation.kt:51-56`, `BaseVoicePlayer.java:83-90`). Body:
`int count` (`readSafeInt(0,127)`) then `{UUID activationId, int distance}` pairs
(`PlayerActivationDistancesPacket.java:27-43`); sent by the client on activation registration and on
slider change (`VoiceClientActivationManager.java:109-116`, `VoiceClientActivation.java:359`).

**id 15 `SourceInfoRequestPacket`** — `:148-173`. Guard `hasVoiceChat()` (`:150`); scan **all** source
lines for the first containing `packet.sourceId` (`:152-158`), not found → silent return; then
`source.notMatchFilters(player)` → debug-log and return (`:160-168`) — `notMatchFilters` is
`!matchFilters`, and `matchFilters` is false when the player disabled voice chat
(`api/server-proxy-common/.../ServerAudioSource.java:153-178`); else
`source.resolveSourceInfo().thenAccept(si -> player.sendPacket(new SourceInfoPacket(si)))` (`:170-172`).
**Reply: yes, id 16 `SourceInfoPacket` to the requester only**, async, only when filters pass. Body:
`UTF typeName` (`PLAYER|ENTITY|STATIC|DIRECT`) then `SourceInfo` fields (`SourceInfo.java:25-30,52-82`):
addonId, UUID id, nullable name (bool+UTF), `byte state`, nullable `CodecInfo`, `bool stereo`,
`UUID lineId`, `bool iconVisible`, `int angle`, then the subclass tail — STATIC: `Pos3d position` +
`Pos3d lookAngle`; ENTITY: `int entityId`; PLAYER: `VoicePlayerInfo`; DIRECT: nullable sender profile,
nullable relative position, `bool cameraRelative` (`StaticSourceInfo.java:45-59`,
`EntitySourceInfo.java:37-49`, `PlayerSourceInfo.java:39-52`, `DirectSourceInfo.java:53-79`). The proxy
mirrors this but replies with the cached `getSourceInfo()` and cancels forwarding
(`proxy/.../PlayerToServerChannelHandler.java:59-79`). Client side:
`VoiceClientSourceManager.sendSourceInfoRequest` remembers the id for 10 s correlation
(`VoiceClientSourceManager.kt:162-170`).

**id 5 `LanguageRequestPacket`** — `:175-188`. No `hasVoiceChat()` guard (only the `udpServer` guard in
`handlePacket`). `requestedLanguage = packet.getLanguage()`; throttled like the state broadcast but with
a 1 000 ms window (`:32`, `:179-187`); `sendLanguage` returns early if the stored language is null,
else sends **id 6 `LanguagePacket{languageName, Map<String,String>}`** (`:203-213`) from
`voiceServer.getLanguages().getClientLanguage(language)` (`VoiceServerLanguages.kt:77-78,248-259` — the
`[client]` table of the language TOML, with forced/default-language fallbacks). **Reply: yes, id 6 to the
requester, coalesced to at most one per 1 000 ms.** Body (`LanguagePacket.java:28-52`):
`UTF languageName`, `int size` (`readSafeInt(0,32767)`), then `(UTF key, UTF value)` pairs; request body
`UTF language`, read limit 32 chars (`LanguageRequestPacket.java:24-32`).

---

## 3. `ConfigPacket` (id 3) — every field, source, default, necessity

Class `protocol/.../tcp/clientbound/ConfigPacket.java`; it **extends `ConfigPlayerInfoPacket`**, so the
permissions map is the second-to-last section of the body and `PlayerIconConfig` is appended after it
(`:28`, `:98-106`, `:129-132`).

Wire order (`write`, `:109-133`):

| # | field | wire encoding | server value |
|---|-------|---------------|--------------|
| 1 | `serverId` | UUID (16 B) | `UUID.fromString(config.serverId())` (`VoiceTcpServerConnectionManager.java:128`) |
| 2 | `captureInfo` | `CaptureInfo` | see below |
| 3 | `encryption` | `bool present` + (`UTF algorithm`, `int len` (read 1..2048), bytes) | `EncryptionInfo("AES/CBC/PKCS5Padding", RSA(clientPubKey, aesKey))` |
| 4 | `sourceLines` | `int n` (read 0..127) + n × `VoiceSourceLine` | all registered lines, mapped per player |
| 5 | `activations` | `int n` (read 0..127) + n × `VoiceActivation` | all activations the player may use |
| 6 | `permissions` | `int n` (read 0..127) + n × (`UTF permission`, `bool value`) | `getPlayerPermissions(receiver)` |
| 7 | `playerIconConfig` | `int n` (read 0..5) + n × `UTF enumName` + `Pos3d` (3 doubles) | `[voice.player_icon]` |

Nested details:

* `captureInfo` (`data/audio/capture/CaptureInfo.java:26-42`): `int sampleRate`, `int mtuSize`,
  `bool hasEncoderInfo`, then `CodecInfo`. Server: sampleRate = `voice.sampleRate()` (default
  `48_000`), mtuSize = `voice.mtuSize()` (default `1024`), encoderInfo = `CodecInfo("opus", params)`
  with `params = { "mode": voice.opus().mode(), "bitrate": String.valueOf(voice.opus().bitrate()) }`
  (`VoiceTcpServerConnectionManager.java:104-106, 129-133`). Defaults/validators:
  `sampleRate` `48_000`, allowed set is inconsistent — the annotation lists
  `8000/16000/24000/48000` (`VoiceServerConfig.java:140-145`) while `SampleRateValidator.test` accepts
  `8000/12000/24000/48000` (`:325-337`); `mtuSize` `1024`, valid 128..5000 (`:154-159`, `:314-323`);
  `opus.mode` default `"VOIP"`, allowed `VOIP|AUDIO|RESTRICTED_LOWDELAY` (`:270-299`);
  `opus.bitrate` default `-1000` (auto), allowed `-1000`, `-1`, `500..512000` (`:281-311`).
* `CodecInfo` (`data/audio/codec/CodecInfo.java:28-49`): `UTF name`, `int n` (`readSafeInt(0, 128)`),
  n × (`UTF key`, `UTF value`). `params` is a `HashMap`, so **param order is not deterministic**.
* `encryption` (`data/encryption/EncryptionInfo.java:27-46`): produced by
  `Cipher.getInstance("RSA")` + `init(ENCRYPT_MODE, publicKey)` + `doFinal(voice.aesEncryptionKey())`
  (`VoiceTcpServerConnectionManager.java:110-125`). `"RSA"` means the provider default
  `RSA/ECB/PKCS1Padding` (SunJCE). The plaintext is the 16-byte AES key: generated at config load from
  a random UUID's two longs (`BaseVoiceServer.java:282-295`), persisted in `config.voice().aesEncryptionKey()`,
  replaceable at runtime from the proxy forwarding secret path (`ServerServiceChannelHandler.java:49`).
  The client decrypts with `Cipher.getInstance("RSA")` + `DECRYPT_MODE` and the key pair whose public
  half it sent in `PlayerInfoPacket` (`client/.../ModServerConnection.java:267-285`). If decryption
  fails the client drops the whole UDP client (`:280-284`) — so a wrong/mismatched key pair after a
  server restart breaks the session until re-handshake.
* `sourceLines` = `sourceLineManager.getLines().stream().map(line -> line.getSourceLineForPlayer(receiver)).collect(Collectors.toSet())`
  (`VoiceTcpServerConnectionManager.java:135-139`) — a `HashSet`, therefore **iteration/serialization
  order is unspecified**; a Rust port must not rely on any order and a Rust client must not either.
* `activations` = `activationManager.getActivations().stream().filter(a -> a.checkPermissions(receiver)).collect(toSet())`
  (`:140-145`) — also a `HashSet`.
* `permissions` = for each entry of `playerManager.getSynchronizedPermissions()`, the player's boolean
  (`:188-198`). Only one permission is registered by default: `pv.allow_freecam`
  (`BaseVoiceServer.java:147`, `server/common/.../command/Permissions.kt:20` — `ALLOW_FREECAM`,
  `PermissionDefault.TRUE`). Addons add more via `registerPermission`.
* `playerIconConfig` = `PlayerIconConfig(new HashSet<>(config.voice().playerIcon().visibility()),
  new Pos3d(0.0, config.voice().playerIcon().yOffset(), 0.0))` (`:147-150`). Config defaults:
  `visibility` = `new ArrayList<>(PlayerIconVisibility.none())` → **empty list** (all icons shown),
  `yOffset` = `0.0` (`VoiceServerConfig.java:212-218`). Enum order/names:
  `HIDE_NOT_INSTALLED, HIDE_VOICE_CHAT_DISABLED, HIDE_SERVER_MUTED, HIDE_CLIENT_MUTED, HIDE_SOURCE_ICON`
  (`protocol/.../data/config/PlayerIconVisibility.kt:13-48`) — the client parses by `valueOf(name)`,
  so the names are wire-visible. `PlayerIconConfig.serialize` writes `int size` then `writeUTF(name)`
  then the offset (`PlayerIconConfig.kt:53-57`); `deserialize` uses
  `readSafeInt(input, 0, PlayerIconVisibility.entries.size)` → max 5 (`:41-51`).

**Defaults / backward compatibility.** Only `playerIconConfig` has a compatibility fallback:
`ConfigPacket.read` wraps it in `try { ... } catch (Exception ignored) {}` (`:100-106`), leaving the
field `null` for servers ≤ 2.1.6. Every other field is mandatory on the wire.

**What a client requires to function** (read from the client, not guessed): `serverId` (non-null;
used as the client config key and `ServerInfo` identity — `client/.../VoiceServerInfo.java:43,69-88`,
`ModServerConnection.java:287-304`); `captureInfo.sampleRate` (output `AudioFormat` and frame size —
`ModServerConnection.java:328-334`, `VoiceServerInfo.java:144-157`); `captureInfo.mtuSize` (encoder
MTU — `VoiceServerInfo.java:111-116`); `captureInfo.encoderInfo != null` (`createOpusEncoder` throws
`IllegalStateException("server codec info is empty")`, `:104-117`, and the decoder path throws too,
`:120-132`); `playerIconConfig` (the wire read tolerates absence, but `VoiceServerInfo.java:89-95`
dereferences it unconditionally, so a null NPEs ⇒ **always emit section 7**, an empty visibility set is
fine: `int 0` + 3 zero doubles). Tolerated if absent/empty: `sourceLines`/`activations` (the client
try/catches per entry, `ModServerConnection.java:306-320`, but without activations it cannot key up)
and `permissions` (`VoiceServerInfo.java:87,161-167`; `pv.allow_freecam` is functional, not
structural). `encryption` is optional (`null` = plaintext); when present the client must decrypt it or
it tears the UDP client down (`ModServerConnection.java:268-285`).

---

## 4. Player-list / state packets: full field lists, `state`, transitions

**`VoicePlayerInfo`** (the shared payload) — `protocol/.../data/player/VoicePlayerInfo.java:29-45`,
wire order: `UUID playerId`, `UTF playerNick`, `bool muted`, `bool voiceDisabled`,
`bool microphoneMuted`. Built by `VoiceServerPlayerEntity.createPlayerInfo()`
(`server/common/.../player/VoiceServerPlayerEntity.java:40-56`):
`muted = muteManager.getMute(uuid).isPresent()`, `voiceDisabled = isVoiceDisabled()`,
`microphoneMuted = isMicrophoneMuted()`; then a `PlayerInfoCreateEvent` may replace the object
(`:52-55`) — addons can therefore alter these five fields.

| packet | body | sender / filter |
|---|---|---|
| 7 `PlayerListPacket` (`PlayerListPacket.java:27-44`) | `int count` (read 0..32767) + n × `VoicePlayerInfo` | every `udpConnectionManager.getConnections()` entry (= `connectionByPlayerId.values`, `VoiceUdpServerConnectionManager.java:114-117`) filtered by `receiver.canSee(other.instance)` ⇒ **only players with an active UDP connection appear** (`VoiceTcpServerConnectionManager.java:157-168`) |
| 8 `PlayerInfoUpdatePacket` (`:22-31`) | one `VoicePlayerInfo` | broadcast under the same lock with filter `p1 -> p1.instance.canSee(player.instance)` (`:170-175`, `:184-186`) ⇒ the subject is included, players who cannot see them are not |
| 9 `PlayerDisconnectPacket` (`:25-33`) | one UUID `playerId` | broadcast, same lock and vanish filter (`:177-182`); triggered by `NettyUdpServerConnection.disconnect()` (`NettyUdpServerConnection.java:108-113`) for every removal (quit/timeout/reconnect/replacement) and by the vanish listener when hidden (`VanishListener.kt:14-19`) |
| 10 `PlayerInfoPacket` (serverbound) | see §1/§2 | its `VoicePlayerInfo playerInfo` field is never read and never serialized (`PlayerInfoPacket.java:19-24,38-60`) |
| 11 `PlayerStatePacket` (serverbound) | `bool voiceDisabled`, `bool microphoneMuted` (`PlayerStatePacket.java:23-33`) | the "state" = *voice chat disabled on the client* + *mic muted on the client*, exactly what the client's own config reports (`HotkeyActions.java:69-77`); **not** `connected`, not server-mute, not numeric |

All three outbound player packets share `synchronized (playerStateLock)`
(`VoiceTcpServerConnectionManager.java:38,159,179`), so list/update frames cannot interleave.

**Transitions.** `BaseVoicePlayer` holds `voiceDisabled`, `microphoneMuted`, `connected`, plus
`distanceByActivationId`, `activeActivations`, `lastActivationSequenceNumber`
(`server-proxy-common/.../BaseVoicePlayer.java:36-51`).

| transition | where |
|---|---|
| initial values | `PlayerInfoPacket` sets both booleans (id 10); `connected` stays false |
| `voiceDisabled`/`microphoneMuted` flip | `PlayerStatePacket` (id 11); only a real change triggers the rate-limited broadcast |
| server mute | `VoiceMuteManager.mute/unmute` → `broadcastPlayerInfoUpdate` (`VoiceMuteManager.java:80`, `:111`); muting also short-circuits `PlayerAudioPacket` (`NettyUdpServerConnection.java:126`) and `PlayerAudioEndPacket` (`PlayerChannelHandler.java:142`) |
| `connected: false → true` | `UdpClientConnectedEvent` → `BaseVoicePlayerManager.onClientConnect` (`:79-83`) — this is what enables `hasVoiceChat()` and therefore the broadcast/list filters |
| `connected → false` + clear distances/activations | `UdpClientDisconnectedEvent` → `player.reset()` (`:85-89`) |
| activation active/inactive | `onPlayerSpeak` adds to `activeActivations` when the sequence number advances (or jumps > 10) (`VoiceServerActivationManager.kt:143-151`); `onPlayerSpeakEnd` removes it and stores the last sequence number (`:193-194`) |

Separately, `SourceInfo.state` is a **byte** describing an audio *source*, not a player: it increments
by 1 on name changes and by 10 on `setIconVisible`/`setStereo`, and a client whose cached state
differs re-requests the source info (`ServerAudioSource.java:17-99`, `SourceInfo.java:41,73`).

---

## 5. Activations and source lines: identity, fields, learning

**Identity is derived from the name, never transmitted.**
`VoiceActivation.generateId(name) = UUID.nameUUIDFromBytes(UTF8(name + "_activation"))`
(`data/audio/capture/VoiceActivation.java:29-34`); `VoiceSourceLine.generateId(name) =
UUID.nameUUIDFromBytes(UTF8(name + "_line"))` (`data/audio/line/VoiceSourceLine.java:30-35`).
`nameUUIDFromBytes` is Java's MD5-based UUID v3 (not RFC 4122 v3 hashing) — the Rust port must
reproduce MD5 over the exact ASCII bytes `"<name>_activation"` / `"<name>_line"` with version/variant
bits set by Java's algorithm. Names must match `[a-z0-9-_]+`
(`Activation.java:20`, `SourceLine.java:20`), enforced at builder time
(`VoiceServerActivationManager.kt:74-80`, `VoiceBaseServerSourceLineManager.kt:67-73`).

**`VoiceActivation` on the wire** (`VoiceActivation.java:121-134`) — this is also the body of
`ActivationRegisterPacket` (id 19, `ActivationRegisterPacket.java:24-33`); id 20
`ActivationUnregisterPacket` carries only `UUID activationId` (`ActivationUnregisterPacket.java:25-33`):

| order | field | encoding | note |
|---|---|---|---|
| 1 | `name` | UTF | id is derived from this |
| 2 | `translation` | UTF | translation key, e.g. `pv.activation.proximity` |
| 3 | `icon` | UTF | `ResourceLocation` or `base64;<data>` (`ServerActivationManager.kt:45-63`) |
| 4 | `distances` | `int n` (read 0..64) + n ints | empty = any distance |
| 5 | `defaultDistance` | int | normalized on write *and* read by `validateDefaultDistance` |
| 6-8 | `proximity`, `transitive`, `stereoSupported` | 3 bools | non-transitive stops subsequent activations (`Activation.java:94-103`) |
| 9 | `encoderInfo` | `bool present` + `CodecInfo` | per-activation encoder override |
| 10 | `weight` | int | lower = higher in the client menu/overlay |

`validateDefaultDistance` (`:136-154`): empty distances → `0`; dynamic distances
(`size == 2 && distances[0] == -1`) → keep if `1 <= defaultDistance <= distances[1]`, else
`distances[1] / 2`; otherwise keep if `distances.contains(defaultDistance)`, else
`distances[(int) floor(size / 2.0)]`. `checkDistance` (`:157-169`): empty → true; dynamic →
`1 <= d <= distances[1]`; else `distances.contains(d)`.

Builder defaults (`VoiceServerActivationManager.kt:262-269`): `distances = []`,
`defaultDistance = 0`, `transitive = true`, `proximity = true`, `stereoSupported = false`,
`encoderInfo = null`; `permission` is a single-element set (`:86-88`) and `weight` = config override
`voice.weights.activations.<name>` if present, else the builder argument (`:88`,
`VoiceServerConfig.java:225-244`). Note these builder defaults differ from the plain
`VoiceActivation` field defaults (`proximity=false`, `transitive=false`, `VoiceActivation.java:45-56`),
but the builder always passes explicit values.

**Proximity activation** (`server/common/.../ProximityServerActivation.kt:16-49`): name `"proximity"`,
translation `"pv.activation.proximity"`, icon `plasmovoice:textures/icons/microphone.png`,
permission `pv.activation.proximity` (default TRUE, `Permissions.kt:22`), weight `1`,
`distances = voice.proximity().distances()` (default `[8, 16, 32]`, sorted ascending by
`DistancesSorter`, `VoiceServerConfig.java:250-267`), `defaultDistance = voice.proximity().defaultDistance()`
(default `16`), `proximity = true`, `transitive = true`, `stereoSupported = false`.
It is re-registered on every `loadConfig` (`BaseVoiceServer.java:305`) after unregistering the old
one, which broadcasts id 20 then id 19.

**`VoiceSourceLine` on the wire** (`VoiceSourceLine.java:84-99`) — also the body of
`SourceLineRegisterPacket` (id 21):
`UTF name`, `UTF translation`, `UTF icon`, `double defaultVolume`, `int weight`,
`bool hasPlayers`, and if true `int n` (read 0..32767) + n × `McGameProfile`.
`McGameProfile` (`serializer/McGameProfileSerializer.kt:32-43`): `UUID id`, `UTF name`,
`int propertyCount` (read 0..100), then `(UTF name, UTF value, UTF signature)` triples (empty string
when the signature is null). `defaultVolume` is clamped to `[0,1]` in the constructor
(`VoiceSourceLine.java:62`); builder default is `1.0` (`VoiceBaseServerSourceLineManager.kt:97`);
`weight` comes from `voice.weights.source_lines.<name>` if present, else the builder argument
(`VoiceServerSourceLineManager.kt:29-30`).
**Upstream quirk worth reproducing exactly:** `getSourceLineForPlayer`
(`VoiceBaseServerSourceLine.kt:40-56`) creates a per-player line when `playerSetManager != null` and
then executes `.also { players = Sets.newHashSet() }` on the *shared* line, so after the first such
call the shared line reports `hasPlayers() == true` with an **empty** player set and serializes
`bool true` + `int 0`; lines without a player-set manager keep `players == null` and serialize
`bool false`.
Proximity source line: name `"proximity"`, translation `"pv.activation.proximity"`, icon
`plasmovoice:textures/icons/speaker.png`, defaultVolume `1.0`, weight `1`, `withPlayers = false`
(`ProximityServerActivation.kt:39-45`).

**How a client learns them:** id 3 `ConfigPacket` (§3) carries the full activation + source-line
catalogue (unicast on UDP registration). Later additions/removals: id 19/20
`Activation(Un)RegisterPacket` broadcast, filtered by `activation.checkPermissions`
(`VoiceServerActivationManager.kt:336-346`, `:92-114`), or sent to a single player when only that
player's permission changed (`:214-251`; the wildcard `pv.activation.*` re-syncs everything,
`:219-238`); id 22 `SourceLineUnregisterPacket` broadcast with no filter
(`VoiceBaseServerSourceLineManager.kt:34-43`); id 23/24/25 per player-set for `withPlayers` lines
(`VoiceServerPlayerSetManager.kt:25-129`). **A source line added after a player's `ConfigPacket` is
never announced** — `Builder.build()` registers it without sending id 21
(`VoiceBaseServerSourceLineManager.kt:107-124`) and `SourceLineRegisterPacket` is constructed nowhere
in server code, so the client sees it only on its next `ConfigPacket`. A source's own info arrives as
id 16 in reply to id 15 (§2), pushed as id 17 `SelfSourceInfoPacket` to the speaker
(`SelfActivationHelper.kt:83-106`) and pushed on source-info changes (`:118-141`).

Source *ids* are `UUID.randomUUID()`-based and travel inside `SourceInfo`; they are unrelated to the
name-derived line/activation ids. There is no "source register" packet — sources are learned lazily by
id via 15/16.

---

## 6. Broadcast vs single-client

`broadcast(packet, filter)` iterates `playerManager.getPlayers()` and sends when
`(filter == null || filter.test(player)) && player.hasVoiceChat()` —
`VoiceTcpServerConnectionManager.java:45-50`; `hasVoiceChat()` is the `connected` flag (§4), so
**players who joined Minecraft but never completed the UDP handshake receive no broadcasts at all**.

**Broadcast** (only to players with voice chat, §4): id 8 `PlayerInfoUpdatePacket` (state change via
id 11, UDP registration burst, mute/unmute, unhide), id 9 `PlayerDisconnectPacket` (UDP
disconnect/replace/timeout/quit, vanish-hide — which also excludes the subject, `VanishListener.kt:15-19`),
id 19/20 `Activation(Un)RegisterPacket`, id 22 `SourceLineUnregisterPacket`. The id 8/9 filter is
`p -> p.canSee(subject)` (`VoiceTcpServerConnectionManager.java:184-186`); id 19/20 use
`activation.checkPermissions(p)` (`VoiceServerActivationManager.kt:109-111,342-344`); id 22 has no
filter (`VoiceBaseServerSourceLineManager.kt:38`).

**Unicast (sent to exactly one player):** 1 `ConnectionPacket` (the connecting player), 2
`PlayerInfoRequestPacket` (join/enable/reconnect/timeout/restart), 3 `ConfigPacket` (UDP burst, and
once per player on `/pv reload`), 4 `ConfigPlayerInfoPacket` (the player whose *synchronized*
permission changed — `BaseVoicePlayerManager.java:66-77`), 6 `LanguagePacket` (requester), 7
`PlayerListPacket` (the newly connected player), 14 `DistanceVisualizePacket` (proximity distance
change), 16 `SourceInfoPacket` (requester), 17 `SelfSourceInfoPacket` (the speaking player), 18
`SourceAudioEndPacket` (the speaking player / direct-source listener), 19/20 on a permission update,
23/24/25 (players in the affected player-set), 26 `AnimatedActionBarPacket`
(`BaseVoicePlayer.java:92-101`).

Every send funnels through `VoiceServerPlayerEntity.sendPacket` (`:30-38`), which first fires the
cancellable `TcpPacketSendEvent`; `EventBus.fire` returns `false` when cancelled
(`api/common/.../event/EventBus.java:12-19`), in which case the packet is dropped. `PacketTcpCodec.encode`
returning `null` for an unregistered class is not checked there, so a `null` would reach the platform
sender (`:35-37`).

---

## 7. Service channel (proxy AES forwarding) — briefly, for completeness

`ServerServiceChannelHandler.receive` (`server/common/.../ServerServiceChannelHandler.java:28-62`) is
registered on `plasmo:voice/v2/service` and is **not** part of the packet-id space: if
`config.host().forwardingSecret() == null` → return (`:29`); else read `byte[32] signature` +
`byte[64] aesEncryptionKey` via `PacketUtil.readBytes` (`:34-35`, i.e. `int` length then bytes);
HMAC-SHA256 over the key with the forwarding-secret UUID bytes as the key, compare in constant time
(`:37-47`) — mismatch logs a warning and returns (`:45-46`); on success
`voiceServer.updateAesEncryptionKey(aesEncryptionKey)` (`:49`, → `BaseVoiceServer.java:313-321`) and
reply on the same channel with `int 32` + the signature (`:51-54`). Forwarding secrets come from
`PLASMO_VOICE_FORWARDING_SECRET`, the `forwarding-secret` file, or the config
(`BaseVoiceServer.java:255-269`).

---

## 8. Not determined / caveats

1. **Outer plugin-message envelope.** `su.plo.slib` is absent from this checkout, so whether the
   platform adds a length prefix or channel-id around `PacketTcpCodec.encode`'s bytes could not be
   read. Established: within the PV payload there is **no** length prefix, and the server feeds the
   received `byte[]` directly to `PacketTcpCodec.decode` (`ServerChannelHandler.java:35-37`). The
   client's `ByteArrayPayload`/`ByteArrayCodec` path (`ModServerConnection.java:116-135`) shows one
   extra type-id indirection that the server side (`sendPacket(channel, byte[])`) does not name.
2. **`canSee` / vanish semantics.** Only use sites were read
   (`VoiceTcpServerConnectionManager.java:163,184-186`, `VanishListener.kt:8-23`); the predicate
   itself is a slib/Minecraft API not present here.
3. **No deterministic ordering** for `sourceLines`/`activations`/`CodecInfo.params`/permissions:
   `HashSet`/`HashMap`/`toSet()` at `VoiceTcpServerConnectionManager.java:104,139,145,148` and
   `BaseVoicePlayerManager.java:27`. Byte-for-byte equality with upstream is unachievable for those
   sections; only set equality is.
4. **The `playerIconConfig` NPE claim** is inferred from the unconditional dereference at
   `VoiceServerInfo.java:89-95` plus the tolerant read at `ConfigPacket.java:100-106`; I did not run a
   client against a 2.1.6 server.
5. **Proxy/BungeeCord address handling.** `[host.public]` is documented as ignored under a proxy
   (`VoiceServerConfig.java:97-102`) and `NettyPacketHandler.java:60-65` records a proxy-supplied
   `connectionAddress` from the UDP `PingPacket`, but the proxy module's own `TcpServerPacketManager`
   was not read, so how it rewrites the `ConnectionPacket` ip/port is unverified. Only §1 phase 2 is
   authoritative.
6. **`PermissionTristate` / `PlayerPermissionUpdateEvent` origin** (LuckPerms listener,
   `server-proxy-common/.../LuckPermsListener.kt`) was not read, so id 4's trigger frequency depends
   on that listener.
7. **`TcpPacketReceivedEvent` vs the info-request retry loop**: the listener drops the pending entry on
   *any* inbound TCP packet (`PlayerInfoRequestScheduler.kt:51-54`), so one non-info packet stops the
   resends even if no key ever arrives. Whether real clients do that is not verifiable here.
8. Debug logging gating, log text and thread pools were not exhaustively audited (irrelevant to the wire).
