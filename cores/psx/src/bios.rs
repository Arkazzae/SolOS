//! The BIOS's kernel, done by the core (high-level emulation), so that no
//! BIOS ROM is needed: what a program calls of it (by jumping to A0, B0 or
//! C0 with the function's number in t1), its exceptions and its interrupts.
//! What a program may read or change of the kernel is in RAM where Sony's
//! keeps it: the exception vector at 80h, the tables of A, B and C
//! functions at 200h, 874h and 674h (a program may put its own function in
//! one), and through the table of tables at 100h the queues of interrupt
//! handlers (ExCB), the process control block (PCB, its first word the
//! current thread's TCB), the threads' TCBs and the events' EvCBs.
//!
//! An exception saves the registers in the current TCB. System calls enter
//! and leave critical sections and change threads. An interrupt walks the
//! queues in priority order, calling each handler's verifier and, when that
//! returns nonzero, the handler; then it goes to the program's own exit
//! from interrupts if it set one (SetCustomExitFromException), and returns
//! through the TCB otherwise. The kernel's own handlers (the root counters'
//! and the pads') are in the queues as Sony's are. Other exceptions go to
//! A(40h), which a program may replace to handle them.
//!
//! What the program gave the kernel (handlers, event callbacks) runs on the
//! CPU: the kernel calls it with the return address at KERNEL_RETURN, and
//! keeps where it was in a frame on the stack, where the program's code
//! leaves it; when the CPU gets back there, the kernel goes on from the
//! frame. A handler that returns from the exception itself leaves the
//! kernel's frames behind with the rest of the exception's stack.

use crate::cpu::{INTERRUPT, SYSCALL};
use crate::files::FILES;
use crate::{Psx, VBLANK};
use savestate::state;

/// What the BIOS leaves at 8000_0080: a jump to its handler (lui k0, 0;
/// addiu k0, 0C80h; jr k0; nop), there for a program to read or replace.
pub const EXCEPTION_STUB: [u8; 16] = [
    0x00, 0x00, 0x1a, 0x3c, 0x80, 0x0c, 0x5a, 0x27, 0x08, 0x00, 0x40, 0x03, 0x00, 0x00, 0x00, 0x00,
];

/// The tables of functions: the vector that calls through each, where it
/// is in RAM, how many functions it holds, and where in the BIOS's ROM
/// the core answers them (four bytes a function).
const TABLES: [(u32, u32, u32, u32); 3] =
    [(0xa0, 0x200, 0xc0, 0x1fc1_0000), (0xb0, 0x874, 0x60, 0x1fc1_0400), (0xc0, 0x674, 0x20, 0x1fc1_0600)];
const B_TABLE: u32 = 0x874;
const C_TABLE: u32 = 0x674;
/// Addresses in the BIOS's ROM (where no ROM is) that the core answers:
/// where a function the kernel calls returns to; where the program's
/// handler of an exception (A(40h)) returns to, to end it; where a program
/// that Exec started returns to; the kernel's own handlers, a verifier and
/// a handler each.
const KERNEL_RETURN: u32 = 0x1fc1_1000;
const EXCEPTION_RETURN: u32 = 0x1fc1_1008;
const EXEC_RETURN: u32 = 0x1fc1_1010;
const OWN_HANDLERS: u32 = 0x1fc1_1100;
/// Where PsyQ's _patch_pad finds the kernel's switches for reading the
/// pads, at their place in Sony's kernel after ChangeClearPad (B(5Bh)):
/// on and off. (What it clears there, the switching of the port when a
/// pad does not answer, is not done here anyway.)
const PADS_ON: u32 = 0x1fc1_0400 + 4 * 0x5b + 0x884;
const PADS_OFF: u32 = 0x1fc1_0400 + 4 * 0x5b + 0x894;
/// Where C(06h), the exception handler, points: the patches some programs
/// make to Sony's handler find no code there, and what they write goes
/// nowhere.
const HANDLER_CODE: u32 = 0x1fc1_2000;
/// The functions of the devices' drivers in the device table: no program
/// is known to call them itself; one that does finds them not answered.
const DRIVERS: u32 = 0x1fc1_3000;

// The kernel's memory: the queues of handlers (four priorities, the first
// handler of each), the PCB, the kernel's own handlers, the device table,
// then the TCBs and EvCBs; the stack the kernel handles exceptions on,
// under 8000h.
const EXCB: u32 = 0x8000_1000;
const PCB: u32 = 0x8000_1020;
const OWN: u32 = 0x8000_1040;
const DEVICES: u32 = 0x8000_1100;
/// Where LoadAndExecute puts the header Exec takes.
const EXEC_HEADER: u32 = 0x8000_14c0;
const BLOCKS: u32 = 0x8000_1500;
const KERNEL_STACK: u32 = 0x8000_7ff0;
const STACK_BOTTOM: u32 = 0x8000_6000;

// The kernel's own handlers: the root counters' (0-3, the last the
// vertical blank) and the pads'.
const RCNT: [u32; 4] = [1 << 4, 1 << 5, 1 << 6, VBLANK];
const PAD: u32 = 4;
/// The default one (InitDefInt), at priority 3: it delivers an event of its
/// own class for each interrupt let through, in this order (the timers 1
/// and 2 sharing one, as Sony's do).
const IRQS: u32 = 5;
const OWN_COUNT: u32 = 6;
const IRQ_EVENTS: [(u32, u32); 11] = [
    (2, 0xf000_0003),
    (9, 0xf000_0009),
    (1, 0xf000_0002),
    (10, 0xf000_000a),
    (8, 0xf000_000b),
    (0, 0xf000_0001),
    (4, 0xf000_0005),
    (5, 0xf000_0006),
    (6, 0xf000_0006),
    (7, 0xf000_0008),
    (3, 0xf000_0004),
];

// Words of a TCB: its state, then the registers from r0, the return
// address, hi, lo, SR and CAUSE as the exception found them.
const TCB_SIZE: u32 = 0xc0;
const TCB_R: u32 = 0x08;
const TCB_EPC: u32 = 0x88;
const TCB_HI: u32 = 0x8c;
const TCB_LO: u32 = 0x90;
const TCB_SR: u32 = 0x94;
const TCB_CAUSE: u32 = 0x98;
const THREAD_FREE: u32 = 0x1000;
const THREAD_USED: u32 = 0x4000;

// An EvCB: class, state, spec, mode and callback. An event is opened
// disabled, enabled to wait for it, and ready when delivered; delivered,
// it calls back, or becomes ready for the program to see.
const EVCB_SIZE: u32 = 0x1c;
const EVENT_FREE: u32 = 0;
const EVENT_DISABLED: u32 = 0x1000;
const EVENT_ENABLED: u32 = 0x2000;
const EVENT_READY: u32 = 0x4000;
const CALL_BACK: u32 = 0x1000;
const NO_CALL_BACK: u32 = 0x2000;

// The heaps: the program's and the kernel's.
const PROGRAM: usize = 0;
const KERNEL: usize = 1;

// The memory card's events: of the driver (HwCARD) and of the backup
// unit's layer above it (SwCARD); done, an error, no card (time out), a
// new card.
const HW_CARD: u32 = 0xf000_0011;
const SW_CARD: u32 = 0xf400_0001;
const CARD_DONE: u32 = 0x0004;
const CARD_ERROR: u32 = 0x8000;
const CARD_TIMEOUT: u32 = 0x0100;
const CARD_NEW: u32 = 0x2000;
// What the driver does for a port.
pub(crate) const CARD_INFO: u32 = 1;
const CARD_READ: u32 = 2;
const CARD_WRITE: u32 = 3;

// Frames the kernel leaves on the stack while what it called runs: the
// walk through the queues (priority, handler, and whether the verifier or
// the handler runs), and delivering an event (where to return, class,
// spec, the next EvCB and what to return).
const FRAME: u32 = 0x20;
const WALK: u32 = 0x4b57_414c;
const DELIVER: u32 = 0x4b44_4c56;
const VERIFYING: u32 = 0;
const HANDLING: u32 = 1;

