//! Save states for Quick Resume: the whole machine as bytes. Each part
//! writes its own fields in a fixed order; a value is its tag and number.
//! The cartridge's code is not in a state: it is compiled again from the
//! cartridge, the same way, so its functions and constant strings have the
//! same numbers they had.

use crate::heap::Value;

#[derive(Debug)]
pub struct BadState(pub &'static str);

pub type Res<T> = Result<T, BadState>;

#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.0.push(v as u8);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
    pub fn len(&mut self, n: usize) {
        self.u32(n as u32);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
    pub fn value(&mut self, v: Value) {
        let (tag, n) = match v {
            Value::Nil => (0, 0),
            Value::Bool(b) => (1, b as u32),
            Value::Num(n) => (2, n as u32),
            Value::Str(i) => (3, i),
            Value::Table(i) => (4, i),
            Value::Func(i) => (5, i),
            Value::Native(i) => (6, u32::from(i)),
            Value::Thread(i) => (7, i),
            Value::Cell(i) => (8, i),
        };
        self.u8(tag);
        self.u32(n);
    }
    pub fn values(&mut self, v: &[Value]) {
        self.len(v.len());
        for &x in v {
            self.value(x);
        }
    }
    pub fn opt_u32(&mut self, v: Option<u32>) {
        self.bool(v.is_some());
        self.u32(v.unwrap_or(0));
    }
}

pub struct R<'a> {
    data: &'a [u8],
    /// The largest index a value may have (the heap's slots).
    pub limit: u32,
}

impl<'a> R<'a> {
    pub fn new(data: &'a [u8]) -> R<'a> {
        R { data, limit: u32::MAX }
    }
    pub fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        if self.data.len() < n {
            return Err(BadState("cut short"));
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }
    pub fn u8(&mut self) -> Res<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Res<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Res<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Res<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn i32(&mut self) -> Res<i32> {
        Ok(self.u32()? as i32)
    }
    pub fn u64(&mut self) -> Res<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn f32(&mut self) -> Res<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    pub fn len(&mut self) -> Res<usize> {
        let n = self.u32()? as usize;
        if n > self.data.len().max(1 << 16) * 8 {
            return Err(BadState("a length past the end"));
        }
        Ok(n)
    }
    pub fn bytes(&mut self) -> Res<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }
    pub fn value(&mut self) -> Res<Value> {
        let tag = self.u8()?;
        let n = self.u32()?;
        let index = |n: u32| if n < self.limit { Ok(n) } else { Err(BadState("a value out of the heap")) };
        Ok(match tag {
            0 => Value::Nil,
            1 => Value::Bool(n != 0),
            2 => Value::Num(n as i32),
            3 => Value::Str(index(n)?),
            4 => Value::Table(index(n)?),
            5 => Value::Func(index(n)?),
            6 => Value::Native(u16::try_from(n).map_err(|_| BadState("a native out of range"))?),
            7 => Value::Thread(index(n)?),
            8 => Value::Cell(index(n)?),
            _ => return Err(BadState("a value of no kind")),
        })
    }
    pub fn values(&mut self) -> Res<Vec<Value>> {
        let n = self.len()?;
        (0..n).map(|_| self.value()).collect()
    }
    pub fn opt_u32(&mut self) -> Res<Option<u32>> {
        let some = self.bool()?;
        let v = self.u32()?;
        Ok(some.then_some(v))
    }
    pub fn done(&self) -> bool {
        self.data.is_empty()
    }
}
