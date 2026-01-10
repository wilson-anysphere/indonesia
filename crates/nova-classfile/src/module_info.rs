use nova_modules::{Exports, ModuleInfo, ModuleKind, ModuleName, Opens, Provides, Requires, Uses};

use crate::constant_pool::{ConstantPool, CpInfo};
use crate::error::{Error, Result};
use crate::reader::Reader;

/// Parse a `module-info.class` file into a [`nova_modules::ModuleInfo`].
pub fn parse_module_info_class(bytes: &[u8]) -> Result<ModuleInfo> {
    let mut reader = Reader::new(bytes);
    let magic = reader.read_u4()?;
    if magic != 0xCAFEBABE {
        return Err(Error::InvalidMagic(magic));
    }

    let _minor_version = reader.read_u2()?;
    let _major_version = reader.read_u2()?;
    let cp = ConstantPool::parse(&mut reader)?;

    // access_flags, this_class, super_class
    let _access_flags = reader.read_u2()?;
    let _this_class = reader.read_u2()?;
    let _super_class = reader.read_u2()?;

    let interfaces_count = reader.read_u2()? as usize;
    for _ in 0..interfaces_count {
        reader.read_u2()?;
    }

    let fields_count = reader.read_u2()? as usize;
    for _ in 0..fields_count {
        skip_member(&mut reader)?;
    }

    let methods_count = reader.read_u2()? as usize;
    for _ in 0..methods_count {
        skip_member(&mut reader)?;
    }

    let attributes_count = reader.read_u2()? as usize;
    for _ in 0..attributes_count {
        let name_index = reader.read_u2()?;
        let length = reader.read_u4()? as usize;
        let info = reader.read_bytes(length)?;
        let name = cp.get_utf8(name_index)?;

        if name == "Module" {
            let mut sub = Reader::new(info);
            let module = parse_module_attribute(&mut sub, &cp)?;
            sub.ensure_empty()?;
            return Ok(module);
        }
    }

    Err(Error::Other("missing Module attribute"))
}

fn skip_member(reader: &mut Reader<'_>) -> Result<()> {
    reader.read_u2()?; // access_flags
    reader.read_u2()?; // name_index
    reader.read_u2()?; // descriptor_index
    skip_attributes(reader)?;
    Ok(())
}

fn skip_attributes(reader: &mut Reader<'_>) -> Result<()> {
    let attributes_count = reader.read_u2()? as usize;
    for _ in 0..attributes_count {
        reader.read_u2()?; // attribute_name_index
        let len = reader.read_u4()? as usize;
        reader.read_bytes(len)?;
    }
    Ok(())
}

fn parse_module_attribute(reader: &mut Reader<'_>, cp: &ConstantPool) -> Result<ModuleInfo> {
    const ACC_OPEN: u16 = 0x0020;
    const ACC_TRANSITIVE: u16 = 0x0020;
    const ACC_STATIC_PHASE: u16 = 0x0040;

    let module_name_index = reader.read_u2()?;
    let module_flags = reader.read_u2()?;
    let _module_version_index = reader.read_u2()?;

    let name = ModuleName::new(cp.get_module_name(module_name_index)?);
    let is_open = (module_flags & ACC_OPEN) != 0;

    let requires_count = reader.read_u2()? as usize;
    let mut requires = Vec::with_capacity(requires_count);
    for _ in 0..requires_count {
        let requires_index = reader.read_u2()?;
        let requires_flags = reader.read_u2()?;
        let _requires_version_index = reader.read_u2()?;
        requires.push(Requires {
            module: ModuleName::new(cp.get_module_name(requires_index)?),
            is_transitive: (requires_flags & ACC_TRANSITIVE) != 0,
            is_static: (requires_flags & ACC_STATIC_PHASE) != 0,
        });
    }

    let exports_count = reader.read_u2()? as usize;
    let mut exports = Vec::with_capacity(exports_count);
    for _ in 0..exports_count {
        let exports_index = reader.read_u2()?;
        let _exports_flags = reader.read_u2()?;
        let exports_to_count = reader.read_u2()? as usize;
        let package = cp.get_package_name(exports_index)?.replace('/', ".");
        let mut to = Vec::with_capacity(exports_to_count);
        for _ in 0..exports_to_count {
            let to_index = reader.read_u2()?;
            to.push(ModuleName::new(cp.get_module_name(to_index)?));
        }
        exports.push(Exports { package, to });
    }

    let opens_count = reader.read_u2()? as usize;
    let mut opens = Vec::with_capacity(opens_count);
    for _ in 0..opens_count {
        let opens_index = reader.read_u2()?;
        let _opens_flags = reader.read_u2()?;
        let opens_to_count = reader.read_u2()? as usize;
        let package = cp.get_package_name(opens_index)?.replace('/', ".");
        let mut to = Vec::with_capacity(opens_to_count);
        for _ in 0..opens_to_count {
            let to_index = reader.read_u2()?;
            to.push(ModuleName::new(cp.get_module_name(to_index)?));
        }
        opens.push(Opens { package, to });
    }

    let uses_count = reader.read_u2()? as usize;
    let mut uses = Vec::with_capacity(uses_count);
    for _ in 0..uses_count {
        let uses_index = reader.read_u2()?;
        let service = cp.get_class_name(uses_index)?.replace('/', ".");
        uses.push(Uses { service });
    }

    let provides_count = reader.read_u2()? as usize;
    let mut provides = Vec::with_capacity(provides_count);
    for _ in 0..provides_count {
        let service_index = reader.read_u2()?;
        let with_count = reader.read_u2()? as usize;
        let service = cp.get_class_name(service_index)?.replace('/', ".");
        let mut implementations = Vec::with_capacity(with_count);
        for _ in 0..with_count {
            let with_index = reader.read_u2()?;
            implementations.push(cp.get_class_name(with_index)?.replace('/', "."));
        }
        provides.push(Provides {
            service,
            implementations,
        });
    }

    Ok(ModuleInfo {
        kind: ModuleKind::Explicit,
        name,
        is_open,
        requires,
        exports,
        opens,
        uses,
        provides,
    })
}

// -----------------------------------------------------------------------------
// Constant pool helpers
// -----------------------------------------------------------------------------

trait ConstantPoolExt {
    fn get_module_name(&self, index: u16) -> Result<String>;
    fn get_package_name(&self, index: u16) -> Result<String>;
}

impl ConstantPoolExt for ConstantPool {
    fn get_module_name(&self, index: u16) -> Result<String> {
        match self.get(index)? {
            CpInfo::Module { name_index } => Ok(self.get_utf8(*name_index)?.to_string()),
            other => Err(Error::ConstantPoolTypeMismatch {
                index,
                expected: "Module",
                found: other.kind(),
            }),
        }
    }

    fn get_package_name(&self, index: u16) -> Result<String> {
        match self.get(index)? {
            CpInfo::Package { name_index } => Ok(self.get_utf8(*name_index)?.to_string()),
            other => Err(Error::ConstantPoolTypeMismatch {
                index,
                expected: "Package",
                found: other.kind(),
            }),
        }
    }
}
