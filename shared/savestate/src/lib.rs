//! Save states: a whole machine as bytes, for Quick Resume. Each part of
//! a core lists its fields with the `state!` macro, in its own module; what
//! a state does not hold (the cartridge's ROM, the last picture and sound)
//! comes back with the next frame or from the cartridge itself.
//!
//! `state!` lists are positional: a field added to one would move every
//! byte after it, and states saved before would no longer load. A core
//! whose states must outlive such changes freezes its lists and puts what
//! it adds later in named records after them (`records!`): each record is
//! its name, its length and its bytes, so a state without a record loads
//! with that field's default, and a record a build does not know is
//! skipped.

/// A state that does not load: cut short, or with a value that cannot be.
#[derive(Debug, PartialEq)]
pub struct BadState;

pub trait State {
    fn save(&self, out: &mut Vec<u8>);
    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState>;
}

/// Takes n bytes off the front of the input.
pub fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], BadState> {
    if input.len() < n {
        return Err(BadState);
    }
    let (head, rest) = input.split_at(n);
    *input = rest;
    Ok(head)
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl State for $t {
            fn save(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
                *self = <$t>::from_le_bytes(take(input, size_of::<$t>())?.try_into().unwrap());
                Ok(())
            }
        }
    )*};
}

numbers!(u8, u16, u32, u64, u128, i8, i16, i32, i64, f32, f64);

impl State for bool {
    fn save(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        *self = match take(input, 1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(BadState),
        };
        Ok(())
    }
}

impl State for usize {
    fn save(&self, out: &mut Vec<u8>) {
        (*self as u32).save(out);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        let mut n = 0u32;
        n.load(input)?;
        *self = n as usize;
        Ok(())
    }
}

impl<T: State, const N: usize> State for [T; N] {
    fn save(&self, out: &mut Vec<u8>) {
        self.iter().for_each(|x| x.save(out));
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.iter_mut().try_for_each(|x| x.load(input))
    }
}

impl<A: State, B: State> State for (A, B) {
    fn save(&self, out: &mut Vec<u8>) {
        self.0.save(out);
        self.1.save(out);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.0.load(input)?;
        self.1.load(input)
    }
}

impl<T: State + Default> State for Option<T> {
    fn save(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(x) => {
                out.push(1);
                x.save(out);
            }
        }
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        *self = match take(input, 1)?[0] {
            0 => None,
            1 => {
                let mut x = T::default();
                x.load(input)?;
                Some(x)
            }
            _ => return Err(BadState),
        };
        Ok(())
    }
}

/// Memory the cartridge brought (work RAM, CHR RAM): its size must match.
impl State for Vec<u8> {
    fn save(&self, out: &mut Vec<u8>) {
        self.len().save(out);
        out.extend_from_slice(self);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        let mut len = 0usize;
        len.load(input)?;
        if len != self.len() {
            return Err(BadState);
        }
        self.copy_from_slice(take(input, len)?);
        Ok(())
    }
}

/// Video memory as halfwords: its size must match too.
impl State for Vec<u16> {
    fn save(&self, out: &mut Vec<u8>) {
        self.len().save(out);
        self.iter().for_each(|x| x.save(out));
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        let mut len = 0usize;
        len.load(input)?;
        if len != self.len() {
            return Err(BadState);
        }
        self.iter_mut().try_for_each(|x| x.load(input))
    }
}

/// Fixed memory (a console's RAM): saved as it is.
impl<const N: usize> State for Box<[u8; N]> {
    fn save(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self[..]);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.copy_from_slice(take(input, N)?);
        Ok(())
    }
}

/// Fixed halfword memory (a console's VRAM).
impl<const N: usize> State for Box<[u16; N]> {
    fn save(&self, out: &mut Vec<u8>) {
        self.iter().for_each(|x| x.save(out));
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.iter_mut().try_for_each(|x| x.load(input))
    }
}

/// Lists a struct's fields that make up its state.
#[macro_export]
macro_rules! state {
    ($t:ty { $($field:ident),* $(,)? }) => {
        impl $crate::State for $t {
            fn save(&self, out: &mut Vec<u8>) {
                $($crate::State::save(&self.$field, out);)*
            }

            fn load(&mut self, input: &mut &[u8]) -> Result<(), $crate::BadState> {
                $($crate::State::load(&mut self.$field, input)?;)*
                Ok(())
            }
        }
    };
}

/// Writes a named record: the name (its length in a byte, then its bytes),
/// the value's length (four bytes) and the value.
pub fn put_record(out: &mut Vec<u8>, name: &str, value: &dyn State) {
    assert!(name.len() <= 255, "a record's name is at most 255 bytes");
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    value.save(out);
    let len = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
}

