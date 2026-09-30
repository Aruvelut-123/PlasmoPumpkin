# RECON — server-side UDP audio path & relay decision

Reference checkout: `C:\Users\Baymaxawa\AppData\Local\Temp\pv\plasmo-voice`.
All citations are repo-relative `path:line`. Nothing below is inferred from names alone; every claim was
read out of the file named.

Scope: the **server** (Bukkit/Paper/Minestom path, `server/common` + `server-proxy-common` +
`api/server` + `api/server-proxy-common`) receiving `PlayerAudioPacket` over UDP and fanning audio
out to other players. The Velocity/BungeeCord raw-forwarding path
(`proxy/common/.../NettyUdpProxyConnection.java`) is noted only where it differs.

---

## 0. The UDP header (exact layout)

`PacketUdpCodec.encodeThrowing` (`protocol/src/main/java/su/plo/voice/proto/packets/udp/PacketUdpCodec.java:45-59`):

| offset | size | field | writer |
|---|---|---|---|
| 0 | 4 | magic `0x4e9004e9` (BE int) | `:53` (`MAGIC_NUMBER` `:24`) |
| 4 | 1 | packet id (`writeByte(type)` → **low byte only**) | `:54` |
| 5 | 16 | secret UUID (2× BE long, MSB first) | `:55` |
| 21 | 8 | timestamp, `System.currentTimeMillis()` (BE long) | `:56` |
| 29 | … | packet body | `:58` |

Decode: `:78-91` (`decodeThrowing`, throws) and `:93-116` (`decode`, returns empty). Both read
magic → id → secret → timestamp and hand the remaining buffer to `PacketUdp`
(`protocol/.../udp/PacketUdp.java:27-37`, lazy body read at `:66-72`).

UDP registry ids (`PacketUdpCodec.java:27-35`):

| id | packet | direction |
|---|---|---|
| `1` | `PingPacket` | ANY |
| `2` | `PlayerAudioPacket` | SERVER |
| `3` | `SourceAudioPacket` | CLIENT |
| `4` | `SelfAudioInfoPacket` | CLIENT |
| `0x100` | `CustomPacket` | ANY |

Trap: `0x100` is registered but `encodeThrowing` writes it with `writeByte` → `0x00`, which is not in
the registry, so `CustomPacket` can never round-trip (`PacketUdpCodec.java:34` vs `:54`; `byType`
`protocol/.../PacketRegistry.java:31-48`). `PacketDirection` is a Kotlin enum; `accepts` is
`direction == ANY || this == ANY || direction == this` (`protocol/src/main/kotlin/su/plo/voice/proto/packets/PacketDirection.kt:3-10`).

Server pipeline (`server/common/src/main/java/su/plo/voice/server/socket/NettyUdpServer.java:81-91`):
`flush_consolidation` (`FlushConsolidationHandler(256, true)`, `:86`) → `decoder`
(`NettyPacketUdpDecoder(PacketDirection.SERVER)`, `:87`) → `handler` (`NettyPacketHandler`, `:88`) →
`exception_handler` (`:89`). `NettyExceptionHandler` only logs, it does **not** close the channel
(`common/src/main/kotlin/su/plo/voice/socket/NettyExceptionHandler.kt:8-10`). Bad magic / unknown id
therefore surfaces as a swallowed exception at `NettyPacketHandler.java:38-40`.
`SO_REUSEPORT` may bind N sockets on the same port (`NettyUdpServer.java:93-121`); all N share the
pipeline, and per-connection replies are written on **whichever** channel the speaker's datagram
arrived on, because the channel is captured from `ctx.channel()` (`NettyPacketHandler.java:55`).

---

## 1. `NettyUdpServerConnection` — every field

`server/common/src/main/java/su/plo/voice/server/socket/NettyUdpServerConnection.java`

| field | type | line | notes |
|---|---|---|---|
| `voiceServer` | `BaseVoiceServer` | `:32` | final |
| `channel` | `DatagramChannel` | `:33` | final; the **receiving** socket, not a per-peer socket |
| `remoteAddress` | `InetSocketAddress` | `:36` | `@Getter`; target of every outgoing datagram; mutable, see `setRemoteAddress` `:66-72` |
| `connectionAddress` | `InetSocketAddress` | `:39` | `@Getter @Setter`; public address the client reported in `PingPacket` (`NettyPacketHandler.java:60-65`) |
| `secret` | `UUID` | `:41` | final, `@Getter`; per-player, minted once (`VoiceUdpServerConnectionManager.java:49-60`) |
| `player` | `VoiceServerPlayer` | `:43` | final, `@Getter` |
| `keepAlive` | `long` | `:45` | `@Getter`, init `System.currentTimeMillis()`; set by `handle(PingPacket)` `:117`; **not** used for timeout |
| `sentKeepAlive` | `long` | `:48` | `@Getter @Setter`; scheduled *next* send time, written by `NettyUdpKeepAlive.java:51` |
| `lastReceivedPacketTimestamp` | `long` | `:50` | `@Getter`, init now; refreshed at `:105`; drives the timeout |
| `connected` | `boolean` | `:53` | `@Getter`, init `true`; only `disconnect()` sets false |

`@ToString(of = {channel, secret, player, keepAlive, sentKeepAlive})` `:29`.
Constructor `(voiceServer, channel, secret, player)` `:55-63` — remote address is *not* set here.