#[derive(Clone, Default)]
pub struct Hle {
    /// Where SetCustomExitFromException points (a jmp_buf: ra, sp, fp,
    /// s0-s7, gp), or 0.
    custom_exit: u32,
    /// Whether the root counters' handlers acknowledge their interrupt and
    /// end the exception (ChangeClearRCnt), and the pads' the vertical
    /// blank (ChangeClearPad).
    rcnt_ack: [bool; 4],
    pad_ack: bool,
    /// Which interrupts the default handler acknowledges (SetIrqAutoAck).
    irq_ack: [bool; 11],
    /// The pads' buffers (InitPad), filled at each vertical blank once
    /// started; the buttons' word of OutdatedPadInitAndStart.
    pad_buffers: [(u32, u32); 2],
    pad_started: bool,
    pad_word: u32,
    /// The heaps InitHeap and SysInitMemory gave, the program's and the
    /// kernel's: where each starts and ends.
    heaps: [(u32, u32); 2],
    /// rand's seed.
    seed: u32,
    /// The memory card driver: whether it runs at the vertical blank
    /// (StartCard), the port it serves next, and for each port its state
    /// (_card_status: 1 when idle, else what it is doing; 11h after no
    /// card answered, 21h after an error), the operation waiting (what,
    /// device, sector, buffer) and whether the backup unit's layer waits
    /// for it. The next operation does not fail on a new card after
    /// _new_card; the device of the last one to end.
    card_started: bool,
    card_port: u32,
    card_state: [u32; 2],
    card_op: [[u32; 4]; 2],
    pub(crate) bu_waits: [bool; 2],
    pub(crate) card_new_ok: bool,
    card_last: u32,
    /// The open files: mode (0 when closed), card, first block, offset and
    /// length; what errno says; firstfile's pattern, and the card and the
    /// block nextfile looks from.
    pub(crate) files: [[u32; 5]; FILES],
    pub(crate) errno: u32,
    pub(crate) find: [u8; 20],
    pub(crate) find_at: [u32; 2],
    /// The slot a file call waited for the card in, for the driver's event
    /// as the call returns (no more than within a call).
    pub(crate) card_touched: Option<usize>,
    /// Functions called that are not answered: (table, number).
    pub unknown: Vec<(u32, u32)>,
}

state!(Hle {
    custom_exit, rcnt_ack, pad_ack, irq_ack, pad_buffers, pad_started, pad_word, heaps, seed, card_started, card_port,
    card_state, card_op, bu_waits, card_new_ok, card_last, files, errno, find, find_at
});

/// A kernel address as the tables hold it: uncached, in the ROM.
fn rom(physical: u32) -> u32 {
    0xa000_0000 | physical
}

impl Psx {
    /// Lays out what the BIOS leaves in RAM for a program, with room for
    /// the threads and events it asks for.
    pub(crate) fn set_up_kernel(&mut self, threads: u32, events: u32) {
        self.ram[0x80..0x90].copy_from_slice(&EXCEPTION_STUB);
        for (_, table, count, entries) in TABLES {
            for n in 0..count {
                self.store32(0x8000_0000 | (table + 4 * n), rom(entries + 4 * n));
            }
        }
        self.store32(0x8000_0000 | (C_TABLE + 4 * 6), rom(HANDLER_CODE));
        let threads = threads.clamp(1, 32);
        let room = (STACK_BOTTOM - BLOCKS - threads * TCB_SIZE) / EVCB_SIZE;
        let events = events.clamp(1, room);
        let (tcbs, evcbs) = (BLOCKS, BLOCKS + threads * TCB_SIZE);
        let tables = [(EXCB, 0x20), (PCB, 4), (tcbs, threads * TCB_SIZE), (0, 0), (evcbs, events * EVCB_SIZE)];
        for (i, (addr, size)) in tables.into_iter().enumerate() {
            self.store32(0x8000_0100 + 8 * i as u32, addr);
            self.store32(0x8000_0104 + 8 * i as u32, size);
        }
        self.set_up_devices();
        for i in 0..threads {
            self.store32(tcbs + i * TCB_SIZE, THREAD_FREE);
        }
        // The first thread runs.
        self.store32(tcbs, THREAD_USED);
        self.store32(PCB, tcbs);
        // The root counters' handlers, at priority 1, acknowledging their
        // interrupts until told not to; the pads' is queued by StartPad.
        for n in 0..OWN_COUNT {
            let element = OWN + 16 * n;
            self.store32(element + 4, rom(OWN_HANDLERS + 8 * n + 4));
            self.store32(element + 8, rom(OWN_HANDLERS + 8 * n));
        }
        self.queue_rcnt_handlers(1);
        self.queue_default_handler(3);
        self.hle.pad_ack = true;
    }

    /// The device table (DCB) through the table of tables, as Sony's kernel
    /// fills it: the TTY, the CD-ROM and the memory cards, each its name,
    /// flags, block size, description and its driver's functions. Programs
    /// look a device up there by name; Soul Reaver's library puts its own
    /// firstfile in the memory cards', and finds nothing without them.
    fn set_up_devices(&mut self) {
        const ENTRY: u32 = 0x50;
        let devices: [(&[u8], u32, u32, &[u8]); 3] =
            [(b"tty", 3, 1, b"CONSOLE"), (b"cdrom", 0x14, 0x800, b"CD-ROM"), (b"bu", 0x14, 0x80, b"MEMORY CARD")];
        let mut text = DEVICES + 10 * ENTRY;
        for (i, (name, flags, block, description)) in devices.into_iter().enumerate() {
            let entry = DEVICES + ENTRY * i as u32;
            for (field, string) in [(0, name), (0x0c, description)] {
                self.store32(entry + field, text);
                for (j, &c) in string.iter().chain(&[0]).enumerate() {
                    self.store8(text + j as u32, c);
                }
                text += (string.len() as u32 + 4) & !3;
            }
            self.store32(entry + 4, flags);
            self.store32(entry + 8, block);
            for f in (0x10..ENTRY).step_by(4) {
                self.store32(entry + f, rom(DRIVERS + entry - DEVICES + f));
            }
        }
        self.store32(0x8000_0150, DEVICES);
        self.store32(0x8000_0154, 10 * ENTRY);
    }

    /// Answers a BIOS call, an exception or a return to the kernel at an
    /// address the BIOS owns; false where the program runs on.
    pub(crate) fn hle_trap(&mut self, pc: u32) -> bool {
        if !self.bios.is_empty() {
            // The real BIOS runs; what goes to putchar is kept as the TTY.
            let (a, b) = (pc & 0x1fff_ffff == 0xa0, pc & 0x1fff_ffff == 0xb0);
            if (a && self.cpu.r[9] == 0x3c) || (b && self.cpu.r[9] == 0x3d) {
                self.tty.push(self.cpu.r[4] as u8);
            }
            return false;
        }
        let physical = pc & 0x1fff_ffff;
        match physical {
            0xa0 | 0xb0 | 0xc0 => self.call_vector(physical),
            // Unless the program put its own handler there.
            0x80 if self.ram[0x80..0x90] == EXCEPTION_STUB => self.exception_entry(),
            KERNEL_RETURN => self.kernel_return(),
            EXCEPTION_RETURN => self.return_from_exception(),
            EXEC_RETURN => self.exec_return(),
            p if (DRIVERS..DRIVERS + 3 * 0x50).contains(&p) => {
                self.note_unknown(0xd0 + (p - DRIVERS) / 0x50, (p - DRIVERS) % 0x50);
                self.ret(0);
            }
            PADS_ON | PADS_OFF => {
                self.hle.pad_started = physical == PADS_ON;
                self.ret(0);
            }
            p if (OWN_HANDLERS..OWN_HANDLERS + 8 * OWN_COUNT).contains(&p) && p & 3 == 0 => {
                self.own_handler((p - OWN_HANDLERS) / 8, p & 4 != 0)
            }
            p => match TABLES.iter().find(|t| (t.3..t.3 + 4 * t.2).contains(&p) && p & 3 == 0) {
                Some(&(vector, _, _, entries)) => self.function(vector, (p - entries) / 4),
                None => return false,
            },
        }
        true
    }

