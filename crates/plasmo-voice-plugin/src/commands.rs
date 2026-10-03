//! The server-side mute administration commands — `/vmute`, `/vunmute`,
//! `/vmutelist` — mirroring upstream `voiceMuteCommand`, `voiceUnmuteCommand` and
//! `voiceMuteListCommand`, gated on the permission nodes of upstream
//! `Permissions.kt` (`pv.mute`, `pv.unmute`, `pv.mutelist`, all defaulting to any
//! operator).
//!
//! The three commands read and write [`crate::mute`]'s process-wide store and
//! persist every mutation to `mutes.toml` in the plugin data folder, so a server
//! restart keeps the mutes. Permission checks happen twice by design: the host
//! gates the whole command on the node handed to `Context::register_command`
//! (namespaced to `<plugin>:pv.mute`), and the handlers re-check the same node
//! with `CommandSender::has_permission` so a host version that ignores the
//! registration gate still denies non-operators.
//!
//! Not mirrored from upstream: the client-side duration suggestions
//! (`MuteDurationType.listSuggestions`), and the offline-profile path of
//! `UnmuteTargetsType` (see the `/vunmute` handler).

use pumpkin_plugin_api::command::{
    Arg, ArgumentType, Command, CommandError, CommandNode, CommandSender, ConsumedArgs, StringType,
};
use pumpkin_plugin_api::commands::CommandHandler;
use pumpkin_plugin_api::permission::{Permission, PermissionDefault, PermissionLevel};
use pumpkin_plugin_api::text::TextComponent;
use pumpkin_plugin_api::{Context, Server};
use uuid::Uuid;

use crate::channel::{player_uuid, wit_uuid};
use crate::mute;
use crate::server::now_ms;

/// Upstream `Permissions.kt` node for `/vmute` (`Permission.MUTE`).
const PERM_MUTE: &str = "pv.mute";
/// Upstream `Permissions.kt` node for `/vunmute` (`Permission.UNMUTE`).
const PERM_UNMUTE: &str = "pv.unmute";
/// Upstream `Permissions.kt` node for `/vmutelist` (`Permission.MUTE_LIST`).
const PERM_MUTE_LIST: &str = "pv.mutelist";

/// The full node the host actually checks: `register_command` namespaces a
/// permission without a colon as `<plugin name>:<node>`, so the node declared
/// through `Context::register_permission` and the one the handler re-checks must
/// use the same spelling.
fn permission_node(node: &str) -> String {
    format!("{}:{node}", crate::PLUGIN_ID)
}

/// Sends one plain chat line to `sender`, honouring the sender's feedback
/// preference (upstream's `sendFeedback`).
fn feedback(sender: &CommandSender, text: impl AsRef<str>) {
    if sender.should_receive_feedback() {
        sender.send_message(TextComponent::text(text.as_ref()));
    }
}

/// Broadcasts the muted-state change to every connected voice client after the
/// store has been touched, exactly like upstream's `broadcastPlayerInfoUpdate`
/// after a mute change. A client that is not in a voice call never hears about it.
///
/// Reused by the expiry sweep in `tick.rs`, which lifts mutes without a command.
pub(crate) fn broadcast_player_info(player_id: &Uuid) {
    use crate::runtime::with_runtime;

    with_runtime(|runtime| {
        let update = runtime
            .protocol()
            .and_then(|protocol| protocol.player_info_update(player_id));
        if let Some(update) = update {
            runtime.push_control(update);
        }
    });
}

/// The `/vmute` executor. One instance per terminal node of the command tree
/// (`targets` alone, `targets → duration`, `targets → duration → reason`); the
/// optional arguments show up as empty strings when they are absent, so a single
/// handler covers all three shapes.
#[derive(Clone)]
struct MuteHandler {
    folder: String,
}

impl MuteHandler {
    fn new(folder: String) -> Self {
        Self { folder }
    }
}

impl CommandHandler for MuteHandler {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        if sender.is_player() && !sender.has_permission(&server, &permission_node(PERM_MUTE)) {
            return Err(CommandError::PermissionDenied);
        }

        let players = match args.get_value("targets") {
            Arg::Players(players) => players,
            _ => {
                feedback(&sender, "/vmute <players> [duration] [reason]");
                return Ok(0);
            }
        };
        if players.is_empty() {
            feedback(&sender, "Player not found");
            return Ok(0);
        }

