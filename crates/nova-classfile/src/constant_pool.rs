use crate::error::{Error, Result};
use crate::reader::Reader;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum CpInfo {
    Utf8(String),
    Integer(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    Class { name_index: u16 },
    String { string_index: u16 },
    Fieldref {
        class_index: u16,
        name_and_type_index: u16,
    },
    Methodref {
        class_index: u16,
        name_and_type_index: u16,
    },
    InterfaceMethodref {
        class_index: u16,
        name_and_type_index: u16,
    },
    NameAndType {
        name_index: u16,
        descriptor_index: u16,
    },
    MethodHandle {
        reference_kind: u8,
        reference_index: u16,
    },
    MethodType { descriptor_index: u16 },
    Dynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },
    InvokeDynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },
    Module { name_index: u16 },
    Package { name_index: u16 },
}

impl CpInfo {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            CpInfo::Utf8(_) => "Utf8",
            CpInfo::Integer(_) => "Integer",
            CpInfo::Float(_) => "Float",
            CpInfo::Long(_) => "Long",
            CpInfo::Double(_) => "Double",
            CpInfo::Class { .. } => "Class",
            CpInfo::String { .. } => "String",
            CpInfo::Fieldref { .. } => "Fieldref",
            CpInfo::Methodref { .. } => "Methodref",
            CpInfo::InterfaceMethodref { .. } => "InterfaceMethodref",
            CpInfo::NameAndType { .. } => "NameAndType",
            CpInfo::MethodHandle { .. } => "MethodHandle",
            CpInfo::MethodType { .. } => "MethodType",
            CpInfo::Dynamic { .. } => "Dynamic",
            CpInfo::InvokeDynamic { .. } => "InvokeDynamic",
            CpInfo::Module { .. } => "Module",
            CpInfo::Package { .. } => "Package",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConstantPool {
    entries: Vec<Option<CpInfo>>,
}

impl ConstantPool {
    pub fn parse(reader: &mut Reader<'_>) -> Result<Self> {
        let count = reader.read_u2()? as usize;
        if count == 0 {
            return Err(Error::Other("constant_pool_count must be >= 1"));
        }

        let mut entries = vec![None; count];
        let mut i = 1usize;
        while i < count {
            let tag = reader.read_u1()?;
            let entry = match tag {
                1 => {
                    let len = reader.read_u2()? as usize;
                    let bytes = reader.read_bytes(len)?;
                    CpInfo::Utf8(decode_modified_utf8(bytes)?)
                }
                3 => CpInfo::Integer(reader.read_i4()?),
                4 => {
                    let bits = reader.read_u4()?;
                    CpInfo::Float(f32::from_bits(bits))
                }
                5 => {
                    let value = reader.read_i8()?;
                    CpInfo::Long(value)
                }
                6 => {
                    let bits = reader.read_i8()? as u64;
                    CpInfo::Double(f64::from_bits(bits))
                }
                7 => CpInfo::Class {
                    name_index: reader.read_u2()?,
                },
                8 => CpInfo::String {
                    string_index: reader.read_u2()?,
                },
                9 => CpInfo::Fieldref {
                    class_index: reader.read_u2()?,
                    name_and_type_index: reader.read_u2()?,
                },
                10 => CpInfo::Methodref {
                    class_index: reader.read_u2()?,
                    name_and_type_index: reader.read_u2()?,
                },
                11 => CpInfo::InterfaceMethodref {
                    class_index: reader.read_u2()?,
                    name_and_type_index: reader.read_u2()?,
                },
                12 => CpInfo::NameAndType {
                    name_index: reader.read_u2()?,
                    descriptor_index: reader.read_u2()?,
                },
                15 => CpInfo::MethodHandle {
                    reference_kind: reader.read_u1()?,
                    reference_index: reader.read_u2()?,
                },
                16 => CpInfo::MethodType {
                    descriptor_index: reader.read_u2()?,
                },
                17 => CpInfo::Dynamic {
                    bootstrap_method_attr_index: reader.read_u2()?,
                    name_and_type_index: reader.read_u2()?,
                },
                18 => CpInfo::InvokeDynamic {
                    bootstrap_method_attr_index: reader.read_u2()?,
                    name_and_type_index: reader.read_u2()?,
                },
                19 => CpInfo::Module {
                    name_index: reader.read_u2()?,
                },
                20 => CpInfo::Package {
                    name_index: reader.read_u2()?,
                },
                other => return Err(Error::InvalidConstantPoolTag(other)),
            };

            entries[i] = Some(entry);

            // Long/Double take up two slots.
            match entries[i].as_ref().unwrap() {
                CpInfo::Long(_) | CpInfo::Double(_) => {
                    if i + 1 >= count {
                        return Err(Error::Other("malformed constant pool"));
                    }
                    i += 2;
                }
                _ => i += 1,
            }
        }

        Ok(Self { entries })
    }