`sendPacket(Packet<?>)` `:74-95`:
1. `UdpPacketSendEvent` fired first; a cancelled event drops the packet (`:76-79`).
2. `PacketUdpCodec.encodeThrowing(packet, secret, new ByteBufDataOutput(buf))` `:83` — **this**
   connection's own secret (`:41`), and a **fresh** `System.currentTimeMillis()` (`PacketUdpCodec.java:56`).
   Encode failure logs and returns (`:84-88`).
3. `channel.writeAndFlush(new DatagramPacket(buf, remoteAddress), channel.voidPromise())` `:90`.
4. `UdpPacketSentEvent` fired after the write is *queued* (`:92-94`, not after flush completes).

`handlePacket(Packet<ServerPacketUdpHandler>)` `:97-106`:
1. `UdpPacketReceivedEvent` fired `:99-102`; if cancelled → `return` **before** the timestamp update.
2. `packet.handle(this)` `:104` — dispatches to one of the three handlers below.
3. `lastReceivedPacketTimestamp = System.currentTimeMillis()` `:105`. This line is *after* the handler,
   so an exception thrown by a handler (caught at `NettyPacketHandler.java:38`) leaves the stamp stale.

`disconnect()` `:108-113`: `connected = false`, then
`tcpPacketManager.broadcastPlayerDisconnect(player)` — a **TCP** `PlayerDisconnectPacket` to all
players who can see this player (`VoiceTcpServerConnectionManager.java:178-186`). It does **not**
remove the entry from the connection maps; the manager does that (`VoiceUdpServerConnectionManager.java:124-138`).

### `handlePacket` per packet type

| packet | handler | line | behaviour |
|---|---|---|---|
| `PingPacket` | `handle(PingPacket)` | `:115-118` | `keepAlive = now`. **No reply.** The server never echoes a ping. |
| `CustomPacket` | `handle(CustomPacket)` | `:120-122` | empty body — silently ignored. |
| `PlayerAudioPacket` | `handle(PlayerAudioPacket)` | `:124-130` | mute gates + fire `PlayerSpeakEvent`. |

`handle(PlayerAudioPacket)` in full: `:125-130`

```java
if (voiceServer.getMuteManager().getMute(player.getInstance().getUuid()).isPresent()) return; // :126
if (player.isMicrophoneMuted()) return;                                                       // :127
voiceServer.getEventBus().fire(new PlayerSpeakEvent(player, packet));                         // :129
```

It does **not** check `hasVoiceChat()`, permissions, activation existence, distance, or the source.
Everything else happens in the listener chain in §2. Note it does not inspect the return value of
`fire` — a cancelled `PlayerSpeakEvent` has no effect here, cancellation only stops *later*
subscribers (`VoiceEventBus.kt:70-83`, `:25-37`).

---

## 2. The relay chain, end to end

```
NettyPacketHandler.channelRead0                 server/common/.../socket/NettyPacketHandler.java:25-72
  └─ connection.handlePacket(packet)            NettyUdpServerConnection.java:97-106
      └─ handle(PlayerAudioPacket)              NettyUdpServerConnection.java:124-130
          └─ fire(PlayerSpeakEvent)             → VoiceServerActivationManager.onPlayerSpeak  (LOW)
              └─ activation listeners           → ProximityServerActivationHelper.onActivation
                  └─ source.sendAudioPacket(SourceAudioPacket, distance, activationInfo)
                      ├─ fire(ServerSourceAudioPacketEvent)
                      │    └─ SelfActivationHelper.onSourceAudioPacket  (HIGHEST) → SelfAudioInfoPacket → speaker
                      └─ getListeners(distance).forEach { it.sendPacket(newSourceAudioPacket) }
                                                                    → SourceAudioPacket → recipients
```

### 2.1 `VoiceServerActivationManager.onPlayerSpeak` — `@EventSubscribe(priority = LOW)`

`server-proxy-common/src/main/kotlin/su/plo/voice/server/audio/capture/VoiceServerActivationManager.kt:121-169`

1. `getActivationById(originalPacket.activationId)`; missing → return `:126-128`.
2. `activation.checkPermissions(player)` false → return `:130`
   (`VoiceServerActivation.kt:64-65`: `permissions.any { serverPlayer.hasPermission(it) }`).
3. **Distance clamp**: `distance = activation.calculateAllowedDistance(originalPacket.distance.toInt()).toShort()` `:132`.
   `Activation.calculateAllowedDistance` = the packet distance if `checkDistance` passes, else
   `defaultDistance` (`protocol/.../data/audio/capture/Activation.java:178-184`; `checkDistance`
   `:157-169`: empty list ⇒ always true; `[-1, max]` ⇒ `1..max`; else exact membership).
4. A **new** `PlayerAudioPacket` is constructed with the clamped distance `:133-139`
   (`sequenceNumber`, `data`, `activationId`, `distance`, `isStereo` — all copied). Downstream code
   therefore sees the clamped distance, never the client's raw one.
5. `activation.requirements?.checkRequirements(player, originalPacket)` false → return `:141`.
6. Activation-start bookkeeping `:143-151`: with `lastActivationSequenceNumber` defaulting to `0`,
   if the activation is not already active and (`packet.sequenceNumber > last || |packet.seq - last| > 10`)
   → add to `player.activeActivations`, fire `PlayerServerActivationStartEvent`, run start listeners.