        let now = now_ms();
        let expiry = match args.get_value("duration") {
            Arg::Simple(duration) if !duration.is_empty() => {
                match mute::parse_mute_duration(&duration, now) {
                    Ok(expiry) => expiry,
                    Err(message) => {
                        feedback(&sender, message);
                        return Ok(0);
                    }
                }
            }
            // No duration → a permanent mute, like upstream's default.
            _ => None,
        };
        let reason = match args.get_value("reason") {
            Arg::Simple(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Arg::Msg(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            _ => None,
        };

        let muted_by = sender
            .as_player()
            .map(|player| player_uuid(&player.get_id()));

        // The muted player is told the notice upstream sends on `mute()` when it is
        // not silent; the wording comes from `pv.mutes.*` resolved to plain text (the
        // vanilla client has no `[server.pv.mutes]` scope of its own).
        let duration_text = match args.get_value("duration") {
            Arg::Simple(duration) if !duration.is_empty() => mute::format_mute_duration(&duration),
            _ => None,
        };
        let notice = mute::muted_notice(duration_text.as_deref(), reason.as_deref());

        for player in players {
            let player_id = player_uuid(&player.get_id());
            let name = player.get_name();
            if mute::store().is_muted(&player_id, now) {
                feedback(&sender, format!("{name} is already muted"));
                continue;
            }
            let muted_to_ms = expiry.unwrap_or(0);
            mute::store().mutate(player_id, muted_by, muted_to_ms, reason.clone(), now);
            if let Err(error) = mute::save(&self.folder) {
                tracing::warn!(%error, "could not persist the mute table");
                feedback(
                    &sender,
                    format!("muted {name} but could not persist: {error}"),
                );
                continue;
            }
            broadcast_player_info(&player_id);
            if let Some(player) = server.get_player_by_uuid(wit_uuid(player_id)) {
                player.send_system_message(TextComponent::text(&notice), false);
            }
            match expiry {
                None => feedback(&sender, format!("{name} muted permanently")),
                Some(to) => feedback(&sender, format!("{name} muted for {}s", (to - now) / 1000)),
            }
        }
        sender.set_success_count(1);
        Ok(0)
    }
}

/// The `/vunmute` executor. The target is the host's `players` argument (a
/// name, a UUID, or an `@`-selector), mirroring upstream `UnmuteTargetsType`.
/// Every resolved player is unmuted in turn; the table is persisted once per
/// mutation, exactly like `/vmute`.
struct UnmuteHandler {
    folder: String,
}

impl UnmuteHandler {
    fn new(folder: String) -> Self {
        Self { folder }
    }
}

impl CommandHandler for UnmuteHandler {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        if sender.is_player() && !sender.has_permission(&server, &permission_node(PERM_UNMUTE)) {
            return Err(CommandError::PermissionDenied);
        }

        let players = match args.get_value("targets") {
            Arg::Players(players) => players,
            _ => {
                feedback(&sender, "/vunmute <players>");
                return Ok(0);
            }
        };
        if players.is_empty() {
            // An `@a`-style selector that matches nobody resolves to an empty
            // list rather than a parse error, so this is the "no such player"
            // answer, the same as `/vmute` gives.
            feedback(&sender, "Player not found");
            return Ok(0);
        }

        for player in players {
            let player_id = player_uuid(&player.get_id());
            let name = player.get_name();
            match mute::store().unmute(&player_id) {
                Some(_) => {
                    if let Err(error) = mute::save(&self.folder) {
                        tracing::warn!(%error, "could not persist the mute table");
                        feedback(
                            &sender,
                            format!("unmuted {name} but could not persist: {error}"),
                        );
                        continue;
                    }
                    broadcast_player_info(&player_id);
                    player.send_system_message(TextComponent::text(&mute::unmuted_notice()), false);
                    feedback(&sender, format!("{name} unmuted"));
                }
                None => feedback(&sender, format!("{name} is not muted")),
            }
        }
        sender.set_success_count(1);
        Ok(0)
    }
}

/// The `/vmutelist` executor.
struct ListHandler;

impl CommandHandler for ListHandler {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        if sender.is_player() && !sender.has_permission(&server, &permission_node(PERM_MUTE_LIST)) {
            return Err(CommandError::PermissionDenied);
        }