    /// A0, B0 or C0: the function the table holds for t1, which is the
    /// core's own or one the program put there.
    fn call_vector(&mut self, vector: u32) {
        let (_, table, count, entries) = TABLES.into_iter().find(|t| t.0 == vector).unwrap();
        let n = self.cpu.r[9] & 0xff;
        let entry = if n < count { self.peek32(0x8000_0000 | (table + 4 * n)) } else { 0 };
        if entry == 0 || entry & 0x1fff_ffff == entries + 4 * n {
            self.function(vector, n);
        } else {
            self.jump(entry);
        }
    }

    fn jump(&mut self, target: u32) {
        self.cpu.pc = target;
        self.cpu.next_pc = target.wrapping_add(4);
        self.cycles += 2;
    }

    /// A function returns v0 to ra.
    fn ret(&mut self, v0: u32) {
        self.cpu.r[2] = v0;
        self.jump(self.cpu.r[31]);
    }

    /// Calls a function the program gave with one argument; it returns to
    /// KERNEL_RETURN, with the kernel's frame on top of the stack.
    fn call(&mut self, function: u32, a0: u32) {
        self.cpu.r[4] = a0;
        self.cpu.r[31] = rom(KERNEL_RETURN);
        self.jump(function);
    }

    fn push_frame(&mut self, words: [u32; 6]) {
        self.cpu.r[29] = self.cpu.r[29].wrapping_sub(FRAME);
        self.set_frame(words);
    }

    fn set_frame(&mut self, words: [u32; 6]) {
        let sp = self.cpu.r[29];
        for (i, w) in words.into_iter().enumerate() {
            self.store32(sp.wrapping_add(4 * i as u32), w);
        }
    }

    fn frame(&self) -> [u32; 6] {
        let sp = self.cpu.r[29];
        std::array::from_fn(|i| self.peek32(sp.wrapping_add(4 * i as u32)))
    }

    fn pop_frame(&mut self) {
        self.cpu.r[29] = self.cpu.r[29].wrapping_add(FRAME);
    }

    /// What the kernel called has returned: it goes on from its frame.
    fn kernel_return(&mut self) {
        let frame = self.frame();
        let v0 = self.cpu.r[2];
        match frame[0] {
            WALK => {
                let [_, priority, element, stage, ..] = frame;
                let handler = self.peek32(element + 4);
                if stage == VERIFYING && v0 != 0 && handler != 0 {
                    self.set_frame([WALK, priority, element, HANDLING, 0, 0]);
                    return self.call(handler, v0);
                }
                let next = self.peek32(element);
                self.walk(priority, next);
            }
            DELIVER => self.deliver_from(frame[4]),
            _ => {
                // The stack is not as the kernel left it.
                self.note_unknown(0x80, 0xff);
                self.return_from_exception();
            }
        }
    }

    fn note_unknown(&mut self, table: u32, function: u32) {
        if !self.hle.unknown.contains(&(table, function)) {
            self.hle.unknown.push((table, function));
        }
    }

    /// The queues of handlers and the PCB, where the table of tables says:
    /// a program may set up the kernel's structures anew, as the BIOS does.
    fn excb(&self) -> u32 {
        self.peek32(0x8000_0100)
    }

    fn pcb(&self) -> u32 {
        self.peek32(0x8000_0108)
    }

    fn tcb(&self) -> u32 {
        self.peek32(self.pcb())
    }

    /// The exception vector: the registers go to the TCB; then the system
    /// call, the interrupt or A(40h).
    fn exception_entry(&mut self) {
        let tcb = self.tcb();
        for i in 0..32 {
            self.store32(tcb + TCB_R + 4 * i, self.cpu.r[i as usize]);
        }
        let cpu = self.cpu.clone();
        self.store32(tcb + TCB_EPC, cpu.epc);
        self.store32(tcb + TCB_HI, cpu.hi);
        self.store32(tcb + TCB_LO, cpu.lo);
        self.store32(tcb + TCB_SR, cpu.sr);
        self.store32(tcb + TCB_CAUSE, cpu.cause);
        match cpu.cause >> 2 & 0x1f {
            SYSCALL => self.syscall(tcb),
            INTERRUPT => {
                self.cpu.r[29] = KERNEL_STACK;
                self.push_frame([WALK, 0, 0, VERIFYING, 0, 0]);
                let first = self.peek32(self.excb());
                self.walk(0, first);
            }
            code => {
                let entry = self.peek32(0x8000_0200 + 4 * 0x40);
                if entry & 0x1fff_ffff != TABLES[0].3 + 4 * 0x40 {
                    // The program's handler, which returns to end the
                    // exception through the TCB it may have changed.
                    self.cpu.r[29] = KERNEL_STACK;
                    self.cpu.r[31] = rom(EXCEPTION_RETURN);
                    self.jump(entry);
                    return;
                }
                self.note_unknown(0x80, code);
                // Skip what failed rather than loop on it.
                self.store32(tcb + TCB_EPC, cpu.epc.wrapping_add(4));
                self.return_from_exception();
            }
        }
    }

    /// System calls 1 and 2 enter and leave a critical section (by the
    /// interrupt enable the return restores), 3 changes the thread.
    fn syscall(&mut self, tcb: u32) {
        let sr = self.peek32(tcb + TCB_SR);
        self.store32(tcb + TCB_EPC, self.cpu.epc.wrapping_add(4));
        match self.cpu.r[4] {
            1 => {
                // Interrupts were on if IEp and the hardware mask were.
                self.store32(tcb + TCB_R + 4 * 2, u32::from(sr & 0x404 == 0x404));
                self.store32(tcb + TCB_SR, sr & !0x404);
            }
            2 => self.store32(tcb + TCB_SR, sr | 0x404),
            3 => {
                self.store32(tcb + TCB_R + 4 * 2, 1);
                self.store32(self.pcb(), self.cpu.r[5]);
            }
            _ => {}
        }
        self.return_from_exception();
    }

    /// Walks the queues from a handler on: calls the next verifier, or
    /// when none is left ends the interrupt.
    fn walk(&mut self, mut priority: u32, mut element: u32) {
        loop {
            if element == 0 {
                priority += 1;
                if priority == 4 {
                    self.pop_frame();
                    return self.end_interrupt();
                }
                element = self.peek32(self.excb() + 8 * priority);
                continue;
            }
            let verifier = self.peek32(element + 8);
            if verifier != 0 {
                self.set_frame([WALK, priority, element, VERIFYING, 0, 0]);
                return self.call(verifier, 0);
            }
            element = self.peek32(element);
        }
    }

    /// To the program's exit from interrupts, or back through the TCB.
    fn end_interrupt(&mut self) {
        match self.hle.custom_exit {
            0 => self.return_from_exception(),
            exit => self.long_jump(exit, 1),
        }
    }

    /// Puts back the registers in the TCB, with the mode stack in its SR
    /// popped (RFE), and goes on where it says.
    fn return_from_exception(&mut self) {
        let tcb = self.tcb();
        for i in 1..32 {
            self.cpu.r[i] = self.peek32(tcb + TCB_R + 4 * i as u32);
        }
        self.cpu.hi = self.peek32(tcb + TCB_HI);
        self.cpu.lo = self.peek32(tcb + TCB_LO);
        let sr = self.peek32(tcb + TCB_SR);
        self.cpu.sr = sr & !0x0f | sr >> 2 & 0x0f;
        let epc = self.peek32(tcb + TCB_EPC);
        self.jump(epc);
    }

