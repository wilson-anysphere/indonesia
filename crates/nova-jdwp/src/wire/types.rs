use std::fmt;

use thiserror::Error;

pub type ObjectId = u64;
pub type ThreadId = u64;
pub type ThreadGroupId = u64;
pub type ReferenceTypeId = u64;
pub type MethodId = u64;
pub type FieldId = u64;
pub type FrameId = u64;

/// Thread status as reported by `ThreadReference.Status` (command 11/4).
///
/// The JDWP spec defines a small, fixed set of statuses, but VMs may return
/// values outside that set (future extensions or vendor-specific values).
/// `ThreadStatus::Other` preserves the raw value for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    Zombie,
    Running,
    Sleeping,
    Monitor,
    Wait,
    NotStarted,
    Other(u32),
}

impl ThreadStatus {
    pub fn as_raw(self) -> u32 {
        match self {
            ThreadStatus::Zombie => 0,
            ThreadStatus::Running => 1,
            ThreadStatus::Sleeping => 2,
            ThreadStatus::Monitor => 3,
            ThreadStatus::Wait => 4,
            ThreadStatus::NotStarted => 5,
            ThreadStatus::Other(v) => v,
        }
    }
}

impl From<u32> for ThreadStatus {
    fn from(value: u32) -> Self {
        match value {
            0 => ThreadStatus::Zombie,
            1 => ThreadStatus::Running,
            2 => ThreadStatus::Sleeping,
            3 => ThreadStatus::Monitor,
            4 => ThreadStatus::Wait,
            5 => ThreadStatus::NotStarted,
            other => ThreadStatus::Other(other),
        }
    }
}

impl From<ThreadStatus> for u32 {
    fn from(value: ThreadStatus) -> Self {
        value.as_raw()
    }
}

/// Suspension bitflags as reported by `ThreadReference.Status` (command 11/4).
///
/// The only standardized flag is `SUSPENDED` (bit 0), but VMs may return
/// additional bits in the future.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SuspendStatus {
    bits: u32,
}

impl SuspendStatus {
    pub const SUSPENDED: u32 = 0x1;

    pub fn new(bits: u32) -> Self {
        Self { bits }
    }

    pub fn bits(self) -> u32 {
        self.bits
    }

    pub fn is_suspended(self) -> bool {
        (self.bits & Self::SUSPENDED) != 0
    }
}

impl From<u32> for SuspendStatus {
    fn from(bits: u32) -> Self {
        Self { bits }
    }
}

impl From<SuspendStatus> for u32 {
    fn from(status: SuspendStatus) -> Self {
        status.bits
    }
}

/// Reply payload of `ObjectReference.MonitorInfo` (command 9/5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The thread that currently owns the monitor, or 0 if there is no owner.
    pub owner: ThreadId,
    /// The number of times the owning thread has entered the monitor.
    pub entry_count: i32,
    /// Threads currently waiting on the monitor (`Object.wait()`).
    pub waiters: Vec<ThreadId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JdwpIdSizes {
    pub field_id: usize,
    pub method_id: usize,
    pub object_id: usize,
    pub reference_type_id: usize,
    pub frame_id: usize,
}

impl Default for JdwpIdSizes {
    fn default() -> Self {
        // Most modern JVMs use 8 byte IDs.
        Self {
            field_id: 8,
            method_id: 8,
            object_id: 8,
            reference_type_id: 8,
            frame_id: 8,
        }
    }
}