        let now = now_ms();
        let store = mute::store();
        let mutes = store.active(now);
        if mutes.is_empty() {
            feedback(&sender, "No players are muted");
            return Ok(0);
        }
        feedback(&sender, format!("{} players are muted:", mutes.len()));
        for mute_info in mutes {
            let name = server
                .get_player_by_uuid(wit_uuid(mute_info.player_id))
                .map_or_else(
                    || format!("{} (offline)", mute_info.player_id),
                    |player| player.get_name(),
                );
            let expires = mute_info.describe(now);
            let reason = mute_info.reason.as_deref().unwrap_or("no reason");
            let by = mute_info.muted_by.map(|id| {
                server
                    .get_player_by_uuid(wit_uuid(id))
                    .map_or_else(|| id.to_string(), |player| player.get_name())
            });
            let line = match by {
                Some(by) => format!("{name} — muted {expires}, by {by}: {reason}"),
                None => format!("{name} — muted {expires}: {reason}"),
            };
            feedback(&sender, line);
        }
        Ok(0)
    }
}

/// Builds the `/vmute` tree: `vmute → targets → [duration → [reason]]`.
fn build_mute_command(folder: &str) -> Command {
    // Terminal node for `/vmute <targets> <duration> <reason>`.
    let reason = CommandNode::argument("reason", &ArgumentType::String(StringType::Greedy))
        .execute(MuteHandler::new(folder.to_string()));

    // `/vmute <targets> <duration>` (reason absent) and the full shape above.
    let duration = CommandNode::argument("duration", &ArgumentType::String(StringType::SingleWord))
        .then(reason)
        .execute(MuteHandler::new(folder.to_string()));

    // `/vmute <targets>` (duration absent → permanent) and the two longer shapes.
    let targets = CommandNode::argument("targets", &ArgumentType::Players)
        .then(duration)
        .execute(MuteHandler::new(folder.to_string()));

    Command::new(&["vmute".to_string()], "Mute players in the voice chat").then(targets)
}

/// Builds the `/vunmute` tree: `vunmute → targets`.
///
/// `targets` is the host's `players` argument, like `/vmute`: it accepts names,
/// UUIDs and `@`-selectors, so the selector half of upstream `UnmuteTargetsType`
/// is mirrored here. A plain UUID of a player who is **not** currently online
/// cannot be resolved by the host (it only knows online players) — upstream
/// resolves it through the profile service — so an offline unmute still has to
/// wait for the player, or be done by editing `mutes.toml`.
fn build_unmute_command(folder: &str) -> Command {
    let targets = CommandNode::argument("targets", &ArgumentType::Players)
        .execute(UnmuteHandler::new(folder.to_string()));

    Command::new(&["vunmute".to_string()], "Unmute players in the voice chat").then(targets)
}

/// Builds `/vmutelist`, which takes no arguments.
fn build_mute_list_command() -> Command {
    Command::new(&["vmutelist".to_string()], "List voice chat mutes").execute(ListHandler)
}

/// Registers the mute commands and their permission nodes.
///
/// Called from the WASI glue's `on_load`. The permission nodes are declared with
/// the same spelling the handlers check, so `Context::register_permission` and
/// the host's command requirement agree even under a host that derives the
/// requirement from the registered node rather than the command's.
pub fn register(context: &Context) -> Result<usize, String> {
    let folder = context.get_data_folder();

    // Load the persisted mute table into the store so `/vmutelist` and the
    // protocol's mute checks see mutes from before the restart.
    mute::init(&folder);

    for (node, description) in [
        (PERM_MUTE, "Allows muting players in the voice chat"),
        (PERM_UNMUTE, "Allows unmuting players in the voice chat"),
        (PERM_MUTE_LIST, "Allows listing voice chat mutes"),
    ] {
        let full = permission_node(node);
        if let Err(error) = context.register_permission(&Permission {
            node: full.clone(),
            description: description.to_string(),
            default: PermissionDefault::Op(PermissionLevel::One),
            children: Vec::new(),
        }) {
            // A duplicate registration (plugin reload) is benign — the node
            // already exists with the same default.
            tracing::warn!(%error, node = %full, "could not register the mute permission node");
        }
    }

    context.register_command(build_mute_command(&folder), PERM_MUTE);
    context.register_command(build_unmute_command(&folder), PERM_UNMUTE);
    context.register_command(build_mute_list_command(), PERM_MUTE_LIST);

    Ok(3)
}