7. `PlayerServerActivationEvent` fired `:153-159`; `result == HANDLED` → cancel `PlayerSpeakEvent`, return.
8. Otherwise every `activation.activationListeners` callback is invoked with the clamped packet
   `:161-168`; the first `HANDLED` cancels the event and breaks.

There is **no** activation-proximity check here: `Activation.isProximity()` is client-side only
(used in `client/.../VoiceAudioCapture.java:266,286,295` and the settings GUI). The server relays any
activation that has a registered listener.

### 2.2 `ProximityServerActivationHelper` — the actual relay for proximity

`api/server/src/main/kotlin/su/plo/voice/api/server/audio/capture/ProximityServerActivationHelper.kt`

Registered as a player-activation listener in its `init` (`:60-63`). The instance the server creates
is the built-in "proximity" activation + line (`server/common/.../audio/capture/ProximityServerActivation.kt:16-49`,
helper constructed `:47`, listeners registered `:48`).

`onActivation(player, packet)` `:78-97`:
- `getPlayerSource(player, packet.isStereo)` `:79` — lazily creates **one** `ServerPlayerSource` per
  player UUID and caches it forever in `sourceByPlayerId` `:113-124`; on every call it re-applies
  `setStereo(isStereo && activation.isStereoSupported)` `:120-122`.
- `distance = distanceSupplier?.getDistance(player, packet) ?: packet.distance` `:80` — for the
  built-in proximity activation the supplier is `null` (`ProximityServerActivation.kt:47`), so the
  already-clamped distance is used.
- Builds a **new clientbound packet**:
  `SourceAudioPacket(packet.sequenceNumber, source.state.toByte(), packet.data, source.id, distance)` `:82-87`.
- `activationInfo = PlayerActivationInfo(player, packet)` `:89`
  (`api/server-proxy-common/.../PlayerActivationInfo.kt:9-12`).
- `source.sendAudioPacket(sourcePacket, distance, activationInfo)`; `true` → `Result.HANDLED`,
  else `Result.IGNORED` `:91-96`.

Source creation: `sourceLine.createPlayerSource(player)` → `ServerSourceLine.createPlayerSource`
defaults (`api/server/.../audio/line/ServerSourceLine.kt:65-68` → `:49-53` → `:32-37`: `stereo=false`,
`decoderInfo=OpusDecoderInfo()`) → `VoiceServerSourceLine.createPlayerSource`
(`server/common/.../audio/line/VoiceServerSourceLine.kt:39-54`) constructs `VoiceServerPlayerSource`
with a fresh `UUID.randomUUID()` and registers it in the line's `sourceById`
(`VoiceBaseServerSourceLine.kt:106-109`).

`onActivationEnd` `:99-111`: also guarded by `player.activeActivations` / permission / distance /
requirements in the activation manager (`VoiceServerActivationManager.kt:171-212`, removal at `:193`,
`lastActivationSequenceNumber` write at `:194`), then builds
`SourceAudioEndPacket(source.id, packet.sequenceNumber)` `:103` and calls
`source.sendPacket(sourceEndPacket, distance)` `:105` — that is **TCP**, see §5/§6.

Source removal: on `UdpClientDisconnectedEvent` the cached player source is removed and its
`remove()` called (`ProximityServerActivationHelper.kt:73-76`).

### 2.3 `VoiceServerProximitySource.sendAudioPacket` — fan-out

`server/common/src/main/kotlin/su/plo/voice/server/audio/source/VoiceServerProximitySource.kt:38-61`

1. `ServerSourceAudioPacketEvent(this, packet, distance, activationInfo)` fired `:43`; `fire` false
   (cancelled) → return `false` `:44` — **no self-info, no fan-out**.
2. `event.result == HANDLED` → return `true` without sending `:45`.
3. `packet.sourceState = state.get().toByte()` `:48` — the source's state is stamped on the outgoing
   packet at the last moment.
4. Dirty flush `:51-54`: if the source was marked dirty, `resolveSourceInfo().thenAccept { sendPacket(SourceInfoPacket(it), event.distance) }`
   — i.e. a **TCP** `SourceInfoPacket` to the same listener set.
5. `getListeners(event.distance).forEach { connection -> connection.sendPacket(packet) }` `:56-58`.

Step 5 is the whole relay: the **same** `SourceAudioPacket` instance is handed to N connections, and
each connection re-encodes it with its own secret and its own timestamp in `sendPacket` (§1).

---

## 3. The exact relay decision (answers Q1)

For one inbound `PlayerAudioPacket` from player **S** (the speaker):

* **What is sent.** Never the inbound packet. `PlayerAudioPacket` (UDP id 2, direction SERVER) is
  decoded, discarded, and a new **`SourceAudioPacket`** (UDP id 3, direction CLIENT) is serialised:
  `sequenceNumber` copied verbatim, `data` **copied by reference (same `byte[]`, no re-encode, no
  re-encrypt)**, `sourceId` = the speaker's per-player source UUID, `sourceState` = the source's
  atomic state, `distance` = the clamped activation distance
  (`ProximityServerActivationHelper.kt:82-87`, `VoiceServerProximitySource.kt:48`,
  `SourceAudioPacket.java:22-36`).
  The bytes are already AES-encrypted by the sender using the single server-wide key delivered in
  `ConfigPacket` (`client/.../VoiceAudioCapture.java:385-392` encrypts before
  `new PlayerAudioPacket(...)` `:397-413`; key: `VoiceTcpServerConnectionManager.java:109-125`,
  `BaseVoiceServer.java:282-295`), so relaying the ciphertext verbatim is correct — there is no
  per-recipient key.
