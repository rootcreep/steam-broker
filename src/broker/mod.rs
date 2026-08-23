mod protocol;
mod session;
mod steam;

use std::{
    fs,
    io::ErrorKind,
    net::TcpListener,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};

use crate::BrokerError;
use session::{Session, SessionResult, State};
use steam::SteamService;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FALLBACK_APP_ID: u32 = 70;

fn appid_for_gamedir(gamedir: &str) -> Option<u32> {
    match gamedir.to_ascii_lowercase().as_str() {
        "cstrike" => Some(10),       // Counter-Strike 1.6
        "tfc" => Some(20),           // Team Fortress Classic
        "dod" => Some(30),           // Day of Defeat
        "dmc" => Some(40),           // Deathmatch Classic
        "gearbox" => Some(50),       // Half-Life: Opposing Force
        "ricochet" => Some(60),      // Ricochet
        "valve" => Some(70),         // Half-Life
        "czero" => Some(80),         // Counter-Strike: Condition Zero
        "czeror" => Some(100),       // Counter-Strike: Condition Zero — Deleted Scenes
        "bshift" => Some(130),       // Half-Life: Blue Shift
        "cstrike_beta" => Some(150), // Counter-Strike 1.6 beta
        _ => None,
    }
}

pub struct Broker {
    listener: TcpListener,
    steam: Option<SteamService>,
    _scratch: ScratchDir,
}

impl Broker {
    pub fn new(addr: &str) -> Result<Self, BrokerError> {
        let scratch = ScratchDir::new()?;
        std::env::set_current_dir(scratch.path()).map_err(BrokerError::Io)?;
        println!("Scratch directory: {}", scratch.path().display());

        let listener = TcpListener::bind(addr).map_err(BrokerError::CreateSocket)?;
        listener.set_nonblocking(true)?;
        println!("Started TCP server at {}", listener.local_addr()?);

        Ok(Self {
            listener,
            steam: None,
            _scratch: scratch,
        })
    }

    pub fn run(&mut self) -> Result<(), BrokerError> {
        loop {
            if let Some(steam) = self.steam.as_ref() {
                steam.client.run_callbacks();
            }

            let (stream, peer) = match self.listener.accept() {
                Ok(x) => x,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // sleep(POLL_INTERVAL);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            println!("Accepted connection from {peer}");

            stream.set_nonblocking(false)?;
            // stream.set_read_timeout(Some(POLL_INTERVAL))?;
            stream.set_nodelay(true).ok();

            let mut session = Session {
                stream,
                rx_buffer: Vec::with_capacity(protocol::MAX_PAYLOAD_SIZE),
                state: State::Idle,
                steam: &mut self.steam,
                pending_player_replies: Vec::new(),
            };

            match session.run() {
                Ok(SessionResult::Continue) => {
                    println!("session ended, awaiting next connection");
                }
                Ok(SessionResult::Terminate) => {
                    println!("sb_terminate received, exiting for restart");
                    return Ok(());
                }
                Err(err) => {
                    println!("session error: {err}");
                    if self.steam.is_some() {
                        println!("steam was initialized, exiting for restart");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Private working directory for the Steamworks SDK (it writes
/// `steam_appid.txt` and other runtime files into the process's cwd).
/// Removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self, BrokerError> {
        let path = PathBuf::from(format!("/tmp/steam-broker-{:08x}", fastrand::u32(..)));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(BrokerError::Io)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}