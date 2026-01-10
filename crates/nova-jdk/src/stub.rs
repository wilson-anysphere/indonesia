#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdkFieldStub {
    pub access_flags: u16,
    pub name: String,
    /// JVM descriptor, e.g. `I` or `Ljava/lang/String;`.
    pub descriptor: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdkMethodStub {
    pub access_flags: u16,
    pub name: String,
    /// JVM method descriptor, e.g. `(Ljava/lang/String;)V`.
    pub descriptor: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdkClassStub {
    /// Internal name, e.g. `java/lang/String`.
    pub internal_name: String,
    /// Binary name, e.g. `java.lang.String`.
    pub binary_name: String,
    pub access_flags: u16,
    pub super_internal_name: Option<String>,
    pub interfaces_internal_names: Vec<String>,
    pub fields: Vec<JdkFieldStub>,
    pub methods: Vec<JdkMethodStub>,
}

impl JdkClassStub {
    pub fn package_name(&self) -> Option<&str> {
        self.binary_name.rsplit_once('.').map(|(p, _)| p)
    }

    pub fn simple_name(&self) -> &str {
        self.binary_name
            .rsplit_once('.')
            .map(|(_, s)| s)
            .unwrap_or(&self.binary_name)
    }
}

pub(crate) fn internal_to_binary(internal: &str) -> String {
    internal.replace('/', ".")
}

pub(crate) fn binary_to_internal(binary: &str) -> String {
    binary.replace('.', "/")
}