* **Who receives it.** Exactly `getListeners(distance)` (§4). All are *registered UDP connections*
  from `VoiceUdpServerConnectionManager.getConnections()` = `connectionByPlayerId.values()`
  (`VoiceUdpServerConnectionManager.java:114-117`, map `:34`, distinct from `:33`).
* **Which secret.** Each recipient's **own** secret (`NettyUdpServerConnection.java:41` → `:83`).
* **Which timestamp.** A fresh `System.currentTimeMillis()` written per recipient at
  `PacketUdpCodec.java:56`. The inbound timestamp is decoded into `PacketUdp.timestamp`
  (`PacketUdp.java:19`) and **never read anywhere in the repo** (`getTimestamp()` has zero call
  sites), so relay latency is not compensated and the timestamp is not propagated.
* **Re-encoded or forwarded?** Re-serialised per recipient (header + `SourceAudioPacket.write`).
  Only the audio payload bytes are shared. Contrast with the proxy path, which literally forwards the
  raw datagram with only bytes 5..20 (the secret) rewritten in place:
  `proxy/common/.../NettyUdpProxyConnection.java:129-143` + `replaceSecret` `:203-213` and
  `PacketUdpCodec.replaceSecret` `:37-43` (`System.arraycopy(..., 0, data, 5, 16)`).
* **Is the sender excluded?** Yes, structurally: `VoiceServerPlayerSource` installs
  `filterSelf { player != this.player }` (`server/common/.../audio/source/VoiceServerPlayerSource.kt:42-45`, `:50-51`)
  and `matchFilters` (`BaseServerAudioSource.java:118-129`) skips any candidate failing a filter.
  Instead of the audio, the speaker receives, on their own UDP connection, one `SelfAudioInfoPacket`
  per relayed frame (see §7), plus `SelfSourceInfoPacket` on the control plane when their active
  activation changes.

Because the fan-out happens synchronously inside `channelRead0`, all recipients are served on the
Netty event-loop thread of the socket that received the speaker's datagram; there is no queue, no
thread hand-off, and no coalescing beyond `FlushConsolidationHandler(256, true)`
(`NettyUdpServer.java:86`).

---

## 4. How recipients are chosen

`VoiceServerProximitySource.getListeners(distance)` — `VoiceServerProximitySource.kt:85-107`:

```kotlin
val listenersDistance = min(distance + voiceServer.config!!.voice().maxExtraAudioBroadcastDistance(), // :87
                            distance * DISTANCE_MULTIPLIER)                                          // :88, =2 (:111)
val distanceSquared = (listenersDistance * listenersDistance).toDouble()                                // :91
return Iterable {
    val sourcePosition = position                    // :94  evaluated once per iteration
    val playerPosition = ServerPos3d()               // :95  reused scratch buffer
    voiceServer.udpConnectionManager.connections     // :97  connectionByPlayerId.values()
        .asSequence()
        .filter { matchFilters(it.player) }          // :99
        .filter { connection ->
            connection.player.instance.getServerPosition(playerPosition)                                // :101
            sourcePosition.world == playerPosition.world &&                                             // :103
                sourcePosition.distanceSquared(playerPosition) <= distanceSquared                       // :103
        }
        .iterator()
}
```

* `matchFilters` (`server-proxy-common/.../audio/source/BaseServerAudioSource.java:118-129`):
  first `if (player.isVoiceDisabled()) return false` `:122`, then every predicate added via
  `addFilter` `:124-126`.
* `VoiceServerPlayerSource` adds exactly two `:42-45`:
  `filterSelf` (`player != this.player`, `:50-51`) and
  `filterVanish` (`(player as VoiceServerPlayer).instance.canSee(this.player.instance)`, `:53-54`).
* No distance attenuation is applied server-side. The **broadcast radius** is
  `min(d, d + maxExtraAudioBroadcastDistance) ` with `maxExtraAudioBroadcastDistance = 16` by default
  (`server/common/.../config/VoiceServerConfig.java:137-138`), i.e. `min(d+16, 2d)`; the client is the
  one that attenuates by volume using the packet's `distance` field.
* Positions come from the Minecraft server at relay time; nothing is cached (`:94`, `:101`).

`distance` and `stereo` on `PlayerAudioPacket` (`protocol/.../udp/serverbound/PlayerAudioPacket.java:21-26`,
read `:36-43`):
* `distance` (short) — the client's chosen activation distance
  (`client/.../VoiceAudioCapture.java:409` uses `activation.getDistance()`), clamped by the server via
  `calculateAllowedDistance` (`VoiceServerActivationManager.kt:132`), and then used **both** as the
  `SourceAudioPacket.distance` value and as the radius input to `getListeners`.
* `stereo` (boolean) — never enters the wire packet. It only drives
  `source.setStereo(isStereo && activation.isStereoSupported)` (`ProximityServerActivationHelper.kt:120-122`),
  which, on a change, marks the source dirty and **increments source state by 10**
  (`BaseServerAudioSource.java:63-72`), causing the next frame to also emit a fresh TCP
  `SourceInfoPacket` (`VoiceServerProximitySource.kt:51-54`). The client uses that to pick a stereo
  decoder. The built-in proximity activation declares `setStereoSupported(false)`
  (`ProximityServerActivation.kt:36`), so player proximity audio is always mono.

