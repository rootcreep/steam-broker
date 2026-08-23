//! Steamworks integration: avatar fetching and per-player snapshots.

use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    time::{Duration, Instant},
};

use png::{BitDepth, ColorType, Encoder};
use steamworks::{Client, FriendFlags, FriendState, Friends, SteamId, User};

use crate::BrokerError;

use super::protocol::Relationship;

const AVATAR_SIZE: u32 = 32;
const AVATAR_PENDING_TIMEOUT: Duration = Duration::from_millis(100); // 0.1s

fn encode_avatar_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, BrokerError> {
    let expected_len = (width * height * 4) as usize;
    if rgba.len() != expected_len {
        return Err(BrokerError::Custom("unexpected avatar buffer size"));
    }

    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(Cursor::new(&mut out), width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_compression(png::Compression::High);
        let mut writer = encoder
            .write_header()
            .map_err(|_| BrokerError::Custom("png header write failed"))?;
        writer
            .write_image_data(rgba)
            .map_err(|_| BrokerError::Custom("png data write failed"))?;
    }
    Ok(out)
}

fn persona_state_byte(state: FriendState) -> u8 {
    match state {
        FriendState::Offline => 0,
        FriendState::Online => 1,
        FriendState::Busy => 2,
        FriendState::Away => 3,
        FriendState::Snooze => 4,
        FriendState::LookingToTrade => 5,
        FriendState::LookingToPlay => 6,
        FriendState::Invisible => 7,
    }
}

pub enum AvatarFetchState {
    Ready(Vec<u8>),
    Pending,
    Unavailable,
}

/// A single steamid's avatar fetch, in exactly one state at a time.
/// `NotRequested` is the initial state for a freshly-captured player,
/// before anything has asked for their avatar yet.
enum AvatarState {
    NotRequested,
    Pending(Instant),
    Ready(Vec<u8>),
    Unavailable,
}

/// Everything about a player except their avatar fetch state — captured
/// synchronously from `Friends` whenever we (re)snapshot them.
#[derive(Clone)]
struct PlayerFacts {
    name: Option<String>,
    relationship: Relationship,
    game_app_id: Option<u32>,
    persona_state: Option<u8>,
    country: Option<String>,                     // reserved: always None today
    rich_presence: Option<Vec<(String, String)>>, // reserved: always None today
}

/// Everything tracked for one player: their avatar fetch (which arrives
/// asynchronously) alongside the facts captured about them (which don't).
struct PlayerEntry {
    avatar: AvatarState,
    facts: PlayerFacts,
}

/// A fully-assembled, wire-ready view of one player — built on demand from
/// a [`PlayerEntry`] plus whatever avatar bytes are currently available.
#[derive(Clone)]
pub struct PlayerSnapshot {
    pub steamid: SteamId,
    pub name: Option<String>,
    pub avatar_small: Option<Vec<u8>>,
    pub avatar_medium: Option<Vec<u8>>, // reserved: always None today (Steam's 64x64 avatar)
    pub avatar_large: Option<Vec<u8>>,  // reserved: always None today (Steam's 184x184 avatar)
    pub relationship: Relationship,
    pub game_app_id: Option<u32>,
    pub persona_state: Option<u8>,
    pub country: Option<String>,                     // reserved: always None today
    pub rich_presence: Option<Vec<(String, String)>>, // reserved: always None today
}

impl PlayerSnapshot {
    fn from_entry(steamid: SteamId, facts: &PlayerFacts, avatar_small: Option<Vec<u8>>) -> Self {
        Self {
            steamid,
            name: facts.name.clone(),
            avatar_small,
            avatar_medium: None,
            avatar_large: None,
            relationship: facts.relationship,
            game_app_id: facts.game_app_id,
            persona_state: facts.persona_state,
            country: facts.country.clone(),
            rich_presence: facts.rich_presence.clone(),
        }
    }
}

pub struct SteamService {
    pub(super) client: Client,
    pub(super) user: User,
    friends: Friends,
    players: HashMap<SteamId, PlayerEntry>,
    pub(super) app_id: u32,
}

impl SteamService {
    pub fn new(app_id: u32) -> Result<Self, BrokerError> {
        // Steamworks SDK picks AppID from steam_appid.txt in cwd at init time.
        fs::write("steam_appid.txt", app_id.to_string()).map_err(BrokerError::Io)?;

        println!("Initializing Steam with AppID {app_id}...");

        let client = Client::init()?;

        let utils = client.utils();
        println!("Utils:");
        println!("AppId: {:?}", utils.app_id());

        let user = client.user();
        println!("User:");
        println!("SteamID: {:?}", user.steam_id());

        let friends = client.friends();

        Ok(Self {
            client,
            user,
            friends,
            players: HashMap::new(),
            app_id,
        })
    }

    /// Sets the avatar-fetch state for a player that's already tracked.
    /// A no-op if no entry exists yet — shouldn't happen, since avatar
    /// polling is only ever driven after a snapshot has been captured.
    fn set_avatar_state(&mut self, steamid: SteamId, state: AvatarState) {
        if let Some(entry) = self.players.get_mut(&steamid) {
            entry.avatar = state;
        }
    }

