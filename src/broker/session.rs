//! Per-connection state machine: reads framed commands off the socket and
//! drives the [`SteamService`] accordingly.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddrV4, TcpStream},
};

use steamworks::SteamId;

use crate::{args::Args, BrokerError};

use super::{
    appid_for_gamedir,
    protocol::{self, PlayerField},
    steam::{PlayerSnapshot, SteamService},
    FALLBACK_APP_ID,
};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Active,
    TicketRequested {
        challenge: i32,
        serveradr: SocketAddrV4,
    },
}

pub enum SessionResult {
    Continue,
    Terminate,
}

enum ReadOutcome {
    DataOrIdle,
    Closed,
}

pub struct Session<'a> {
    pub(super) stream: TcpStream,
    pub(super) rx_buffer: Vec<u8>,
    pub(super) state: State,
    pub(super) steam: &'a mut Option<SteamService>,
    pub(super) pending_player_replies: Vec<SteamId>,
}

impl Session<'_> {
    pub fn run(&mut self) -> Result<SessionResult, BrokerError> {
        loop {
            if let Some(steam) = self.steam.as_mut() {
                steam.client.run_callbacks();
                steam.process_pending_avatars();
            }

            self.flush_ready_player_replies()?;

            if let ReadOutcome::Closed = self.read_chunk()? {
                println!("connection closed by peer");
                self.cleanup_active_ticket();
                if self.steam.is_some() {
                    println!("steam was initialized, treating disconnect as sb_terminate");
                    return Ok(SessionResult::Terminate);
                }
                return Ok(SessionResult::Continue);
            }

            while let Some(payload) = self.try_parse_frame()? {
                if let SessionResult::Terminate = self.handle_command(&payload)? {
                    return Ok(SessionResult::Terminate);
                }
            }
        }
    }

    /// Sends a `sb_playerx` response for any queued `sb_get_player` request
    /// whose avatar fetch has since finished (or given up).
    fn flush_ready_player_replies(&mut self) -> Result<(), BrokerError> {
        if self.pending_player_replies.is_empty() {
            return Ok(());
        }

        let Some(steam) = self.steam.as_mut() else {
            return Ok(());
        };

        let mut still_pending = Vec::with_capacity(self.pending_player_replies.len());
        let mut ready_snapshots = Vec::new();

        for steamid in self.pending_player_replies.drain(..) {
            match steam.try_finalize_player_snapshot(steamid) {
                Some(snapshot) => ready_snapshots.push(snapshot),
                None => still_pending.push(steamid),
            }
        }

        self.pending_player_replies = still_pending;

        for snapshot in ready_snapshots {
            self.send_player_snapshot_response(snapshot)?;
        }

        Ok(())
    }

    fn read_chunk(&mut self) -> Result<ReadOutcome, BrokerError> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(0) => Ok(ReadOutcome::Closed),
            Ok(n) => {
                if self.rx_buffer.len() + n > protocol::MAX_PAYLOAD_SIZE * 2 {
                    return Err(BrokerError::Custom("rx buffer overflow"));
                }
                self.rx_buffer.extend_from_slice(&buf[..n]);
                Ok(ReadOutcome::DataOrIdle)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(ReadOutcome::DataOrIdle)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn try_parse_frame(&mut self) -> Result<Option<Vec<u8>>, BrokerError> {
        let Some((consumed, payload)) = protocol::parse_frame(&self.rx_buffer)? else {
            return Ok(None);
        };
        self.rx_buffer.drain(..consumed);
        Ok(Some(payload))
    }

    fn handle_command(&mut self, payload: &[u8]) -> Result<SessionResult, BrokerError> {
        let text = std::str::from_utf8(payload)?;
        let mut parts = text.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").as_bytes();

        println!("got {cmd}");

        match cmd {
            "sb_gamedir" => self.handle_gamedir(rest)?,
            "sb_connect" => self.handle_connect(rest)?,
            "sb_disconnect" => self.handle_disconnect(rest)?,
            "sb_get_player" => self.handle_get_player(rest)?,
            "sb_terminate" => {
                self.cleanup_active_ticket();
                return Ok(SessionResult::Terminate);
            }
            _ => return Err(BrokerError::Custom("unknown command")),
        }

        Ok(SessionResult::Continue)
    }

    fn handle_gamedir(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        if self.state != State::Idle {
            return Err(BrokerError::Custom("session already active"));
        }

        let mut args = Args::new(args)?;
        let gamedir = args.next().ok_or(BrokerError::Missing("gamedir"))?;
        let app_id = appid_for_gamedir(gamedir).unwrap_or_else(|| {
            println!(
                "warning: unknown gamedir \"{gamedir}\", falling back to AppID {FALLBACK_APP_ID}"
            );
            FALLBACK_APP_ID
        });
        println!("activating session for gamedir \"{gamedir}\" (AppID {app_id})");

        match self.steam.as_ref() {
            Some(existing) if existing.app_id != app_id => {
                // Steamworks SDK can't be re-initialized under a different AppID in-process.
                return Err(BrokerError::Custom(
                    "broker already initialized with a different AppID; sb_terminate first",
                ));
            }
            Some(_) => {}
            None => {
                *self.steam = Some(SteamService::new(app_id)?);
            }
        }

        self.state = State::Active;
        Ok(())
    }

    fn handle_connect(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        if self.state != State::Active {
            return Err(BrokerError::Custom("session not active"));
        }

        let mut args = Args::new(args)?;
        println!("handle_connect: {}", args.as_str());

        // sb_connect <ip:port> <server_steamid> <secure 0|1> <challenge>
        let serveradr: SocketAddrV4 = args.parse("ip addr")?;
        let game_server_steam_id: u64 = args.parse("steam id")?;
        let secure = args.parse::<i32>("secure")? != 0;
        let challenge: i32 = args.parse("challenge")?;

        let steam = self
            .steam
            .as_ref()
            .expect("steam service initialized in active state");

        println!(
            "initiate_game_connection: {serveradr} {game_server_steam_id} {secure} {challenge}"
        );
        #[allow(deprecated)]
        let ticket = steam
            .user
            .initiate_game_connection(SteamId::from_raw(game_server_steam_id), serveradr, secure)
            .ok_or(BrokerError::Custom("steam refused to issue auth ticket"))?;

        self.state = State::TicketRequested {
            challenge,
            serveradr,
        };

        println!("steam ticket size: {}, sending response", ticket.len());

        // payload: "sb_connect\n" + i32 challenge LE + u64 steamid LE + u32 size LE + ticket
        let steam_id = steam.user.steam_id().raw();
        let mut payload = Vec::with_capacity(
            protocol::CONNECT_RESPONSE_HEADER.len() + 4 + 8 + 4 + ticket.len(),
        );
        payload.extend_from_slice(protocol::CONNECT_RESPONSE_HEADER);
        payload.extend_from_slice(&challenge.to_le_bytes());
        payload.extend_from_slice(&steam_id.to_le_bytes());
        payload.extend_from_slice(&(ticket.len() as u32).to_le_bytes());
        payload.extend_from_slice(&ticket);

        self.send_frame(&payload)
    }

    fn handle_disconnect(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        let State::TicketRequested {
            challenge: requested,
            ..
        } = self.state
        else {
            return Err(BrokerError::Custom("no ticket requested"));
        };

        let mut args = Args::new(args)?;

        // sb_disconnect <ip:port> <challenge>
        let serveradr: SocketAddrV4 = args.parse("ip addr")?;
        let challenge: i32 = args.parse("challenge")?;

        if challenge != requested {
            return Err(BrokerError::Custom("challenge mismatch"));
        }

        let steam = self
            .steam
            .as_ref()
            .expect("steam service initialized in ticket state");
        #[allow(deprecated)]
        steam.user.terminate_game_connection(serveradr);

        self.state = State::Active;
        Ok(())
    }

    fn handle_get_player(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        if self.state == State::Idle {
            return Err(BrokerError::Custom("session not active"));
        }

        let mut args = Args::new(args)?;
        let steamid_raw: u64 = args.parse("steam id")?;
        let steamid = SteamId::from_raw(steamid_raw);

        self.steam
            .as_mut()
            .expect("steam service initialized when session active")
            .request_player_snapshot(steamid);

        if !self.pending_player_replies.contains(&steamid) {
            self.pending_player_replies.push(steamid);
        }

        Ok(())
    }

    fn send_player_snapshot_response(&mut self, snapshot: PlayerSnapshot) -> Result<(), BrokerError> {
        let mut payload = Vec::with_capacity(protocol::MAX_PAYLOAD_SIZE);
        payload.extend_from_slice(protocol::PLAYER_RESPONSE_HEADER);
        payload.extend_from_slice(&snapshot.steamid.raw().to_le_bytes());

        // Flags are backfilled once every field has been written.
        let flags_offset = payload.len();
        payload.extend_from_slice(&0u32.to_le_bytes());
        let mut flags = 0u32;

        if let Some(name) = snapshot.name.as_ref() {
            protocol::write_field(&mut payload, &mut flags, PlayerField::Name, name.as_bytes());
        }
        if let Some(avatar) = snapshot.avatar_small.as_ref() {
            protocol::write_field(&mut payload, &mut flags, PlayerField::AvatarSmall, avatar);
        }
        if let Some(avatar) = snapshot.avatar_medium.as_ref() {
            protocol::write_field(&mut payload, &mut flags, PlayerField::AvatarMedium, avatar);
        }
        if let Some(avatar) = snapshot.avatar_large.as_ref() {
            protocol::write_field(&mut payload, &mut flags, PlayerField::AvatarLarge, avatar);
        }

        protocol::write_field(
            &mut payload,
            &mut flags,
            PlayerField::Relationship,
            &[snapshot.relationship as u8],
        );

        if let Some(country) = snapshot.country.as_ref() {
            protocol::write_field(
                &mut payload,
                &mut flags,
                PlayerField::Country,
                country.as_bytes(),
            );
        }
        if let Some(game_app_id) = snapshot.game_app_id {
            protocol::write_field(
                &mut payload,
                &mut flags,
                PlayerField::Game,
                &game_app_id.to_le_bytes(),
            );
        }
        if let Some(rich_presence) = snapshot.rich_presence.as_ref() {
            let mut data = Vec::new();
            for (key, value) in rich_presence {
                if key.len() > u16::MAX as usize || value.len() > u16::MAX as usize {
                    continue;
                }
                data.extend_from_slice(&(key.len() as u16).to_le_bytes());
                data.extend_from_slice(key.as_bytes());
                data.extend_from_slice(&(value.len() as u16).to_le_bytes());
                data.extend_from_slice(value.as_bytes());
            }
            if !data.is_empty() {
                protocol::write_field(&mut payload, &mut flags, PlayerField::RichPresence, &data);
            }
        }
        if let Some(persona_state) = snapshot.persona_state {
            protocol::write_field(
                &mut payload,
                &mut flags,
                PlayerField::PersonaState,
                &[persona_state],
            );
        }

        payload[flags_offset..flags_offset + 4].copy_from_slice(&flags.to_le_bytes());

        println!(
            "sending player info: steamid={} flags=0x{:08x} payload={} B",
            snapshot.steamid.raw(),
            flags,
            payload.len()
        );

        self.send_frame(&payload)
    }

    fn send_frame(&mut self, payload: &[u8]) -> Result<(), BrokerError> {
        let frame = protocol::build_frame(payload)?;
        self.stream.write_all(&frame).map_err(BrokerError::Send)
    }

    fn cleanup_active_ticket(&mut self) {
        if let State::TicketRequested { serveradr, .. } = self.state {
            if let Some(steam) = self.steam.as_ref() {
                println!("cleaning up dangling ticket for {serveradr}");
                #[allow(deprecated)]
                steam.user.terminate_game_connection(serveradr);
            }
            self.state = State::Active;
        }
    }
}