### Static / entity / broadcast / direct variants

* `VoiceServerPlayerSource.position` = `player.instance.getServerPosition()` — fresh call per frame
  (`VoiceServerPlayerSource.kt:25-26`).
* `VoiceServerEntitySource.position` = `entity.getServerPosition()`
  (`server/common/.../audio/source/VoiceServerEntitySource.kt:25-26`); `resolveSourceInfo()`
  hops to the entity's task scheduler `:42-43`. No extra filters, so `matchFilters` only applies the
  voice-disabled check.
* `VoiceServerStaticSource.position` = a settable `ServerPos3d` (`VoiceServerStaticSource.kt:25-34`);
  `set` marks the source dirty when the value changes `:30-33`. No filters.
* `VoiceServerBroadcastSource` (a **direct** source, i.e. *not* proximity-attenuated): listeners are
  `players` if set, else every connection, and only `matchFilters` applies
  (`server-proxy-common/.../audio/source/VoiceServerBroadcastSource.kt:24-33`). Its audio packets are
  built by `BaseServerDirectSource.sendAudioFrame` with `distance = 0`
  (`api/server-proxy-common/.../audio/source/BaseServerDirectSource.kt:83-89`) and fanned out to
  `getListeners()` (`VoiceBaseServerDirectSource.java:123-140`, loop `:135-137`). Direct sources also
  emit a `SourceInfoPacket` (TCP) when dirty via `updateSourceInfo()` `:165-167`.
* `VoiceUdpServerConnectionManager.broadcast(packet, filter)` `:140-146` exists as a raw,
  unconditional fan-out of any clientbound UDP packet; it does **not** consult `matchFilters`,
  voice-disabled, distance, or position. It is not used for audio in the core.
* The player-set machinery (`VoiceServerPlayerSetManager`) is metadata for
  `ConfigPacket`/`SourceLinePlayersListPacket` only (`VoiceBaseServerSourceLine.kt:40-56`,
  `VoiceTcpServerConnectionManager.java:135-139`); it never filters the relay.
* Addons may add filters, replace the distance via `ProximityServerActivationHelper.DistanceSupplier`
  (`ProximityServerActivationHelper.kt:126-131`), or suppress a packet with
  `ServerSourceAudioPacketEvent.Result.HANDLED` / cancellation (`VoiceServerProximitySource.kt:43-45`).

---

## 5. Recipient-side prerequisites, and audio that arrives too early (answers Q4 + part of Q5)

### What must exist before audio is relayed

1. **Kontrol-plane handshake.** The client sends `PlayerInfoPacket` (TCP id 10, `PacketTcpCodec.java:59`)
   on `plasmo:voice/v2`; `PlayerChannelHandler.handle(PlayerInfoPacket)` `:66-104` requires a
   matching major version and the configured minimum, parses the RSA public key `:89-92`, stores
   voice-disabled / mic-muted / mod version `:99-101`, and finally calls
   `tcpConnections.connect(player)` `:103` → `VoiceTcpServerConnectionManager.connect` `:52-82`,
   which mints/returns the secret (`getSecretByPlayerId`, `:56-57`) and sends `ConnectionPacket(secret, ip, port)`
   `:75-79` over the control plane. **Only then does `playerIdBySecret` contain the secret.**
2. **UDP registration.** The player's first datagram with that secret is not in
   `connectionBySecret`, so `NettyPacketHandler.java:47-66` resolves the player and creates +
   `addConnection`s a `NettyUdpServerConnection` `:53-66`. `addConnection`
   (`VoiceUdpServerConnectionManager.java:62-75`) fires `UdpClientConnectEvent`, inserts into both
   maps `:67-68`, disconnects any previous connection for the same secret/player `:70-71`, and fires
   `UdpClientConnectedEvent` `:74` → `BaseVoicePlayerManager.onClientConnect` sets
   `player.connected = true` `:79-83`, which is what `hasVoiceChat()` reports
   (`BaseVoicePlayer.java:63-66`). `sendConfigInfo` / `sendPlayerList` /
   `broadcastPlayerInfoUpdate` are then pushed `NettyPacketHandler.java:68-71`.
3. **Activation knowledge** is server-side: the built-in proximity activation is registered on every
   config load (`BaseVoiceServer.java:304-305` → `ProximityServerActivation.register` `:16-49`) with
   id `UUID.nameUUIDFromBytes(("proximity" + "_activation").getBytes(UTF_8))`
   (`VoiceActivation.java:29-34`) and distances from config (`ProximityServerActivation.kt:32-33`;
   defaults `[8,16,32]`, default 16, `VoiceServerConfig.java:252-257`). The client learns it from
   `ConfigPacket` (`VoiceTcpServerConnectionManager.java:140-145`).

### Audio that arrives too early

* **Unknown secret** (no `PlayerInfoPacket` yet, or a reconnecting player whose secret was dropped):
  `NettyPacketHandler.java:47-48` finds no player id and **returns silently**. No reply, no log, no
  registration. This is the security boundary: never register a secret that was not minted.
* **Known secret, no connection yet**: the datagram is consumed to *create* the connection; the
  `handlePacket` call lives only in the `.map(...)` branch (`NettyPacketHandler.java:30-45`), which was
  not taken. So **the very first datagram from a player is never relayed** — it only registers them.
  (`PingPacket` is the normal first datagram; a first-frame `PlayerAudioPacket` is silently lost.)