    /// The registers a jmp_buf keeps: ra, sp, fp, s0-s7 and gp.
    fn set_jump(&mut self, buffer: u32) {
        let r = self.cpu.r;
        for (i, n) in [31, 29, 30, 16, 17, 18, 19, 20, 21, 22, 23, 28].into_iter().enumerate() {
            self.store32(buffer + 4 * i as u32, r[n]);
        }
    }

    fn long_jump(&mut self, buffer: u32, v0: u32) {
        for (i, n) in [31, 29, 30, 16, 17, 18, 19, 20, 21, 22, 23, 28].into_iter().enumerate() {
            self.cpu.r[n] = self.peek32(buffer + 4 * i as u32);
        }
        self.ret(v0);
    }

    fn enqueue(&mut self, priority: u32, element: u32) {
        let queue = self.excb() + 8 * priority;
        let first = self.peek32(queue);
        self.store32(queue, element);
        self.store32(element, first);
    }

    fn dequeue(&mut self, priority: u32, element: u32) -> u32 {
        let mut link = self.excb() + 8 * priority;
        loop {
            let at = self.peek32(link);
            if at == 0 {
                return 0;
            }
            if at == element {
                let next = self.peek32(at);
                self.store32(link, next);
                return at;
            }
            link = at;
        }
    }

    /// The EvCBs: where they start, how many.
    fn evcbs(&self) -> (u32, u32) {
        (self.peek32(0x8000_0120), self.peek32(0x8000_0124) / EVCB_SIZE)
    }

    /// DeliverEvent: events waiting for the class and spec become ready,
    /// or call back; then returns `result` to `ret`.
    fn deliver(&mut self, class: u32, spec: u32, ret: u32, result: u32) {
        self.push_frame([DELIVER, ret, class, spec, 0, result]);
        self.deliver_from(0);
    }

    fn deliver_from(&mut self, mut i: u32) {
        let [_, ret, class, spec, _, result] = self.frame();
        let (evcbs, count) = self.evcbs();
        while i < count {
            let event = evcbs + EVCB_SIZE * i;
            i += 1;
            let [c, state, s, mode, function] = std::array::from_fn(|w| self.peek32(event + 4 * w as u32));
            if state != EVENT_ENABLED || c != class || s != spec {
                continue;
            }
            if mode == NO_CALL_BACK {
                self.store32(event + 4, EVENT_READY);
            } else if mode == CALL_BACK && function != 0 {
                self.set_frame([DELIVER, ret, class, spec, i, result]);
                return self.call(function, 0);
            }
        }
        self.pop_frame();
        if ret == 0 {
            // The next event of a list, in the frame below.
            return self.deliver_from(0);
        }
        self.cpu.r[2] = result;
        self.jump(ret);
    }

    /// Delivers events one after another, then returns `result` to `ret`.
    fn deliver_all(&mut self, events: &[(u32, u32)], ret: u32, result: u32) {
        if events.is_empty() {
            self.cpu.r[2] = result;
            return self.jump(ret);
        }
        for (i, &(class, spec)) in events.iter().enumerate().rev() {
            let to = if i + 1 == events.len() { ret } else { 0 };
            self.push_frame([DELIVER, to, class, spec, 0, result]);
        }
        self.deliver_from(0);
    }

    /// The kernel's own handlers, called from the walk.
    fn own_handler(&mut self, n: u32, handler: bool) {
        let pending = self.irq_stat & self.irq_mask;
        match (n, handler) {
            (0..4, false) if pending & RCNT[n as usize] != 0 => {
                let ret = self.cpu.r[31];
                self.deliver(0xf200_0000 + n, 2, ret, 1);
            }
            (0..4, true) if self.hle.rcnt_ack[n as usize] => {
                self.irq_stat &= !RCNT[n as usize];
                self.update_irq_line();
                self.return_from_exception();
            }
            (PAD, false) => self.ret(u32::from(pending & VBLANK != 0)),
            (IRQS, false) => {
                let mut events = Vec::new();
                for (irq, class) in IRQ_EVENTS {
                    if pending & 1 << irq != 0 {
                        events.push((class, 0x1000));
                        if self.hle.irq_ack[irq as usize] {
                            self.irq_stat &= !(1 << irq);
                        }
                    }
                }
                self.update_irq_line();
                let ret = self.cpu.r[31];
                self.deliver_all(&events, ret, 0);
            }
            (PAD, true) => {
                if self.hle.pad_started {
                    self.read_pads();
                }
                if self.hle.pad_ack {
                    self.irq_stat &= !VBLANK;
                    self.update_irq_line();
                }
                if self.hle.card_started {
                    let events = self.card_step();
                    let ret = self.cpu.r[31];
                    return self.deliver_all(&events, ret, 0);
                }
                self.ret(0);
            }
            _ => self.ret(0),
        }
    }

    /// What the pads' handler reads at the vertical blank, in the buffers
    /// InitPad gave: 0 and the pad's ID, then its buttons (low when held);
    /// FFh first where no pad answers (port 2 while the host has no pad
    /// plugged in there).
    fn read_pads(&mut self) {
        for port in 0..2 {
            let (buffer, _) = self.hle.pad_buffers[port];
            if buffer == 0 {
                continue;
            }
            if port == 0 || self.sio.second_pad {
                let held = !self.sio.pads[port];
                for (i, b) in [0, 0x41, held as u8, (held >> 8) as u8].into_iter().enumerate() {
                    self.store8(buffer + i as u32, b);
                }
            } else {
                self.store8(buffer, 0xff);
            }
        }
        if self.hle.pad_word != 0 {
            // Port 1's buttons in the low halfword, port 2's high (none: all released).
            let [one, two] = [0, 1].map(|port| (!self.sio.pads[port]).swap_bytes());
            let two = if self.sio.second_pad { two } else { 0xffff };
            self.store32(self.hle.pad_word, u32::from(two) << 16 | u32::from(one));
        }
    }

    /// Exec started a program, which returned: what Exec kept in its header
    /// (at s0, which the program keeps) goes back, and Exec returns 1.
    fn exec_return(&mut self) {
        let header = self.cpu.r[16];
        for (offset, n) in [(0x34, 31), (0x28, 29), (0x2c, 30), (0x30, 28), (0x38, 16)] {
            self.cpu.r[n] = self.peek32(header + offset);
        }
        self.ret(1);
    }