    pub fn get(&self, index: u16) -> Result<&CpInfo> {
        let idx = index as usize;
        if idx == 0 || idx >= self.entries.len() {
            return Err(Error::InvalidConstantPoolIndex(index));
        }
        self.entries[idx]
            .as_ref()
            .ok_or(Error::InvalidConstantPoolIndex(index))
    }

    pub fn get_utf8(&self, index: u16) -> Result<&str> {
        match self.get(index)? {
            CpInfo::Utf8(s) => Ok(s.as_str()),
            other => Err(Error::ConstantPoolTypeMismatch {
                index,
                expected: "Utf8",
                found: other.kind(),
            }),
        }
    }

    pub fn get_class_name(&self, index: u16) -> Result<String> {
        match self.get(index)? {
            CpInfo::Class { name_index } => Ok(self.get_utf8(*name_index)?.to_string()),
            other => Err(Error::ConstantPoolTypeMismatch {
                index,
                expected: "Class",
                found: other.kind(),
            }),
        }
    }
}

fn decode_modified_utf8(bytes: &[u8]) -> Result<String> {
    // Modified UTF-8 as used in class files is essentially UTF-8 for the BMP plus:
    // - NUL encoded as 0xC0 0x80
    // - Supplementary characters encoded as surrogate pairs (CESU-8 style)
    //
    // We decode into UTF-16 code units and then convert via from_utf16.
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b & 0x80 == 0 {
            units.push(b as u16);
            i += 1;
            continue;
        }

        if (b & 0xE0) == 0xC0 {
            if i + 1 >= bytes.len() {
                return Err(Error::InvalidModifiedUtf8);
            }
            let b2 = bytes[i + 1];
            if b == 0xC0 && b2 == 0x80 {
                units.push(0);
            } else {
                if (b2 & 0xC0) != 0x80 {
                    return Err(Error::InvalidModifiedUtf8);
                }
                let value = (((b & 0x1F) as u16) << 6) | ((b2 & 0x3F) as u16);
                units.push(value);
            }
            i += 2;
            continue;
        }

        if (b & 0xF0) == 0xE0 {
            if i + 2 >= bytes.len() {
                return Err(Error::InvalidModifiedUtf8);
            }
            let b2 = bytes[i + 1];
            let b3 = bytes[i + 2];
            if (b2 & 0xC0) != 0x80 || (b3 & 0xC0) != 0x80 {
                return Err(Error::InvalidModifiedUtf8);
            }
            let value = (((b & 0x0F) as u16) << 12)
                | (((b2 & 0x3F) as u16) << 6)
                | ((b3 & 0x3F) as u16);
            units.push(value);
            i += 3;
            continue;
        }

        // Modified UTF-8 never uses 4-byte sequences.
        return Err(Error::InvalidModifiedUtf8);
    }

    // Classfile modified UTF-8 encodes a sequence of UTF-16 code units. Java
    // strings/identifiers may legally contain unpaired surrogate values, so use
    // lossy decoding instead of rejecting the entire classfile.
    Ok(String::from_utf16_lossy(&units))
}