* **Before `PlayerActivationDistancesPacket`** (TCP id 13, `PlayerChannelHandler.java:128-137`): no
  effect on relay. The stored `distanceByActivationId` / `getActivationDistanceById`
  (`BaseVoicePlayer.java:47`, `:78-80`) is consulted by addons only; the server's clamp uses the
  distance carried by each `PlayerAudioPacket`.
* **Before/without `ConfigPacket`**: irrelevant to the server — the payload is opaque ciphertext.
* **Client muted**: `microphoneMuted` is set from `PlayerInfoPacket` `:100` and `PlayerStatePacket`
  `:112-113`, and both `NettyUdpServerConnection.java:127` and `PlayerChannelHandler.java:143` drop
  audio on it.
* **Voice disabled**: does not stop the *speaker's* audio server-side (there is no such check in
  `handle(PlayerAudioPacket)`); it only makes that player ineligible as a *recipient* via
  `matchFilters` (`BaseServerAudioSource.java:122`).
* **UDP timeout**: `NettyUdpKeepAlive.tick` runs every **100 ms** (`:28-33`) and removes a connection
  when `now - lastReceivedPacketTimestamp > keepAliveTimeoutMs` (default 15 s,
  `VoiceServerConfig.java:147-152`) with reason `TIMED_OUT`, also sending a TCP `PlayerInfoRequestPacket`
  (`:45-48`). Otherwise it sends a `PingPacket` roughly every 1.0–2.5 s — the check is
  `now - sentKeepAlive >= 1000` but `sentKeepAlive` is set to `now + 1500 + rand(1500)`, so the
  effective period is ~2.5–4 s (`:49-52`). Removal clears the secret↔player mappings and fires
  `UdpClientDisconnectedEvent` (`VoiceUdpServerConnectionManager.java:124-138`), which also resets the
  voice player (`BaseVoicePlayerManager.java:85-89` → `BaseVoicePlayer.reset()` `:128-133`).

---

## 6. Which packets travel where (answers Q5)

TCP/control-plane ids are assigned in `protocol/src/main/java/su/plo/voice/proto/packets/tcp/PacketTcpCodec.java:46-81`
(increment order). The control plane is the Minecraft plugin-message channel `plasmo:voice/v2`
(`BaseVoiceServer.java:77`, received at `ServerChannelHandler.java:34-51`).

| packet | plane | id | sender → receiver | triggered by |
|---|---|---|---|---|
| `PlayerAudioPacket` | **UDP** | 2 | client → server | capture, once per ~20 ms frame (`VoiceAudioCapture.java:397-413`) |
| `SourceAudioPacket` | **UDP** | 3 | server → each listener connection | relay, once per frame per recipient (`VoiceServerProximitySource.kt:56-58`) |
| `SelfAudioInfoPacket` | **UDP** | 4 | server → the **speaker's own** connection | every relayed frame (`SelfActivationHelper.kt:108-116`) |
| `SourceInfoRequestPacket` | TCP | 15 | client → server | unknown `sourceId`, or `sourceState` mismatch (`NettyUdpClientHandler.java:89-91`; `VoiceClientSourceManager.kt:162-170`) |
| `SourceInfoPacket` | TCP | 16 | server → player(s) | reply to a request (`PlayerChannelHandler.java:148-173`), or a dirty source broadcast (`VoiceServerProximitySource.kt:51-54`) |
| `SelfSourceInfoPacket` | TCP | 17 | server → the speaker | active activation changed, or own source info updated (`SelfActivationHelper.kt:83-106`, `:131-135`) |
| `SourceAudioEndPacket` | TCP | 18 | server → listeners (and an echo to the speaker) | `PlayerAudioEndPacket` → activation end (`ProximityServerActivationHelper.kt:99-111`) |
| `PlayerAudioEndPacket` | TCP | 12 | client → server | capture release (`VoiceAudioCapture.java:415-428`) |
| `PlayerActivationDistancesPacket` | TCP | 13 | client → server | client activation-distance settings (`PlayerChannelHandler.java:128-137`) |
| `PlayerStatePacket` / `PlayerInfoPacket` | TCP | 11 / 10 | client → server | mute/voice-disabled changes / handshake (`PlayerChannelHandler.java:106-126`, `:66-104`) |
| `ConnectionPacket` | TCP | 1 | server → client | handshake reply, carries the secret (`VoiceTcpServerConnectionManager.java:75-79`) |
| `ConfigPacket` | TCP | 3 | server → client | registration burst (`NettyPacketHandler.java:68`), reload (`BaseVoiceServer.java:429-436`) |

Consequences worth stating explicitly:

* **`SourceInfoRequest` → `SourceInfoPacket` never touch UDP.** `SourceInfoPacket` is sent as a plugin
  message via `player.sendPacket` (`VoiceServerPlayerEntity.java:31-38` → `instance.sendPacket(CHANNEL_STRING, encoded)`).
  For a player source it is answered only if the player passes the source's filters
  (`PlayerChannelHandler.java:160-168`).
* **`SourceAudioEndPacket` is also TCP**, sent to listener *players* rather than UDP connections
  (`VoiceServerProximitySource.kt:68-70`), and additionally echoed to the speaker by
  `SelfActivationHelper.onSourceSendPacket` (`:136-140`). It can race an in-flight UDP audio packet —
  the client tolerates this via the jitter buffer and its sequence check (§8).
