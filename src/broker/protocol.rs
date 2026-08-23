//! Wire protocol: frame parsing/building, and the `sb_playerx` field encoding.
//!
//! Frame layout: `"SBRK"` + `u16` LE payload length + payload (max
//! [`MAX_PAYLOAD_SIZE`] bytes).

use crate::BrokerError;

pub const FRAME_HEADER: &[u8; 4] = b"SBRK";
pub const FRAME_HEADER_SIZE: usize = 4;
pub const FRAME_LENGTH_SIZE: usize = 2;
pub const MAX_PAYLOAD_SIZE: usize = 4096;

pub const CONNECT_RESPONSE_HEADER: &[u8] = b"sb_connect\n";
pub const PLAYER_RESPONSE_HEADER: &[u8] = b"sb_playerx\n";

/// Tries to parse one complete frame off the front of `buf`.
///
/// Returns `Some((consumed_bytes, payload))` once a full frame is
/// available, or `None` if more data is still needed. Errors on a bad
/// magic header or an oversized length field.
pub fn parse_frame(buf: &[u8]) -> Result<Option<(usize, Vec<u8>)>, BrokerError> {
    if buf.len() < FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE {
        return Ok(None);
    }

    if &buf[..FRAME_HEADER_SIZE] != FRAME_HEADER {
        return Err(BrokerError::Custom("invalid frame magic"));
    }

    let payload_size =
        u16::from_le_bytes([buf[FRAME_HEADER_SIZE], buf[FRAME_HEADER_SIZE + 1]]) as usize;
    if payload_size > MAX_PAYLOAD_SIZE {
        return Err(BrokerError::Custom("frame too large"));
    }

    let total_size = FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE + payload_size;
    if buf.len() < total_size {
        return Ok(None);
    }

    let payload = buf[FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE..total_size].to_vec();
    Ok(Some((total_size, payload)))
}

/// Wraps `payload` in a frame (`"SBRK"` + `u16` LE length + payload).
pub fn build_frame(payload: &[u8]) -> Result<Vec<u8>, BrokerError> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(BrokerError::Custom("response payload too large"));
    }

    let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE + payload.len());
    frame.extend_from_slice(FRAME_HEADER);
    frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// One field of a `sb_playerx` response. The discriminant doubles as the
/// field's bit position in the response's leading flags word, and
/// [`PlayerField::wire_type`] gives its on-the-wire type tag.
#[derive(Clone, Copy)]
pub enum PlayerField {
    Name = 1 << 0,
    AvatarSmall = 1 << 1,
    AvatarMedium = 1 << 2, // reserved: not populated yet, kept for wire compatibility
    AvatarLarge = 1 << 3,  // reserved: not populated yet, kept for wire compatibility
    Relationship = 1 << 4,
    Country = 1 << 5, // reserved: not populated yet, kept for wire compatibility
    Game = 1 << 6,
    RichPresence = 1 << 7, // reserved: not populated yet, kept for wire compatibility
    PersonaState = 1 << 8,
}

impl PlayerField {
    fn wire_type(self) -> u8 {
        match self {
            PlayerField::Name => 1,
            PlayerField::AvatarSmall => 2,
            PlayerField::AvatarMedium => 3,
            PlayerField::AvatarLarge => 4,
            PlayerField::Relationship => 5,
            PlayerField::Country => 6,
            PlayerField::Game => 7,
            PlayerField::RichPresence => 8,
            PlayerField::PersonaState => 9,
        }
    }
}

/// Appends one TLV field (`type: u8`, `len: u32` LE, `data`) to `payload`
/// if it still fits within [`MAX_PAYLOAD_SIZE`], and ORs the field's bit
/// into `flags` on success. Returns whether the field was written.
pub fn write_field(payload: &mut Vec<u8>, flags: &mut u32, field: PlayerField, data: &[u8]) -> bool {
    const FIELD_HEADER_SIZE: usize = 1 + 4;

    if payload.len() + FIELD_HEADER_SIZE + data.len() > MAX_PAYLOAD_SIZE {
        return false;
    }

    payload.push(field.wire_type());
    payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
    payload.extend_from_slice(data);
    *flags |= field as u32;
    true
}

/// Relationship byte sent in the [`PlayerField::Relationship`] field.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Relationship {
    None = 0,
    Friend = 1,
    Blocked = 2,
    FriendshipRequested = 3,
    RequestingFriendship = 4,
}