    /// Checks on (or, the first time, kicks off) an avatar fetch for
    /// `steamid`.
    fn poll_avatar(&mut self, steamid: SteamId) -> AvatarFetchState {
        match self.players.get(&steamid).map(|entry| &entry.avatar) {
            Some(AvatarState::Ready(png)) => return AvatarFetchState::Ready(png.clone()),
            Some(AvatarState::Unavailable) => return AvatarFetchState::Unavailable,
            Some(AvatarState::Pending(started)) => {
                if started.elapsed() <= AVATAR_PENDING_TIMEOUT {
                    return AvatarFetchState::Pending;
                }
                println!("Avatar request timed out: {}", steamid.raw());
                self.set_avatar_state(steamid, AvatarState::Unavailable);
                return AvatarFetchState::Unavailable;
            }
            Some(AvatarState::NotRequested) | None => {}
        }

        println!("Requesting avatar: {}", steamid.raw());
        self.friends.request_user_information(steamid, false);
        self.set_avatar_state(steamid, AvatarState::Pending(Instant::now()));
        AvatarFetchState::Pending
    }

    /// Checks every currently-pending avatar fetch and, for whichever ones
    /// have arrived, encodes the PNG and refreshes that player's facts.
    pub fn process_pending_avatars(&mut self) {
        let pending: Vec<SteamId> = self
            .players
            .iter()
            .filter_map(|(id, entry)| matches!(entry.avatar, AvatarState::Pending(_)).then_some(*id))
            .collect();

        for steamid in pending {
            let friend = self.friends.get_friend(steamid);
            let Some(raw_rgba) = friend.small_avatar() else {
                continue;
            };

            match encode_avatar_png(&raw_rgba, AVATAR_SIZE, AVATAR_SIZE) {
                Ok(png_bytes) => {
                    println!(
                        "Avatar ready: {} (raw {} B -> png {} B)",
                        steamid.raw(),
                        raw_rgba.len(),
                        png_bytes.len()
                    );
                    self.set_avatar_state(steamid, AvatarState::Ready(png_bytes));
                    // Refresh the rest of their facts too, now that we have a
                    // reason to believe Steam's cached data is current.
                    self.capture_player_snapshot(steamid);
                }
                Err(e) => {
                    println!("Avatar encode failed for {}: {e}", steamid.raw());
                    self.set_avatar_state(steamid, AvatarState::Unavailable);
                }
            }
        }
    }

    /// (Re)captures the non-avatar facts for `steamid`. Creates a new
    /// entry (with `AvatarState::NotRequested`) if one doesn't exist yet;
    /// otherwise leaves the existing avatar state untouched.
    fn capture_player_snapshot(&mut self, steamid: SteamId) {
        let friend = self.friends.get_friend(steamid);

        let name = {
            let value = friend.name();
            (!value.is_empty()).then_some(value)
        };

        let relationship = if friend.has_friend(FriendFlags::IMMEDIATE) {
            Relationship::Friend
        } else if friend.has_friend(FriendFlags::BLOCKED) {
            Relationship::Blocked
        } else if friend.has_friend(FriendFlags::FRIENDSHIP_REQUESTED) {
            Relationship::FriendshipRequested
        } else if friend.has_friend(FriendFlags::REQUESTING_FRIENDSHIP) {
            Relationship::RequestingFriendship
        } else {
            Relationship::None
        };

        let persona_state = Some(persona_state_byte(friend.state()));
        let game_app_id = friend.game_played().map(|game| game.game.app_id().0);

        let facts = PlayerFacts {
            name,
            relationship,
            game_app_id,
            persona_state,
            country: None,
            rich_presence: None,
        };

        match self.players.get_mut(&steamid) {
            Some(entry) => entry.facts = facts,
            None => {
                self.players.insert(
                    steamid,
                    PlayerEntry {
                        avatar: AvatarState::NotRequested,
                        facts,
                    },
                );
            }
        }
    }

    /// Ensures a snapshot exists for `steamid` and (re)starts its avatar
    /// fetch. Follow up with [`Self::try_finalize_player_snapshot`] to
    /// check whether the avatar has arrived yet.
    pub fn request_player_snapshot(&mut self, steamid: SteamId) {
        if !self.players.contains_key(&steamid) {
            self.capture_player_snapshot(steamid);
        }
        let _ = self.poll_avatar(steamid);
    }

    /// Returns a wire-ready snapshot for the player once their avatar
    /// fetch is no longer pending (either it's ready, or it gave up) —
    /// `None` while still waiting.
    pub fn try_finalize_player_snapshot(&mut self, steamid: SteamId) -> Option<PlayerSnapshot> {
        let avatar_small = match self.poll_avatar(steamid) {
            AvatarFetchState::Pending => return None,
            AvatarFetchState::Ready(png) => Some(png),
            AvatarFetchState::Unavailable => None,
        };

        let entry = self.players.get(&steamid)?;
        Some(PlayerSnapshot::from_entry(steamid, &entry.facts, avatar_small))
    }
}