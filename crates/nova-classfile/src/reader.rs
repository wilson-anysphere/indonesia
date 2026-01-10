use crate::error::{Error, Result};

#[derive(Clone, Copy)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    pub fn read_u1(&mut self) -> Result<u8> {
        let b = *self.bytes.get(self.pos).ok_or(Error::UnexpectedEof)?;
        self.pos += 1;
        Ok(b)
    }

    pub fn read_u2(&mut self) -> Result<u16> {
        let bytes = self.read_n::<2>()?;
        Ok(u16::from_be_bytes(bytes))
    }

    pub fn read_u4(&mut self) -> Result<u32> {
        let bytes = self.read_n::<4>()?;
        Ok(u32::from_be_bytes(bytes))
    }

    pub fn read_i4(&mut self) -> Result<i32> {
        Ok(self.read_u4()? as i32)
    }

    pub fn read_i8(&mut self) -> Result<i64> {
        let bytes = self.read_n::<8>()?;
        Ok(i64::from_be_bytes(bytes))
    }

    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or(Error::UnexpectedEof)?;
        if end > self.bytes.len() {
            return Err(Error::UnexpectedEof);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn ensure_empty(&self) -> Result<()> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(Error::Other("unexpected trailing bytes"))
        }
    }

    fn read_n<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.pos.checked_add(N).ok_or(Error::UnexpectedEof)?;
        if end > self.bytes.len() {
            return Err(Error::UnexpectedEof);
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.pos..end]);
        self.pos = end;
        Ok(out)
    }
}