    /// A function of table A, B or C; returns to ra with v0 set.
    fn function(&mut self, table: u32, function: u32) {
        let [a0, a1, a2, a3] = [self.cpu.r[4], self.cpu.r[5], self.cpu.r[6], self.cpu.r[7]];
        let ra = self.cpu.r[31];
        let v0 = match (table, function) {
            // setjmp, longjmp.
            (0xa0, 0x13) => {
                self.set_jump(a0);
                0
            }
            (0xa0, 0x14) => return self.long_jump(a0, a1),
            (0xa0, 0x0e | 0x0f) => (a0 as i32).unsigned_abs(),
            (0xa0, 0x10 | 0x11) => self.atoi(a0),
            (0xa0, 0x15) => {
                let end = a0 + self.c_string(a0).len() as u32;
                self.copy_string(end, a1, u32::MAX);
                a0
            }
            (0xa0, 0x16) => {
                let end = a0 + self.c_string(a0).len() as u32;
                self.copy_string(end, a1, a2);
                a0
            }
            (0xa0, 0x17) => self.compare_strings(a0, a1, u32::MAX),
            (0xa0, 0x18) => self.compare_strings(a0, a1, a2),
            (0xa0, 0x19) => {
                self.copy_string(a0, a1, u32::MAX);
                a0
            }
            (0xa0, 0x1a) => {
                self.copy_string(a0, a1, a2);
                a0
            }
            (0xa0, 0x1b) => self.c_string(a0).len() as u32,
            // index, rindex, strchr, strrchr.
            (0xa0, 0x1c..=0x1f) => {
                let text = self.c_string(a0);
                let c = a1 as u8;
                let at = if function & 1 == 0 {
                    text.iter().position(|&b| b == c)
                } else {
                    text.iter().rposition(|&b| b == c)
                };
                match at {
                    Some(i) => a0 + i as u32,
                    None if c == 0 => a0 + text.len() as u32,
                    None => 0,
                }
            }
            (0xa0, 0x25) => (a0 as u8).to_ascii_uppercase().into(),
            (0xa0, 0x26) => (a0 as u8).to_ascii_lowercase().into(),
            // bcopy(src, dst, n), bzero, bcmp, memcpy, memset, memmove,
            // memcmp, memchr.
            (0xa0, 0x27) => {
                self.copy(a1, a0, a2);
                0
            }
            (0xa0, 0x28) => {
                self.fill(a0, 0, a1);
                0
            }
            (0xa0, 0x29 | 0x2d) => self.compare(a0, a1, a2),
            (0xa0, 0x2a | 0x2c) => {
                self.copy(a0, a1, a2);
                a0
            }
            (0xa0, 0x2b) => {
                self.fill(a0, a1 as u8, a2);
                a0
            }
            (0xa0, 0x2e) => (0..a2).find(|&i| self.peek8(a0 + i) == a1 as u8).map_or(0, |i| a0 + i),
            (0xa0, 0x2f) => {
                self.hle.seed = self.hle.seed.wrapping_mul(0x41c6_4e6d).wrapping_add(0x3039);
                self.hle.seed >> 16 & 0x7fff
            }
            (0xa0, 0x30) => {
                self.hle.seed = a0;
                0
            }
            // malloc, free, calloc, realloc, InitHeap; SysMalloc, SysFree,
            // SysInitMemory, of the kernel's heap.
            (0xa0, 0x33) => self.malloc(PROGRAM, a0),
            (0xa0, 0x34) => {
                self.free(PROGRAM, a0);
                0
            }
            (0xa0, 0x37) => {
                let size = a0.wrapping_mul(a1);
                let block = self.malloc(PROGRAM, size);
                self.fill(block, 0, if block != 0 { size } else { 0 });
                block
            }
            (0xa0, 0x38) => self.realloc(a0, a1),
            (0xa0, 0x39) => {
                self.init_heap(PROGRAM, a0, a1);
                0
            }
            (0xb0, 0x00) => self.malloc(KERNEL, a0),
            (0xb0, 0x01) => {
                self.free(KERNEL, a0);
                0
            }
            (0xc0, 0x08) => {
                self.init_heap(KERNEL, a0, a1);
                0
            }
            // EnqueueTimerAndVblankIrqs, EnqueueSyscallHandler (system calls
            // are taken before the queues), InitDefInt, SetIrqAutoAck.
            (0xc0, 0x00) => {
                self.queue_rcnt_handlers(a0);
                0
            }
            (0xc0, 0x01) => 0,
            (0xc0, 0x0c) => {
                self.queue_default_handler(a0);
                0
            }
            (0xc0, 0x0d) => {
                if let Some(ack) = self.hle.irq_ack.get_mut(a0 as usize) {
                    *ack = a1 != 0;
                }
                0
            }
            (0xa0, 0x3c) | (0xb0, 0x3d) => {
                self.tty.push(a0 as u8);
                a0
            }
            (0xa0, 0x3e) | (0xb0, 0x3f) => {
                let text = self.c_string(a0);
                self.tty.extend_from_slice(&text);
                self.tty.push(b'\n');
                0
            }
            (0xa0, 0x3f) => {
                let text = self.printf();
                self.tty.extend_from_slice(&text);
                text.len() as u32
            }
            (0xa0, 0x43) => return self.exec(a0, a1, a2),
            (0xa0, 0x51) => return self.load_and_execute(a0, a1, a2),
            // _96_init: the BIOS's own reading of the CD-ROM, which reads the
            // disc itself here.
            (0xa0, 0x71) => 0,
            // FlushCache: written code is dropped as it is written.
            (0xa0, 0x44) => 0,
            // GPU_cw (GP0), SendGP1Command, GPU_GetGpuStatus.
            (0xa0, 0x49) => {
                self.io_write(0x1f80_1810, a0, 4);
                0
            }
            (0xa0, 0x48) => {
                self.io_write(0x1f80_1814, a0, 4);
                0
            }
            (0xa0, 0x4d) => self.load32(0x1f80_1814),
            // CdRemove: the kernel queued no handlers for the CD-ROM.
            (0xa0, 0x72) => 0,
            // SetMemSize: 2 or 8 MB.
            (0xa0, 0x9f) if a0 == 2 || a0 == 8 => {
                self.ram_size = if a0 == 2 { 0xb88 } else { 0x888 };
                self.store32(0x8000_0060, a0);
                0
            }
            (0xb0, 0x07) => return self.deliver(a0, a1, ra, 0),
            (0xb0, 0x08) => self.open_event(a0, a1, a2, a3),
            (0xb0, 0x09) => {
                self.set_event_state(a0, None, EVENT_FREE);
                1
            }
            (0xb0, 0x0a) => return self.wait_event(a0),
            (0xb0, 0x0b) => u32::from(self.set_event_state(a0, Some(EVENT_READY), EVENT_ENABLED)),
            (0xb0, 0x0c) => {
                if self.event_state(a0) != EVENT_FREE {
                    self.set_event_state(a0, None, EVENT_ENABLED);
                }
                1
            }
            (0xb0, 0x0d) => {
                if self.event_state(a0) != EVENT_FREE {
                    self.set_event_state(a0, None, EVENT_DISABLED);
                }
                1
            }
            (0xb0, 0x0e) => self.open_thread(a0, a1, a2),
            (0xb0, 0x0f) => {
                let tcb = self.peek32(0x8000_0110) + TCB_SIZE * (a0 & 0xffff);
                self.store32(tcb, THREAD_FREE);
                1
            }
            (0xb0, 0x10) => return self.change_thread(a0),
            // InitPad, StartPad, StopPad.
            (0xb0, 0x12) => {
                self.hle.pad_buffers = [(a0, a1), (a2, a3)];
                self.hle.pad_word = 0;
                self.fill(a0, 0, a1);
                self.fill(a2, 0, a3);
                self.hle.pad_started = true;
                1
            }
            (0xb0, 0x13) => {
                self.start_pad();
                1
            }
            (0xb0, 0x14) => {
                self.hle.rcnt_ack[3] = true;
                self.dequeue(2, OWN + 16 * PAD);
                0
            }
            // OutdatedPadInitAndStart(type, buttons), OutdatedPadGetButtons.
            (0xb0, 0x15) if a0 == 0x2000_0000 || a0 == 0x2000_0001 => {
                self.hle.pad_buffers = [(0, 0); 2];
                self.hle.pad_started = true;
                self.hle.pad_word = a1;
                self.start_pad();
                2
            }
            (0xb0, 0x16) => {
                self.read_pads();
                self.peek32(self.hle.pad_word)
            }
            (0xb0, 0x17) => return self.return_from_exception(),
            (0xb0, 0x18) => {
                self.hle.custom_exit = 0;
                0
            }
            (0xb0, 0x19) => {
                self.hle.custom_exit = a0;
                0
            }
            (0xb0, 0x20) => {
                self.undeliver(a0, a1);
                0
            }
            // The A table's open, lseek, read, write, close, ioctl, exit,
            // isatty, getc and putc are the B table's.
            (0xa0, 0x00..=0x09) => return self.function(0xb0, 0x32 + function),
            (0xb0, 0x32) => self.open(a0, a1),
            (0xb0, 0x33) => self.seek(a0, a1, a2),
            // read and write: the TTY's, nothing to read from it.
            (0xb0, 0x34) if a0 == 0 => 0,
            (0xb0, 0x34) => self.file_io(a0, a1, a2, false),
            (0xb0, 0x35) if a0 == 1 => {
                let text: Vec<u8> = (0..a2).map(|i| self.peek8(a1 + i)).collect();
                self.tty.extend_from_slice(&text);
                a2
            }
            (0xb0, 0x35) => self.file_io(a0, a1, a2, true),
            (0xb0, 0x36) => self.close(a0),
            (0xb0, 0x41) => self.format(a0),
            (0xb0, 0x42) => self.first_file(a0, a1),
            (0xb0, 0x43) => self.next_file(a0),
            (0xb0, 0x44) => self.rename(a0, a1),
            (0xb0, 0x45) => self.erase(a0),
            // AddDevice, RemoveDevice: a program's own devices are not kept.
            (0xb0, 0x47 | 0x48) => 1,
            // _get_errno, _get_error.
            (0xb0, 0x54 | 0x55) => self.hle.errno,
            (0xb0, 0x56) => C_TABLE,
            (0xb0, 0x57) => B_TABLE,
            // InitCard(pads too), StartCard, StopCard.
            (0xb0, 0x4a) => {
                self.hle.card_state = [1, 1];
                self.hle.card_port = 0;
                self.hle.pad_started = a0 != 0;
                0
            }
            (0xb0, 0x4b) => {
                self.start_pad();
                self.hle.card_started = true;
                1
            }
            (0xb0, 0x4c) => {
                self.hle.rcnt_ack[3] = true;
                self.dequeue(2, OWN + 16 * PAD);
                self.hle.card_started = false;
                1
            }
            // _card_info, of the backup unit's layer and of the driver.
            (0xa0, 0xab) | (0xb0, 0x4d) => {
                let started = self.card_start(CARD_INFO, a0, 0, 0);
                self.hle.bu_waits[(a0 >> 4 & 1) as usize] = started && table == 0xa0;
                u32::from(started)
            }
            // _card_load: the backup unit reads the card's directory.
            (0xa0, 0xac) => {
                let started = self.card_start(CARD_INFO, a0, 0, 0);
                self.hle.bu_waits[(a0 >> 4 & 1) as usize] = started;
                u32::from(started)
            }
            // _card_auto: formatting a card found unformatted, not done.
            (0xa0, 0xad) => 0,
            // _bu_init: the backup unit reads both cards' directories, which
            // it keeps nothing of here.
            (0xa0, 0x70) => 0,
            (0xb0, 0x4e) => u32::from(self.card_start(CARD_WRITE, a0, a1, a2)),
            (0xb0, 0x4f) => u32::from(self.card_start(CARD_READ, a0, a1, a2)),
            (0xb0, 0x50) => {
                self.hle.card_new_ok = true;
                0
            }
            (0xb0, 0x58) => self.hle.card_last,
            (0xb0, 0x5c) => self.hle.card_state[a0 as usize & 1],
            (0xb0, 0x5d) => {
                if self.hle.card_state[a0 as usize & 1] & 1 == 0 {
                    // As the BIOS loops until the driver is idle.
                    self.cycles = self.cycles.max(self.next_event);
                    return self.jump(self.cpu.pc);
                }
                self.hle.card_state[a0 as usize & 1]
            }
            (0xb0, 0x5b) => {
                self.hle.pad_ack = a0 != 0;
                0
            }
            (0xc0, 0x02) => {
                self.enqueue(a0 & 3, a1);
                0
            }
            (0xc0, 0x03) => self.dequeue(a0 & 3, a1),
            (0xc0, 0x0a) => {
                let ack = &mut self.hle.rcnt_ack[a0 as usize & 3];
                u32::from(std::mem::replace(ack, a1 != 0))
            }
            _ => {
                self.note_unknown(table, function);
                0
            }
        };
        if let Some(port) = self.hle.card_touched.take() {
            let spec = if self.sio.cards[port].is_some() { CARD_DONE } else { CARD_TIMEOUT };
            self.hle.card_last = (port as u32) << 4;
            return self.deliver_all(&[(HW_CARD, spec)], ra, v0);
        }
        self.ret(v0);
    }