* **`SelfSourceInfoPacket` carries `sequenceNumber = -1`** and the speaker's `playerId` and
  `lastActivationId` (`SelfActivationHelper.kt:96-105`, `SelfSourceInfo.java:22-45`). It exists so the
  client can build its *own* source locally (self-monitoring/overlay) without a server relay.

### `SelfAudioInfoPacket` details

`SelfActivationHelper.sendAudioInfo` (`server-proxy-common/.../audio/capture/SelfActivationHelper.kt:46-81`),
called from `onSourceAudioPacket` at `@EventSubscribe(priority = HIGHEST)` `:108-116`, i.e. **after**
lower-priority `ServerSourceAudioPacketEvent` listeners and **before** the fan-out:

* records `sourceIdToPlayerId` / `playerIdToSourceIds` `:60-63`;
* if the speaker's last activation id changed, emits a TCP `SelfSourceInfoPacket` `:65-68`;
* sends `SelfAudioInfoPacket(source.id, packet.sequenceNumber, if (dataChanged) packet.data else null, packet.distance)`
  to the speaker's UDP connection `:70-80`.
* `dataChanged` = `playerPacket.data.size != sourcePacket.data.size`
  (`:51`) — a size comparison only, no content compare. On the stock proximity path the arrays are the
  same object, so the field is always `null`.

---

## 7. Sequence numbers, ends, out-of-order packets, buffering (answer Q6)

* **Sequence numbers are the sender's, passed through unchanged for audio.** The client keeps one
  counter per activation, incremented per frame and per end packet
  (`VoiceAudioCapture.java:430-434`, used `:406`, `:423`). The server copies
  `packet.sequenceNumber` into the `SourceAudioPacket` (`ProximityServerActivationHelper.kt:83`) and
  into the `SourceAudioEndPacket` (`:103`) verbatim; it never renumbers, never drops duplicates, and
  never reorders deliberately. Monotonicity is the sender's responsibility.
* **The server keeps exactly two pieces of sequence state per speaker**, both for activation
  bookkeeping only, not for audio:
  `BaseVoicePlayer.lastActivationSequenceNumber` (`BaseVoicePlayer.java:51`), written on audio end
  (`VoiceServerActivationManager.kt:194`) and consulted on the next start `:143-147` (start if
  `seq > last` or `|seq - last| > 10`); and `activeActivations` (`:49`), the "is this stream open" set.
  `reset()` clears both when the UDP connection goes away (`:128-133`).
* **No server-side jitter buffer, no PLC, no reordering, no retransmit.** A frame is relayed
  immediately or dropped. All buffering is client-side: `BaseClientAudioSource` offers packets to an
  `Adaptive`/`Static` `JitterBuffer` (`client/.../BaseClientAudioSource.kt:214-218`), synthesises
  packet-loss concealment for gaps (`:295`, `:329-355`) and closes the source after a timeout of
  inactivity (`:277-284`).
* **Late/duplicate packets are filtered by the client, not the server**
  (`client/.../BaseClientAudioSource.kt:300-327`): drop if the source-state distance is `>= 10`, drop
  if `seq <= lastSequenceNumber` unless the backward jump exceeds `SEQUENCE_RESTART_THRESHOLD`
  (a genuine sender restart). `Byte.diff` is the wrapping 8-bit distance
  (`client/.../extension/Byte.kt:3-8`). Because dropping relies on `sourceState`, a reimplementation
  must reproduce the source-state arithmetic: initial `1` (`BaseServerAudioSource.java:40`), `+1` for
  name/icon changes (`:75-90`), `+10` for stereo changes (`:63-72`), wrapping at `Byte.MAX_VALUE`
  into `Byte.MIN_VALUE + remainder` (`:131-142`).
* **Client-side gap filling for unknown sources**: a `SourceAudioPacket` for an unknown `sourceId` is
  buffered (5 s max) and drained once the source is created from a `SourceInfoPacket`
  (`NettyUdpClientHandler.java:93-95`; `VoiceClientSourceManager.kt:202-213`, `:110-160`, timeout
  `:226-231`, constant `:258`).
* **Activation end is control-plane** (§6) and is the only stream-terminating signal; there is no UDP
  "end" packet. `SourceAudioEndPacket` sets the client's `lastSequenceNumber` and lets the jitter
  buffer drain (`BaseClientAudioSource.kt:224-229`, `:398-399`).

---

## 8. What cannot be replicated without the Minecraft server (answer Q7)

State the Rust side must be *given* by the host, because upstream reads it out of `su.plo.slib`
(`McServerPlayer`, `McPlayer`, `ServerPos3d`) which is **not** part of this checkout — only call
sites exist here:

1. **Player identity**: UUID, name, and `GameProfile` — needed for `PlayerSourceInfo`
   (`VoiceServerPlayerSource.kt:28-40`, `:39` `player.createPlayerInfo()` →
   `VoiceServerPlayerEntity.java:41-56`) and for `PlayerListPacket`/`PlayerInfoUpdatePacket`.