/// Takes a record off the front of the input: its name and its value's
/// bytes.
pub fn take_record<'a>(input: &mut &'a [u8]) -> Result<(&'a [u8], &'a [u8]), BadState> {
    let name_len = take(input, 1)?[0] as usize;
    let name = take(input, name_len)?;
    let mut len = 0u32;
    len.load(input)?;
    Ok((name, take(input, len as usize)?))
}

/// Whether no two names are the same: `records!` checks its names with it
/// as it compiles.
pub const fn distinct(names: &[&str]) -> bool {
    let mut i = 0;
    while i < names.len() {
        let mut j = i + 1;
        while j < names.len() {
            let (a, b) = (names[i].as_bytes(), names[j].as_bytes());
            let mut k = 0;
            while k < a.len() && a.len() == b.len() && a[k] == b[k] {
                k += 1;
            }
            if a.len() == b.len() && k == a.len() {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

/// Lists fields saved as named records (see the crate's notes), each with
/// its name, where it is and the value it takes when a state has no record
/// of it: `records!(Machine { "cpu.latch": cpu.latch = 0, ... })`. Gives the
/// type `save_records`, which writes how many records there are and then
/// each; `load_records`, which sets the fields to their defaults and reads
/// the records back; and `reset_records`, the defaults alone, for a state
/// from before there were records. A name is never reused for something
/// else.
#[macro_export]
macro_rules! records {
    ($t:ty { $($name:literal : $($field:ident).+ = $default:expr),* $(,)? }) => {
        const _: () = assert!($crate::distinct(&[$($name),*]), "two records with one name");

        impl $t {
            fn save_records(&self, out: &mut Vec<u8>) {
                $crate::State::save(&(<[&str]>::len(&[$($name),*]) as u32), out);
                $($crate::put_record(out, $name, &self.$($field).+);)*
            }

            fn reset_records(&mut self) {
                $(self.$($field).+ = $default;)*
            }

            fn load_records(&mut self, input: &mut &[u8]) -> Result<(), $crate::BadState> {
                self.reset_records();
                let mut count = 0u32;
                $crate::State::load(&mut count, input)?;
                for _ in 0..count {
                    #[allow(unused_variables, unused_mut)]
                    let (name, mut value) = $crate::take_record(input)?;
                    $(if name == $name.as_bytes() {
                        $crate::State::load(&mut self.$($field).+, &mut value)?;
                        if !value.is_empty() {
                            return Err($crate::BadState);
                        }
                    })*
                }
                Ok(())
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Inner {
        b: u16,
    }

    #[derive(Default)]
    struct Machine {
        inner: Inner,
        c: u64,
    }

    records!(Machine { "inner.b": inner.b = 7, "c": c = 9 });

    struct Empty;

    records!(Empty {});

    fn load(machine: &mut Machine, mut input: &[u8]) -> Result<(), BadState> {
        machine.load_records(&mut input)?;
        if input.is_empty() { Ok(()) } else { Err(BadState) }
    }

    #[test]
    fn records_load_by_name_with_defaults_for_the_missing() {
        let machine = Machine { inner: Inner { b: 2 }, c: 3 };
        let mut out = Vec::new();
        machine.save_records(&mut out);
        assert_eq!(out, [2, 0, 0, 0, 7, b'i', b'n', b'n', b'e', b'r', b'.', b'b', 2, 0, 0, 0, 2, 0,
            1, b'c', 8, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0]);

        let mut other = Machine::default();
        load(&mut other, &out).unwrap();
        assert_eq!((other.inner.b, other.c), (2, 3));
        // No records: the defaults, whatever the machine held.
        load(&mut other, &[0; 4]).unwrap();
        assert_eq!((other.inner.b, other.c), (7, 9));
        other.c = 1;
        other.reset_records();
        assert_eq!((other.inner.b, other.c), (7, 9));
        // A record this build does not know is skipped.
        let mut unknown = vec![2, 0, 0, 0];
        put_record(&mut unknown, "later", &5u8);
        unknown.extend_from_slice(&out[18..]);
        load(&mut other, &unknown).unwrap();
        assert_eq!((other.inner.b, other.c), (7, 3));
        // Cut short anywhere, even between records, or a record longer than
        // its field: refused.
        for len in 0..out.len() {
            assert_eq!(load(&mut other, &out[..len]), Err(BadState), "{len}");
        }
        let mut long = vec![1, 0, 0, 0];
        put_record(&mut long, "c", &(1u64, 2u8));
        assert_eq!(load(&mut other, &long), Err(BadState));

        let mut none = Vec::new();
        Empty.save_records(&mut none);
        assert_eq!(none, [0; 4]);
        // A build with no records skips all.
        Empty.load_records(&mut &unknown[..]).unwrap();
    }

    #[test]
    fn names_are_distinct() {
        assert!(distinct(&[]) && distinct(&["a", "ab", "b", "ba"]));
        assert!(!distinct(&["a", "ab", "a"]) && !distinct(&["cpu.cop2", "x", "cpu.cop2"]));
    }
}