    /// The root counters' handlers join a queue (EnqueueTimerAndVblankIrqs),
    /// acknowledging their interrupts and ending the exception until told
    /// not to; their interrupts are masked until a program asks for them.
    fn queue_rcnt_handlers(&mut self, priority: u32) {
        for n in 0..4 {
            self.requeue(priority, OWN + 16 * n);
        }
        self.hle.rcnt_ack = [true; 4];
        self.irq_mask &= !RCNT.iter().fold(0, |m, b| m | b);
        self.update_irq_line();
    }

    /// The default handler joins a queue (InitDefInt), acknowledging no
    /// interrupt.
    fn queue_default_handler(&mut self, priority: u32) {
        self.requeue(priority, OWN + 16 * IRQS);
        self.hle.irq_ack = [false; 11];
    }

    /// One of the kernel's handlers leaves any queue it is in and joins one.
    fn requeue(&mut self, priority: u32, element: u32) {
        for p in 0..4 {
            self.dequeue(p, element);
        }
        self.enqueue(priority & 3, element);
    }

    /// The memory card driver at a vertical blank: what was delivered of
    /// it before and not seen goes, and it serves the other port, ending
    /// the operation waiting there at once; the events to deliver.
    fn card_step(&mut self) -> Vec<(u32, u32)> {
        for spec in [CARD_DONE, CARD_ERROR, CARD_TIMEOUT, 0x0200, CARD_NEW] {
            self.undeliver(HW_CARD, spec);
        }
        let port = 1 - self.hle.card_port;
        self.hle.card_port = port;
        let p = port as usize;
        if self.hle.card_state[p] & 1 != 0 {
            return Vec::new();
        }
        let [op, device, sector, buffer] = self.hle.card_op[p];
        self.hle.card_last = device;
        let new_ok = std::mem::take(&mut self.hle.card_new_ok);
        let fresh = self.sio.card_fresh(p);
        let outcome = match self.sio.cards[p].as_ref() {
            None => Err(CARD_TIMEOUT),
            Some(_) if fresh && !new_ok => Err(CARD_NEW),
            Some(card) => match op {
                CARD_READ => match card.sector(sector) {
                    Some(data) => {
                        let data = data.to_vec();
                        for (i, b) in data.into_iter().enumerate() {
                            self.store8(buffer + i as u32, b);
                        }
                        Ok(())
                    }
                    None => Err(CARD_ERROR),
                },
                CARD_WRITE => {
                    let data: Vec<u8> = (0..128).map(|i| self.peek8(buffer + i)).collect();
                    if self.sio.write_card_sector(p, sector, &data) { Ok(()) } else { Err(CARD_ERROR) }
                }
                _ => Ok(()),
            },
        };
        let spec = match outcome {
            Ok(()) => CARD_DONE,
            Err(spec) => spec,
        };
        self.hle.card_state[p] = match spec {
            CARD_TIMEOUT => 0x11,
            CARD_ERROR => 0x21,
            _ => 1,
        };
        let mut events = Vec::new();
        if std::mem::take(&mut self.hle.bu_waits[p]) {
            events.push((SW_CARD, spec));
        }
        events.push((HW_CARD, spec));
        events
    }

    /// Starts an operation of the memory card driver on a port, if it is
    /// idle; whether it started.
    pub(crate) fn card_start(&mut self, op: u32, device: u32, sector: u32, buffer: u32) -> bool {
        let p = (device >> 4 & 1) as usize;
        if self.hle.card_state[p] & 1 == 0 || sector > 0x400 {
            return false;
        }
        self.hle.card_op[p] = [op, device, sector, buffer];
        self.hle.card_state[p] = match op {
            CARD_READ => 2,
            CARD_WRITE => 4,
            _ => 0,
        };
        true
    }

