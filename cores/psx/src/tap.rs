//! A Multitap (SCPH-1070) in a controller port: four pads in slots A to D
//! (psx-spx, Controller and Memory Card Multitap Adaptor). Slot A's pad is
//! the port's own (Sio::dual_shocks, Sio::pads); this has slots B to D.
//!
//! An access to address 01h is slot A's, as without the Multitap, unless
//! the access before it asked for the long one with 01h as its third byte
//! (REQ): then 80h, 5Ah (the Multitap's ID, 5A80h) and each slot's exchange
//! in eight bytes, the bytes the game sends there being that slot's command
//! and parameters, a pad that gives fewer padded with FFh, an empty slot all
//! FFh. Asking again from a long access gives the short garbage one (HiZ,
//! 80h, 5Ah, slot A's ID's low byte), and from that a long one again:
//!
//!   REQ=0 before                     slot A
//!   REQ=1, slot A before             long
//!   REQ=1, long before               garbage
//!   REQ=1, garbage before            long
//!
//! Addresses 02h to 04h reach slots B to D's pads directly; memory cards
//! other than slot A's (82h to 84h) are not there.

use savestate::state;

use crate::pad::Pad;

/// What an access to 01h returns.
pub(crate) const SLOT_A: u8 = 0;
pub(crate) const LONG: u8 = 1;
pub(crate) const GARBAGE: u8 = 2;

#[derive(Clone, Default)]
pub struct Tap {
    /// The port it is in, while plugged in.
    pub port: Option<usize>,
    /// Slots B, C and D: a pad plugged in, its buttons (bit set: held) and
    /// its DualShock.
    pub plugged: [bool; 3],
    pub buttons: [u16; 3],
    pub pads: [Pad; 3],
    /// The last access to 01h: whether it asked for the long one (REQ),
    /// and what it returned.
    req: bool,
    last: u8,
    /// The access going on: what it returns, and which slots still answer
    /// in a long one.
    pub(crate) kind: u8,
    alive: [bool; 4],
}

// The slots' pads' modes and the last access, after the rest of a state.
state!(Tap { pads, req, last });

impl Tap {
    /// As plugged in, with the pads held as they are, their modes and the
    /// last access as at power-on (a state from before the Multitap).
    pub(crate) fn emptied(&self) -> Tap {
        Tap { port: self.port, plugged: self.plugged, buttons: self.buttons, ..Tap::default() }
    }

    /// An access to 01h begins: what it returns, by the one before.
    pub(crate) fn begin(&mut self) -> u8 {
        self.kind = match (self.req, self.last) {
            (false, _) => SLOT_A,
            (true, SLOT_A) | (true, GARBAGE) => LONG,
            _ => GARBAGE,
        };
        (self.last, self.req, self.alive) = (self.kind, false, [true; 4]);
        self.kind
    }

    /// The access's third byte: whether it asks for the long one next.
    pub(crate) fn request(&mut self, value: u8) {
        self.req = value == 0x01;
    }

    /// A slot (0 to 3) in a long access answers the byte of its eight that
    /// is `seg` (0 to 7), slot A through `a` (its pad and buttons); FFh from
    /// an empty slot, or one whose exchange has ended.
    pub(crate) fn segment(&mut self, slot: usize, seg: u8, value: u8, a: (&mut Pad, u16)) -> u8 {
        if !self.alive[slot] || (slot > 0 && !self.plugged[slot - 1]) {
            return 0xff;
        }
        let (pad, buttons) = match slot {
            0 => a,
            n => (&mut self.pads[n - 1], self.buttons[n - 1]),
        };
        let (reply, ack) = pad.transfer(seg + 1, value, buttons);
        self.alive[slot] = ack;
        reply
    }
}