/// Typed view of `VirtualMachine.CapabilitiesNew` (command 1/17).
///
/// The JDWP spec defines a fixed-order list of booleans returned by the target VM.
/// Keeping this as a struct (instead of a raw `Vec<bool>`) makes capability checks
/// self-documenting and less error-prone.
///
/// Note: historically HotSpot returns 32 booleans. The Nova wire client is
/// defensive and treats missing entries as `false` so older/partial
/// implementations degrade gracefully.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JdwpCapabilitiesNew {
    pub can_watch_field_modification: bool,
    pub can_watch_field_access: bool,
    pub can_get_bytecodes: bool,
    pub can_get_synthetic_attribute: bool,
    pub can_get_owned_monitor_info: bool,
    pub can_get_current_contended_monitor: bool,
    pub can_get_monitor_info: bool,
    pub can_redefine_classes: bool,
    pub can_add_method: bool,
    pub can_unrestrictedly_redefine_classes: bool,
    pub can_pop_frames: bool,
    pub can_use_instance_filters: bool,
    pub can_get_source_debug_extension: bool,
    pub can_request_vm_death_event: bool,
    pub can_set_default_stratum: bool,
    pub can_get_instance_info: bool,
    pub can_request_monitor_events: bool,
    pub can_get_monitor_frame_info: bool,
    pub can_use_source_name_filters: bool,
    pub can_get_constant_pool: bool,
    pub can_force_early_return: bool,
    pub can_get_owned_monitor_stack_depth_info: bool,
    pub can_get_method_return_values: bool,

    // The JDWP spec includes additional capability booleans beyond the commonly
    // used set above. We keep them as reserved fields so we preserve the on-wire
    // order even if Nova doesn't use them yet.
    #[allow(dead_code)]
    pub reserved_23: bool,
    #[allow(dead_code)]
    pub reserved_24: bool,
    #[allow(dead_code)]
    pub reserved_25: bool,
    #[allow(dead_code)]
    pub reserved_26: bool,
    #[allow(dead_code)]
    pub reserved_27: bool,
    #[allow(dead_code)]
    pub reserved_28: bool,
    #[allow(dead_code)]
    pub reserved_29: bool,
    #[allow(dead_code)]
    pub reserved_30: bool,
    #[allow(dead_code)]
    pub reserved_31: bool,
}

impl JdwpCapabilitiesNew {
    pub fn from_vec(v: Vec<bool>) -> Self {
        fn get(v: &[bool], idx: usize) -> bool {
            v.get(idx).copied().unwrap_or(false)
        }

        Self {
            can_watch_field_modification: get(&v, 0),
            can_watch_field_access: get(&v, 1),
            can_get_bytecodes: get(&v, 2),
            can_get_synthetic_attribute: get(&v, 3),
            can_get_owned_monitor_info: get(&v, 4),
            can_get_current_contended_monitor: get(&v, 5),
            can_get_monitor_info: get(&v, 6),
            can_redefine_classes: get(&v, 7),
            can_add_method: get(&v, 8),
            can_unrestrictedly_redefine_classes: get(&v, 9),
            can_pop_frames: get(&v, 10),
            can_use_instance_filters: get(&v, 11),
            can_get_source_debug_extension: get(&v, 12),
            can_request_vm_death_event: get(&v, 13),
            can_set_default_stratum: get(&v, 14),
            can_get_instance_info: get(&v, 15),
            can_request_monitor_events: get(&v, 16),
            can_get_monitor_frame_info: get(&v, 17),
            can_use_source_name_filters: get(&v, 18),
            can_get_constant_pool: get(&v, 19),
            can_force_early_return: get(&v, 20),
            can_get_owned_monitor_stack_depth_info: get(&v, 21),
            can_get_method_return_values: get(&v, 22),
            reserved_23: get(&v, 23),
            reserved_24: get(&v, 24),
            reserved_25: get(&v, 25),
            reserved_26: get(&v, 26),
            reserved_27: get(&v, 27),
            reserved_28: get(&v, 28),
            reserved_29: get(&v, 29),
            reserved_30: get(&v, 30),
            reserved_31: get(&v, 31),
        }
    }

    pub fn supports_redefine_classes(&self) -> bool {
        self.can_redefine_classes || self.can_unrestrictedly_redefine_classes
    }

    pub fn supports_watchpoints(&self) -> bool {
        self.can_watch_field_access || self.can_watch_field_modification
    }

    pub fn supports_method_return_values(&self) -> bool {
        self.can_get_method_return_values
    }

    pub fn supports_monitor_info(&self) -> bool {
        self.can_get_monitor_info
    }

    pub fn supports_owned_monitor_info(&self) -> bool {
        self.can_get_owned_monitor_info
    }

    pub fn supports_current_contended_monitor(&self) -> bool {
        self.can_get_current_contended_monitor
    }

    pub fn supports_owned_monitor_stack_depth_info(&self) -> bool {
        self.can_get_owned_monitor_stack_depth_info
    }