    /// The pads' handler joins the queue at priority 2, and reads the pads
    /// at each vertical blank; the root counters' handler for the vertical
    /// blank no longer acknowledges it, so that the pads' sees it.
    fn start_pad(&mut self) {
        self.dequeue(2, OWN + 16 * PAD);
        self.enqueue(2, OWN + 16 * PAD);
        self.irq_stat &= !VBLANK;
        self.irq_mask |= VBLANK;
        self.update_irq_line();
        self.hle.pad_ack = true;
        self.hle.rcnt_ack[3] = false;
    }

    fn open_event(&mut self, class: u32, spec: u32, mode: u32, function: u32) -> u32 {
        let (evcbs, count) = self.evcbs();
        let Some(i) = (0..count).find(|&i| self.peek32(evcbs + EVCB_SIZE * i + 4) == EVENT_FREE) else {
            return u32::MAX;
        };
        let event = evcbs + EVCB_SIZE * i;
        for (w, value) in [class, EVENT_DISABLED, spec, mode, function].into_iter().enumerate() {
            self.store32(event + 4 * w as u32, value);
        }
        0xf100_0000 | i
    }

    fn event_state(&self, handle: u32) -> u32 {
        let (evcbs, _) = self.evcbs();
        self.peek32(evcbs + EVCB_SIZE * (handle & 0xffff) + 4)
    }

    /// Sets an event's state if it is in the one given (or any); whether it
    /// was.
    fn set_event_state(&mut self, handle: u32, from: Option<u32>, to: u32) -> bool {
        let (evcbs, _) = self.evcbs();
        let state = evcbs + EVCB_SIZE * (handle & 0xffff) + 4;
        if from.is_some_and(|f| self.peek32(state) != f) {
            return false;
        }
        self.store32(state, to);
        true
    }

    /// WaitEvent: 1 once the event is ready (making it wait again); while
    /// it is enabled and not ready, the call runs again after the next
    /// thing the console does, as the BIOS loops until an interrupt
    /// delivers it.
    fn wait_event(&mut self, handle: u32) {
        match self.event_state(handle) {
            EVENT_READY => {
                self.set_event_state(handle, None, EVENT_ENABLED);
                self.ret(1);
            }
            EVENT_ENABLED => {
                self.cycles = self.cycles.max(self.next_event);
                self.jump(self.cpu.pc);
            }
            _ => self.ret(0),
        }
    }

    fn undeliver(&mut self, class: u32, spec: u32) {
        let (evcbs, count) = self.evcbs();
        for i in 0..count {
            let event = evcbs + EVCB_SIZE * i;
            let [c, state, s, mode] = std::array::from_fn(|w| self.peek32(event + 4 * w as u32));
            if state == EVENT_READY && c == class && s == spec && mode == NO_CALL_BACK {
                self.store32(event + 4, EVENT_ENABLED);
            }
        }
    }

    fn open_thread(&mut self, pc: u32, sp: u32, gp: u32) -> u32 {
        let (tcbs, count) = (self.peek32(0x8000_0110), self.peek32(0x8000_0114) / TCB_SIZE);
        let Some(i) = (0..count).find(|&i| self.peek32(tcbs + TCB_SIZE * i) == THREAD_FREE) else {
            return u32::MAX;
        };
        let tcb = tcbs + TCB_SIZE * i;
        self.store32(tcb, THREAD_USED);
        self.store32(tcb + 4, 0x1000);
        self.store32(tcb + TCB_R + 4 * 29, sp);
        self.store32(tcb + TCB_R + 4 * 30, sp);
        self.store32(tcb + TCB_R + 4 * 28, gp);
        self.store32(tcb + TCB_EPC, pc);
        0xff00_0000 | i
    }

    /// ChangeThread: as the system call it makes, the registers go to the
    /// current TCB (to return 1 to the caller) and come back from the
    /// other one.
    fn change_thread(&mut self, handle: u32) {
        let tcb = self.tcb();
        for i in 0..32 {
            self.store32(tcb + TCB_R + 4 * i, self.cpu.r[i as usize]);
        }
        self.store32(tcb + TCB_R + 4 * 2, 1);
        self.store32(tcb + TCB_EPC, self.cpu.r[31]);
        self.store32(tcb + TCB_HI, self.cpu.hi);
        self.store32(tcb + TCB_LO, self.cpu.lo);
        // The SR an exception would have pushed.
        let sr = self.cpu.sr;
        self.store32(tcb + TCB_SR, sr & !0x3f | (sr & 0x0f) << 2);
        let tcbs = self.peek32(0x8000_0110);
        self.store32(self.pcb(), tcbs + TCB_SIZE * (handle & 0xffff));
        self.return_from_exception();
    }

    /// LoadAndExecute(path, stack, offset): a PS-X EXE from the disc
    /// ("cdrom:\\NAME.EXE;1"), read at once, put in RAM and run by Exec with
    /// the stack given (or the caller's, with none); 0 back if it is not
    /// there.
    fn load_and_execute(&mut self, path: u32, stack: u32, offset: u32) {
        let name = String::from_utf8_lossy(&self.c_string(path)).into_owned();
        let file = name.split_once(':').map_or(name.as_str(), |(_, f)| f);
        let exe = self.cdrom.disc().and_then(|disc| crate::boot::file(&mut *disc.lock().unwrap(), file));
        let Some(exe) = exe.filter(|e| e.len() >= 0x800 && &e[..8] == b"PS-X EXE") else {
            self.note_unknown(0xa0, 0x51);
            return self.ret(0);
        };
        let word = |at: usize| u32::from_le_bytes(exe[at..at + 4].try_into().unwrap());
        for i in 0..10 {
            self.store32(EXEC_HEADER + 4 * i, word(0x10 + 4 * i as usize));
        }
        self.store32(EXEC_HEADER + 0x20, stack);
        self.store32(EXEC_HEADER + 0x24, offset);
        let (start, size) = (word(0x18), word(0x1c).min(exe.len() as u32 - 0x800));
        for i in (0..size).step_by(4) {
            let at = 0x800 + i as usize;
            let w = exe.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            self.store32(start + i, w);
        }
        self.exec(EXEC_HEADER, 1, 0);
    }

    /// DoExecute(header, a0, a1): clears the BSS, sets the stack if the
    /// header has one, keeps ra, sp, fp, gp and s0 in the header, and calls
    /// the program, which returns to EXEC_RETURN.
    fn exec(&mut self, header: u32, a0: u32, a1: u32) {
        let word = |psx: &Psx, offset: u32| psx.peek32(header + offset);
        let (pc, gp, bss, bss_size, stack, stack_size) =
            (word(self, 0), word(self, 4), word(self, 0x18), word(self, 0x1c), word(self, 0x20), word(self, 0x24));
        for (offset, n) in [(0x34, 31), (0x28, 29), (0x2c, 30), (0x30, 28), (0x38, 16)] {
            self.store32(header + offset, self.cpu.r[n]);
        }
        self.fill(bss, 0, bss_size);
        if stack != 0 {
            self.cpu.r[29] = stack.wrapping_add(stack_size);
            self.cpu.r[30] = self.cpu.r[29];
        }
        self.cpu.r[16] = header;
        self.cpu.r[28] = gp;
        self.cpu.r[5] = a1;
        self.cpu.r[31] = rom(EXEC_RETURN);
        self.cpu.r[4] = a0;
        self.jump(pc);
    }

    /// A heap is blocks one after another, each a word of its size (with
    /// the word) and 1 when in use, then what it holds.
    fn init_heap(&mut self, heap: usize, addr: u32, size: u32) {
        let start = (addr + 3) & !3;
        let end = (addr + size) & !3;
        self.hle.heaps[heap] = (start, end);
        if end > start + 4 {
            self.store32(start, end - start);
        }
    }

