use super::types::{
    JdwpError, JdwpIdSizes, JdwpValue, Location, ObjectId, ReferenceTypeId, Result,
};

pub const HANDSHAKE: &[u8] = b"JDWP-Handshake";
pub const HEADER_LEN: usize = crate::JDWP_HEADER_LEN;
pub const FLAG_REPLY: u8 = 0x80;

pub fn signature_to_tag(signature: &str) -> u8 {
    signature.as_bytes().first().copied().unwrap_or(b'V')
}

/// Convert a Java binary class name (e.g. `com.example.Foo`) into a JDWP
/// reference type signature (e.g. `Lcom/example/Foo;`).
///
/// If `class` already looks like a JDWP signature (starts with `L` and ends with
/// `;`), it is returned unchanged.
#[allow(dead_code)]
pub fn class_name_to_signature(class: &str) -> String {
    if class.starts_with('L') && class.ends_with(';') {
        return class.to_string();
    }
    let internal = class.replace('.', "/");
    format!("L{internal};")
}

pub struct JdwpWriter {
    buf: Vec<u8>,
}

impl JdwpWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(if v { 1 } else { 0 });
    }

    #[allow(dead_code)]
    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[allow(dead_code)]
    pub fn write_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[allow(dead_code)]
    pub fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[allow(dead_code)]
    pub fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_string(&mut self, s: &str) {
        // JDWP strings are length-prefixed with a u32 number of bytes.
        self.write_u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn write_id(&mut self, id: u64, size: usize) {
        let be = id.to_be_bytes();
        self.buf.extend_from_slice(&be[8 - size..]);
    }

    pub fn write_object_id(&mut self, id: ObjectId, sizes: &JdwpIdSizes) {
        self.write_id(id, sizes.object_id);
    }

    pub fn write_tagged_object_id(&mut self, tag: u8, id: ObjectId, sizes: &JdwpIdSizes) {
        self.write_u8(tag);
        self.write_object_id(id, sizes);
    }

    pub fn write_reference_type_id(&mut self, id: ReferenceTypeId, sizes: &JdwpIdSizes) {
        self.write_id(id, sizes.reference_type_id);
    }

    pub fn write_location(&mut self, loc: &Location, sizes: &JdwpIdSizes) {
        self.write_u8(loc.type_tag);
        self.write_reference_type_id(loc.class_id, sizes);
        self.write_id(loc.method_id, sizes.method_id);
        self.write_u64(loc.index);
    }

    pub fn write_value(&mut self, v: &JdwpValue, sizes: &JdwpIdSizes) {
        match *v {
            JdwpValue::Boolean(v) => self.write_bool(v),
            JdwpValue::Byte(v) => self.write_u8(v as u8),
            JdwpValue::Char(v) => self.write_u16(v),
            JdwpValue::Short(v) => self.write_u16(v as u16),
            JdwpValue::Int(v) => self.write_i32(v),
            JdwpValue::Long(v) => self.write_i64(v),
            JdwpValue::Float(v) => self.write_f32(v),
            JdwpValue::Double(v) => self.write_f64(v),
            JdwpValue::Object { id, .. } => self.write_object_id(id, sizes),
            JdwpValue::Void => {}
        }
    }

    pub fn write_tagged_value(&mut self, v: &JdwpValue, sizes: &JdwpIdSizes) {
        let tag = match *v {
            JdwpValue::Boolean(_) => b'Z',
            JdwpValue::Byte(_) => b'B',
            JdwpValue::Char(_) => b'C',
            JdwpValue::Short(_) => b'S',
            JdwpValue::Int(_) => b'I',
            JdwpValue::Long(_) => b'J',
            JdwpValue::Float(_) => b'F',
            JdwpValue::Double(_) => b'D',
            JdwpValue::Object { tag, .. } => tag,
            JdwpValue::Void => b'V',
        };
        self.write_u8(tag);
        self.write_value(v, sizes);
    }
}