    /// Maps the legacy `VirtualMachine.Capabilities` (command 1/12) boolean list
    /// into the subset of `CapabilitiesNew` fields that existed historically.
    ///
    /// The legacy reply contains fewer booleans than `CapabilitiesNew`; fields that
    /// are not present in the legacy reply are left as `false`.
    pub fn from_legacy_vec(v: Vec<bool>) -> Self {
        fn get(v: &[bool], idx: usize) -> bool {
            v.get(idx).copied().unwrap_or(false)
        }

        Self {
            can_watch_field_modification: get(&v, 0),
            can_watch_field_access: get(&v, 1),
            can_get_bytecodes: get(&v, 2),
            can_get_synthetic_attribute: get(&v, 3),
            can_get_owned_monitor_info: get(&v, 4),
            can_get_current_contended_monitor: get(&v, 5),
            can_get_monitor_info: get(&v, 6),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub type_tag: u8,
    pub class_id: ReferenceTypeId,
    pub method_id: MethodId,
    pub index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub ref_type_tag: u8,
    pub type_id: ReferenceTypeId,
    pub signature: String,
    pub status: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInfo {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInfoWithGeneric {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    pub generic_signature: Option<String>,
    pub mod_bits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub frame_id: FrameId,
    pub location: Location,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineTableEntry {
    pub code_index: u64,
    pub line: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTable {
    pub start: u64,
    pub end: u64,
    pub lines: Vec<LineTableEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableInfo {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    pub length: u32,
    pub slot: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableInfoWithGeneric {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    pub generic_signature: Option<String>,
    pub length: u32,
    pub slot: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfoWithGeneric {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    pub generic_signature: Option<String>,
    pub mod_bits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmClassPaths {
    pub base_dir: String,
    pub classpaths: Vec<String>,
    pub boot_classpaths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JdwpValue {
    Boolean(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Object { tag: u8, id: ObjectId },
    Void,
}

// --- JDWP event kinds -------------------------------------------------------
//
// See: https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html#JDWP_EventKind
//
// Note: only a small subset is currently implemented by nova-jdwp's wire client.

/// EventKind: SingleStep (1)
pub const EVENT_KIND_SINGLE_STEP: u8 = 1;
/// EventKind: Breakpoint (2)
pub const EVENT_KIND_BREAKPOINT: u8 = 2;
/// EventKind: Exception (4)
pub const EVENT_KIND_EXCEPTION: u8 = 4;
/// EventKind: ClassPrepare (8)
pub const EVENT_KIND_CLASS_PREPARE: u8 = 8;
/// EventKind: ClassUnload (9)
pub const EVENT_KIND_CLASS_UNLOAD: u8 = 9;
/// EventKind: FieldAccess (20)
pub const EVENT_KIND_FIELD_ACCESS: u8 = 20;
/// EventKind: FieldModification (21)
pub const EVENT_KIND_FIELD_MODIFICATION: u8 = 21;
/// EventKind: MethodExitWithReturnValue (42)
pub const EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE: u8 = 42;
/// EventKind: VMStart (90)
pub const EVENT_KIND_VM_START: u8 = 90;
/// EventKind: VMDeath (99)
pub const EVENT_KIND_VM_DEATH: u8 = 99;
/// EventKind: VMDisconnect (100)
pub const EVENT_KIND_VM_DISCONNECT: u8 = 100;

// --- JDWP EventRequest modifier kinds ---------------------------------------
//
// See: https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html#JDWP_EventRequest

/// EventRequest modifier: Count (1)
pub const EVENT_MODIFIER_KIND_COUNT: u8 = 1;
/// EventRequest modifier: ThreadOnly (3)
pub const EVENT_MODIFIER_KIND_THREAD_ONLY: u8 = 3;
/// EventRequest modifier: ClassOnly (4)
pub const EVENT_MODIFIER_KIND_CLASS_ONLY: u8 = 4;
/// EventRequest modifier: ClassMatch (5)
pub const EVENT_MODIFIER_KIND_CLASS_MATCH: u8 = 5;
/// EventRequest modifier: ClassExclude (6)
pub const EVENT_MODIFIER_KIND_CLASS_EXCLUDE: u8 = 6;
/// EventRequest modifier: LocationOnly (7)
pub const EVENT_MODIFIER_KIND_LOCATION_ONLY: u8 = 7;
/// EventRequest modifier: ExceptionOnly (8)
pub const EVENT_MODIFIER_KIND_EXCEPTION_ONLY: u8 = 8;
/// EventRequest modifier: FieldOnly (9)
pub const EVENT_MODIFIER_KIND_FIELD_ONLY: u8 = 9;
/// EventRequest modifier: Step (10)
pub const EVENT_MODIFIER_KIND_STEP: u8 = 10;
/// EventRequest modifier: InstanceOnly (11)
pub const EVENT_MODIFIER_KIND_INSTANCE_ONLY: u8 = 11;
/// EventRequest modifier: SourceNameMatch (12)
pub const EVENT_MODIFIER_KIND_SOURCE_NAME_MATCH: u8 = 12;

// --- JDWP suspend policies ---------------------------------------------------
//
// See: https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html#JDWP_SuspendPolicy

/// SuspendPolicy: NONE (0)
pub const SUSPEND_POLICY_NONE: u8 = 0;
/// SuspendPolicy: EVENT_THREAD (1)
pub const SUSPEND_POLICY_EVENT_THREAD: u8 = 1;
/// SuspendPolicy: ALL (2)
pub const SUSPEND_POLICY_ALL: u8 = 2;

// --- JDWP invocation options -------------------------------------------------
//
// See: https://docs.oracle.com/javase/8/docs/platform/jpda/jdwp/jdwp-protocol.html#JDWP_InvokeOptions

/// InvokeOptions: INVOKE_SINGLE_THREADED (0x1)
pub const INVOKE_SINGLE_THREADED: u32 = 0x1;
/// InvokeOptions: INVOKE_NONVIRTUAL (0x2)
pub const INVOKE_NONVIRTUAL: u32 = 0x2;

impl fmt::Display for JdwpValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JdwpValue::Boolean(v) => write!(f, "{v}"),
            JdwpValue::Byte(v) => write!(f, "{v}"),
            JdwpValue::Char(v) => write!(f, "{v}"),
            JdwpValue::Short(v) => write!(f, "{v}"),
            JdwpValue::Int(v) => write!(f, "{v}"),
            JdwpValue::Long(v) => write!(f, "{v}"),
            JdwpValue::Float(v) => write!(f, "{v}"),
            JdwpValue::Double(v) => write!(f, "{v}"),
            JdwpValue::Object { tag, id } => write!(f, "{:02x}:{tag}", id),
            JdwpValue::Void => write!(f, "void"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JdwpEvent {
    VmStart {
        request_id: i32,
        thread: ThreadId,
    },
    ThreadStart {
        request_id: i32,
        thread: ThreadId,
    },
    ThreadDeath {
        request_id: i32,
        thread: ThreadId,
    },
    Breakpoint {
        request_id: i32,
        thread: ThreadId,
        location: Location,
    },
    SingleStep {
        request_id: i32,
        thread: ThreadId,
        location: Location,
    },
    MethodExitWithReturnValue {
        request_id: i32,
        thread: ThreadId,
        location: Location,
        value: JdwpValue,
    },
    Exception {
        request_id: i32,
        thread: ThreadId,
        location: Location,
        exception: ObjectId,
        catch_location: Option<Location>,
    },
    ClassPrepare {
        request_id: i32,
        thread: ThreadId,
        ref_type_tag: u8,
        type_id: ReferenceTypeId,
        signature: String,
        status: u32,
    },
    ClassUnload {
        request_id: i32,
        signature: String,
    },
    FieldAccess {
        request_id: i32,
        thread: ThreadId,
        location: Location,
        ref_type_tag: u8,
        type_id: ReferenceTypeId,
        field_id: FieldId,
        object: ObjectId,
        value: JdwpValue,
    },
    FieldModification {
        request_id: i32,
        thread: ThreadId,
        location: Location,
        ref_type_tag: u8,
        type_id: ReferenceTypeId,
        field_id: FieldId,
        object: ObjectId,
        value_current: Option<JdwpValue>,
        value_to_be: JdwpValue,
    },
    VmDeath,
    VmDisconnect,
}

/// Wrapper that preserves the composite event packet's suspend policy alongside the parsed event.
#[derive(Debug, Clone, PartialEq)]
pub struct JdwpEventEnvelope {
    pub suspend_policy: u8,
    pub event: JdwpEvent,
}

#[derive(Debug, Error)]
pub enum JdwpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("VM returned error code {0}")]
    VmError(u16),

    #[error("request timed out")]
    Timeout,

    #[error("request cancelled")]
    Cancelled,

    #[error("connection closed")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, JdwpError>;

#[cfg(test)]
mod tests {
    use super::JdwpCapabilitiesNew;

    #[test]
    fn capabilities_new_from_vec_maps_known_fields() {
        let mut caps = vec![false; 32];
        caps[0] = true; // canWatchFieldModification
        caps[7] = true; // canRedefineClasses
        caps[22] = true; // canGetMethodReturnValues
        caps[31] = true; // reserved

        let typed = JdwpCapabilitiesNew::from_vec(caps);

        assert!(typed.can_watch_field_modification);
        assert!(typed.supports_watchpoints());

        assert!(typed.can_redefine_classes);
        assert!(typed.supports_redefine_classes());

        assert!(typed.can_get_method_return_values);
        assert!(typed.supports_method_return_values());

        assert!(typed.reserved_31);
    }
}