    fn malloc(&mut self, heap: usize, size: u32) -> u32 {
        let (start, end) = self.hle.heaps[heap];
        let need = size.max(1).next_multiple_of(4) + 4;
        let mut at = start;
        while at + 4 <= end {
            let header = self.peek32(at);
            let mut length = header & !3;
            if length == 0 || at + length > end {
                return 0;
            }
            if header & 1 == 0 {
                // Free blocks after this one join it.
                loop {
                    let next = at + length;
                    let h = self.peek32(next);
                    if next + 4 > end || h & 1 != 0 || h & !3 == 0 {
                        break;
                    }
                    length += h & !3;
                }
                self.store32(at, length);
                if length >= need {
                    if length - need >= 8 {
                        self.store32(at + need, length - need);
                        length = need;
                    }
                    self.store32(at, length | 1);
                    return at + 4;
                }
            }
            at += length;
        }
        0
    }

    fn free(&mut self, heap: usize, block: u32) {
        let (start, end) = self.hle.heaps[heap];
        if block >= start + 4 && block < end {
            let header = self.peek32(block - 4);
            self.store32(block - 4, header & !1);
        }
    }

    fn realloc(&mut self, block: u32, size: u32) -> u32 {
        if block == 0 {
            return self.malloc(PROGRAM, size);
        }
        if size == 0 {
            self.free(PROGRAM, block);
            return 0;
        }
        let new = self.malloc(PROGRAM, size);
        if new != 0 {
            let old = (self.peek32(block - 4) & !3) - 4;
            self.copy(new, block, old.min(size));
            self.free(PROGRAM, block);
        }
        new
    }

    pub(crate) fn peek8(&self, addr: u32) -> u8 {
        (self.peek32(addr & !3) >> (8 * (addr & 3))) as u8
    }

    fn copy(&mut self, dst: u32, src: u32, n: u32) {
        let bytes: Vec<u8> = (0..n).map(|i| self.peek8(src.wrapping_add(i))).collect();
        for (i, b) in bytes.into_iter().enumerate() {
            self.store8(dst.wrapping_add(i as u32), b);
        }
    }

    fn fill(&mut self, dst: u32, value: u8, n: u32) {
        for i in 0..n {
            self.store8(dst.wrapping_add(i), value);
        }
    }

    fn compare(&self, a: u32, b: u32, n: u32) -> u32 {
        (0..n)
            .map(|i| i32::from(self.peek8(a + i)) - i32::from(self.peek8(b + i)))
            .find(|&d| d != 0)
            .unwrap_or(0) as u32
    }

    /// Copies a string with its zero, at most n bytes (zeros after it to n
    /// when n is given, as strncpy).
    fn copy_string(&mut self, dst: u32, src: u32, n: u32) {
        let text = self.c_string(src);
        let len = (text.len() as u32).min(n);
        for i in 0..len {
            self.store8(dst + i, text[i as usize]);
        }
        let end = if n == u32::MAX { len + 1 } else { n };
        self.fill(dst + len, 0, end - len);
    }

    fn compare_strings(&self, a: u32, b: u32, n: u32) -> u32 {
        for i in 0..n {
            let (x, y) = (self.peek8(a + i), self.peek8(b + i));
            if x != y {
                return (i32::from(x) - i32::from(y)) as u32;
            }
            if x == 0 {
                break;
            }
        }
        0
    }

    fn atoi(&self, addr: u32) -> u32 {
        let text = self.c_string(addr);
        let text = String::from_utf8_lossy(&text);
        let text = text.trim_start();
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let value = digits.bytes().take_while(u8::is_ascii_digit).fold(0i32, |v, d| {
            v.wrapping_mul(10).wrapping_add(i32::from(d - b'0'))
        });
        (if negative { -value } else { value }) as u32
    }

    pub(crate) fn c_string(&self, mut addr: u32) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < 4096 {
            let c = self.peek8(addr);
            if c == 0 {
                break;
            }
            out.push(c);
            addr = addr.wrapping_add(1);
        }
        out
    }

    /// printf: the format at a0, the arguments in a1-a3 and then on the
    /// stack after the 16 bytes the caller keeps for the first four.
    fn printf(&mut self) -> Vec<u8> {
        let format = self.c_string(self.cpu.r[4]);
        let mut next_arg = 1;
        let mut arg = |psx: &mut Psx| {
            let i = next_arg;
            next_arg += 1;
            if i < 4 { psx.cpu.r[4 + i] } else { psx.peek32(psx.cpu.r[29].wrapping_add(4 * i as u32)) }
        };
        let mut out = Vec::new();
        let mut i = 0;
        while i < format.len() {
            let c = format[i];
            i += 1;
            if c != b'%' {
                out.push(c);
                continue;
            }
            // Flags, width, precision and a long modifier, then the kind.
            let (mut zero, mut left, mut alternate, mut width) = (false, false, false, 0usize);
            // The BIOS takes flags after the width too ("%2-d").
            while i < format.len() && matches!(format[i], b'0'..=b'9' | b'-' | b' ' | b'+' | b'#') {
                match format[i] {
                    b'0' if width == 0 => zero = true,
                    b'-' => left = true,
                    b'#' => alternate = true,
                    d @ b'0'..=b'9' => width = width * 10 + usize::from(d - b'0'),
                    _ => {}
                }
                i += 1;
            }
            if i < format.len() && format[i] == b'.' {
                i += 1;
                while i < format.len() && format[i].is_ascii_digit() {
                    i += 1;
                }
            }
            while i < format.len() && matches!(format[i], b'l' | b'h') {
                i += 1;
            }
            let Some(&kind) = format.get(i) else { break };
            i += 1;
            // With #, hexadecimal but zero has 0x before it.
            let prefix = |value: u32, x: &'static str| if alternate && value != 0 { x } else { "" };
            let text: Vec<u8> = match kind {
                b'd' | b'i' => (arg(self) as i32).to_string().into_bytes(),
                b'u' => arg(self).to_string().into_bytes(),
                b'x' | b'p' => {
                    let v = arg(self);
                    format!("{}{v:x}", prefix(v, "0x")).into_bytes()
                }
                b'X' => {
                    let v = arg(self);
                    format!("{}{v:X}", prefix(v, "0X")).into_bytes()
                }
                b'o' => format!("{:o}", arg(self)).into_bytes(),
                b'c' => vec![arg(self) as u8],
                b's' => {
                    let addr = arg(self);
                    self.c_string(addr)
                }
                b'%' => vec![b'%'],
                other => vec![b'%', other],
            };
            let pad = width.saturating_sub(text.len());
            if left {
                out.extend_from_slice(&text);
                out.extend(std::iter::repeat_n(b' ', pad));
            } else {
                let fill = if zero { b'0' } else { b' ' };
                out.extend(std::iter::repeat_n(fill, pad));
                out.extend_from_slice(&text);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    /// The kernel's pad buffers (InitPad) and PAD_init's word have the second
    /// port's pad while one is plugged in, and nothing there while not.
    #[test]
    fn a_second_pad_for_the_kernels_reader() {
        let mut psx = crate::Psx::blank();
        psx.hle.pad_buffers = [(0x8001_0000, 34), (0x8001_0100, 34)];
        psx.hle.pad_word = 0x8001_0200;
        psx.sio.pads = [crate::sio::buttons::CROSS, crate::sio::buttons::START];
        psx.read_pads();
        assert_eq!(psx.peek8(0x8001_0100), 0xff);
        assert_eq!(psx.peek32(0x8001_0200) >> 16, 0xffff);
        psx.sio.second_pad = true;
        psx.read_pads();
        let two: Vec<u8> = (0..4).map(|i| psx.peek8(0x8001_0100 + i)).collect();
        assert_eq!(two, [0, 0x41, !0x08, 0xff]);
        assert_eq!(psx.peek32(0x8001_0200), u32::from((!0x0008u16).swap_bytes()) << 16 | u32::from((!0x4000u16).swap_bytes()));
    }
}