pub struct JdwpReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> JdwpReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn require(&self, n: usize) -> Result<()> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            JdwpError::Protocol(format!(
                "buffer underflow: need {n} bytes at {}, have {}",
                self.pos,
                self.buf.len()
            ))
        })?;

        if end > self.buf.len() {
            return Err(JdwpError::Protocol(format!(
                "buffer underflow: need {n} bytes at {}, have {}",
                self.pos,
                self.buf.len()
            )));
        }
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        self.require(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        self.require(2)?;
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        self.require(4)?;
        let v = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        self.require(4)?;
        let v = i32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        self.require(8)?;
        let v = u64::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
            self.buf[self.pos + 4],
            self.buf[self.pos + 5],
            self.buf[self.pos + 6],
            self.buf[self.pos + 7],
        ]);
        self.pos += 8;
        Ok(v)
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(self.read_u64()? as i64)
    }

    pub fn read_f32(&mut self) -> Result<f32> {
        self.require(4)?;
        let bits = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(f32::from_bits(bits))
    }

    pub fn read_f64(&mut self) -> Result<f64> {
        self.require(8)?;
        let bits = u64::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
            self.buf[self.pos + 4],
            self.buf[self.pos + 5],
            self.buf[self.pos + 6],
            self.buf[self.pos + 7],
        ]);
        self.pos += 8;
        Ok(f64::from_bits(bits))
    }

    pub fn read_string(&mut self) -> Result<String> {
        let len = self.read_u32()? as usize;
        self.require(len)?;
        let bytes = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate string buffer ({len} bytes)"))
        })?;
        out.extend_from_slice(bytes);
        String::from_utf8(out)
            .map_err(|e| JdwpError::Protocol(format!("invalid utf-8 string: {e}")))
    }

    #[allow(dead_code)]
    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        self.require(len)?;
        let bytes = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(bytes)
    }

    pub fn read_id(&mut self, size: usize) -> Result<u64> {
        self.require(size)?;
        if size == 0 || size > 8 {
            return Err(JdwpError::Protocol(format!("invalid id size: {size}")));
        }
        let mut be = [0u8; 8];
        be[8 - size..].copy_from_slice(&self.buf[self.pos..self.pos + size]);
        self.pos += size;
        Ok(u64::from_be_bytes(be))
    }

    pub fn read_object_id(&mut self, sizes: &JdwpIdSizes) -> Result<ObjectId> {
        self.read_id(sizes.object_id)
    }

    pub fn read_tagged_object_id(&mut self, sizes: &JdwpIdSizes) -> Result<(u8, ObjectId)> {
        let tag = self.read_u8()?;
        let id = self.read_object_id(sizes)?;
        Ok((tag, id))
    }

    pub fn read_reference_type_id(&mut self, sizes: &JdwpIdSizes) -> Result<ReferenceTypeId> {
        self.read_id(sizes.reference_type_id)
    }

    pub fn read_location(&mut self, sizes: &JdwpIdSizes) -> Result<Location> {
        Ok(Location {
            type_tag: self.read_u8()?,
            class_id: self.read_reference_type_id(sizes)?,
            method_id: self.read_id(sizes.method_id)?,
            index: self.read_u64()?,
        })
    }

    pub fn read_value(&mut self, tag: u8, sizes: &JdwpIdSizes) -> Result<JdwpValue> {
        let v = match tag {
            b'Z' => JdwpValue::Boolean(self.read_bool()?),
            b'B' => JdwpValue::Byte(self.read_u8()? as i8),
            b'C' => JdwpValue::Char(self.read_u16()?),
            b'S' => JdwpValue::Short(self.read_u16()? as i16),
            b'I' => JdwpValue::Int(self.read_i32()?),
            b'J' => JdwpValue::Long(self.read_i64()?),
            b'F' => JdwpValue::Float(self.read_f32()?),
            b'D' => JdwpValue::Double(self.read_f64()?),
            b'V' => JdwpValue::Void,
            // Object-like values are represented as an object id.
            _ => JdwpValue::Object {
                tag,
                id: self.read_object_id(sizes)?,
            },
        };
        Ok(v)
    }

    pub fn read_tagged_value(&mut self, sizes: &JdwpIdSizes) -> Result<JdwpValue> {
        let tag = self.read_u8()?;
        self.read_value(tag, sizes)
    }
}

#[allow(dead_code)]
pub fn encode_command(id: u32, command_set: u8, command: u8, payload: &[u8]) -> Vec<u8> {
    let length = (HEADER_LEN + payload.len()) as u32;
    let mut out = Vec::with_capacity(length as usize);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.push(0); // flags
    out.push(command_set);
    out.push(command);
    out.extend_from_slice(payload);
    out
}

#[allow(dead_code)]
pub fn encode_reply(id: u32, error_code: u16, payload: &[u8]) -> Vec<u8> {
    let length = (HEADER_LEN + payload.len()) as u32;
    let mut out = Vec::with_capacity(length as usize);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.push(FLAG_REPLY);
    out.extend_from_slice(&error_code.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_name_to_signature_converts_dots() {
        assert_eq!(
            class_name_to_signature("com.example.Foo"),
            "Lcom/example/Foo;"
        );
        assert_eq!(class_name_to_signature("Foo"), "LFoo;");
    }

    #[test]
    fn class_name_to_signature_passes_through_signatures() {
        assert_eq!(
            class_name_to_signature("Ljava/lang/String;"),
            "Ljava/lang/String;"
        );
    }
}