2. **Positions at relay time.** `connection.player.instance.getServerPosition(playerPosition)`
   (`VoiceServerProximitySource.kt:101`) and `player.instance.getServerPosition()`
   (`VoiceServerPlayerSource.kt:26`) are the slib API. `ServerPos3d.distanceSquared` and
   `world` identity semantics are **not verifiable from this checkout**; the code only requires
   "same world" plus a squared 3-D distance comparison against `min(d+16, 2d)²`. A reimplementation
   must be handed `world id + (x,y,z)` doubles for every online player, refreshed at least once per
   audio frame, and must implement the same `min(d + maxExtra, d * 2)` radius (default 16,
   `VoiceServerConfig.java:137-138`).
3. **Visibility / vanish**: `canSee` (both as a source filter
   `VoiceServerPlayerSource.kt:53-54` and as a TCP broadcast filter
   `VoiceTcpServerConnectionManager.java:163`, `:184-186`).
4. **Permissions**: `hasPermission` for activation permissions
   (`VoiceServerActivation.kt:64-65`) plus the wildcard `pv.activation.*` broadcast path
   (`VoiceServerActivationManager.kt:219-237`, constant `:351`), the proximity permission
   `pv.activation.proximity` (`ProximityServerActivation.kt:28`, `server/.../command/Permission`), and
   `getSynchronizedPermissions` for `ConfigPacket` (`VoiceTcpServerConnectionManager.java:188-197`).
   Without a permission provider the whole activation gate collapses to "allowed".
5. **Mute storage**: `getMuteManager().getMute(uuid).isPresent()` gates both audio
   (`NettyUdpServerConnection.java:126`) and audio-end (`PlayerChannelHandler.java:142`). Its backing
   store is a JSON file on the server's config folder (`BaseVoiceServer.java:162-174`).
6. **Third-party / addon behaviour**: activation `Requirements`
   (`ServerActivation.java:230-249`, checked `VoiceServerActivationManager.kt:141`, `:191`),
   extra activation listeners, `DistanceSupplier`, source filters, and events that can cancel or
   swallow packets (`PlayerSpeakEvent`, `ServerSourceAudioPacketEvent`, `ServerSourcePacketEvent`,
   `UdpPacketReceivedEvent`, `UdpPacketSendEvent`). Any of these can change the recipient set or drop a
   frame; a reimplementation can only reproduce the *built-in* behaviour.
7. **Control plane itself.** Upstream's control plane is the Minecraft plugin-message channel
   `plasmo:voice/v2` (`BaseVoiceServer.java:77`) with a **second** channel
   `plasmo:voice/v2/installed` (`:78`). Pumpkin has IPC instead; equivalently, the `ConnectionPacket`
   secret delivery, `ConfigPacket` (AES key + activations + source lines), `PlayerInfoRequestPacket`,
   `SourceInfoPacket` replies and `SourceAudioEndPacket` all need a host-mediated transport, because
   none of them are UDP.
8. **Encode/decode details that stay opaque**: the audio payload codec (Opus via
   `createOpusEncoder`/`createOpusDecoder`, `BaseVoiceServer.java:389-417`) and the AES session cipher
   (`AES/CBC/PKCS5Padding`, `:317`). The server never decodes or re-encrypts relayed audio, so a
   reimplementation only needs to copy the bytes — but it *does* need the sample-rate/MTU/bitrate
   config values to build `ConfigPacket` (`VoiceTcpServerConnectionManager.java:104-133`).

---

## 9. Copy-paste checklist for the Rust relay

1. Parse header: magic `0x4e9004e9`, id byte, secret (2 BE longs), timestamp (BE long); body lazily.
2. Drop on bad magic / unknown id / unknown secret. Unknown secret ⇒ **silent drop**, never register.
3. Known secret + no connection ⇒ register (channel = receiving socket, remote = datagram sender),
   push `ConfigPacket`/`PlayerListPacket`/`PlayerInfoUpdatePacket`, and **drop this datagram**.
4. Known connection + sender address differs ⇒ `setRemoteAddress` (follow, do not duplicate).
5. `PingPacket` ⇒ refresh keep-alive only, never reply. `CustomPacket` ⇒ ignore.
6. `PlayerAudioPacket` ⇒ drop if muted (server mute or client mic-muted); resolve activation; check
   permission; clamp distance (`checkDistance` ? distance : defaultDistance); check requirements;
   activation-start bookkeeping; build a **new** `SourceAudioPacket` with the per-player source UUID,
   the source state, the clamped distance and the *same payload bytes*; send the speaker a
   `SelfAudioInfoPacket`; fan out to
   `all distinct connections where matchFilters(player) [voice-enabled ∧ not self ∧ canSee] ∧ same world ∧ dist² ≤ min(d+16, 2d)²`,
   encoding one datagram per recipient with **that recipient's secret** and a **fresh**
   `System.currentTimeMillis()`.
7. On `PlayerAudioEndPacket` (control plane): re-run permission / distance / requirements, remove the
   activation from `activeActivations`, store `lastActivationSequenceNumber`, then send
   `SourceAudioEndPacket` over the **control plane** to the same listener set plus the speaker.
8. Keep per-speaker-source state (stereo flag ⇒ `+10` on change, name/icon ⇒ `+1`, wrap as
   `Byte.MAX_VALUE → Byte.MIN_VALUE + remainder`), because clients drop frames on `state diff >= 10`.
9. Keep-alive sweep every 100 ms: timeout after `keepAliveTimeoutMs` of silence; otherwise send a ping
   when `now >= sentKeepAlive`, then set `sentKeepAlive = now + 1500 + rand(0..1499)`.
