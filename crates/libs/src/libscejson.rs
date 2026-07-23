//! `sce::Json` HLE — the C++ JSON library Sony ships, answered as a world that parses
//! nothing and contains exactly one value: **null**.
//!
//! # Why a C++ library can be HLE'd at all
//!
//! Every entry point here is an *import*: `Value::operator[]`, `String::c_str()`, the
//! constructors, all of it. The guest never reaches inside these objects itself — it
//! allocates storage for them (on its stack, sized by `sizeof` baked in at its compile
//! time) and then hands us pointers. That makes the layout ours to define, as long as we
//! never write more than the real type's size.
//!
//! We currently exploit that only to the extent of writing **nothing at all**. Every
//! constructor is a no-op, so a guest `Value` holds whatever was on its stack; nothing
//! reads it but us, and we do not look. The destructors are no-ops too, which is what makes
//! that safe: there is no pointer for a destructor to free.
//!
//! # The one invariant
//!
//! `Parser::parse` always fails, and *because* it always fails no title should ever reach a
//! populated value. The accessors are still implemented rather than left to crash, because
//! "should" is not "will": a title that ignores the parse result gets a coherent empty
//! world — an empty string, a null value, a count of zero — instead of a jump through a
//! garbage pointer. Both singletons live in the HLE arena and are handed out repeatedly;
//! they are read-only by construction, since nothing here ever mutates a value.
//!
//! # When this stops being enough
//!
//! The moment a title parses JSON it actually needs — a local manifest, a cached response —
//! this file must grow a real implementation: a host-side value tree keyed by a handle
//! stored in the first eight bytes of the guest object. That is a bigger piece of work and
//! wants its own task. The tell that it is needed: a title that behaves as if a config it
//! read were empty. Until then, everything reaching this library comes from the network,
//! and the network already refused (see [`crate::libscenp`]).

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::sync::OnceLock;

/// A parse that did not happen. `sce::Json` returns negative SCE error codes; we do not
/// have the documented values and will not invent one that looks verified. Any negative
/// value takes the caller down its failure path.
const JSON_PARSE_FAILED: i32 = -1;

/// Guest address of the canonical **empty `String`** every accessor hands back, and of the
/// canonical **null `Value`**. Both are allocated once, on first use, in the HLE arena.
///
/// One shared instance for all callers is safe precisely because nothing in this library
/// mutates: a title cannot make one empty string differ from another through the API we
/// expose. The third singleton is a lone NUL byte — what `c_str()` points at, so a guest
/// that does `strlen`/`printf` on the result reads an empty C string rather than faulting.
static EMPTY_STRING: OnceLock<u64> = OnceLock::new();
static NULL_VALUE: OnceLock<u64> = OnceLock::new();
static EMPTY_CSTR: OnceLock<u64> = OnceLock::new();

/// Allocate one 16-byte guest-resident singleton and zero it. Returns 0 if the arena is
/// unset or exhausted; callers hand that 0 straight to the guest, which is a null pointer
/// it will fault on visibly rather than a wild address it would corrupt memory through.
fn singleton() -> u64 {
    let addr = ps4_core::kernel::hle_alloc(16);
    if addr != 0
        && let Some(gp) = GuestPtr::<u64>::new(addr)
    {
        let _ = gp.write(0);
    }
    addr
}

fn empty_string() -> u64 {
    *EMPTY_STRING.get_or_init(singleton)
}

fn null_value() -> u64 {
    *NULL_VALUE.get_or_init(singleton)
}

fn empty_cstr() -> u64 {
    *EMPTY_CSTR.get_or_init(singleton)
}

// ---------------------------------------------------------------------------
// MemAllocator — the abstract allocator a title subclasses to feed Json its memory.
// ---------------------------------------------------------------------------

/// `sce::Json::MemAllocator::MemAllocator()` — the base constructor, called by the title's
/// own derived constructor. A no-op: on hardware this would set the base vtable pointer,
/// and the derived constructor immediately overwrites it with its own. Writing nothing
/// leaves the derived class's work untouched, which is the only part that matters — we
/// never call back into this allocator, because nothing here allocates guest memory.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON12_MEM_ALLOCATOR_C2_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json12MemAllocatorC2Ev"
)]
pub fn json_mem_allocator_ctor(this: u64) -> u64 {
    // The Itanium ABI lets a constructor return `this` in RAX; callers ignore it.
    this
}

/// `sce::Json::MemAllocator::~MemAllocator()` — nothing was acquired.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON12_MEM_ALLOCATOR_D2_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json12MemAllocatorD2Ev"
)]
pub fn json_mem_allocator_dtor(this: u64) -> u64 {
    this
}

/// `sce::Json::MemAllocator::notifyError(int, size_t, void*)` — the base implementation of
/// the hook Json calls when an allocation fails. Nothing here allocates, so nothing fails.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON12_MEM_ALLOCATOR11NOTIFY_ERROR_EIM_PV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json12MemAllocator11notifyErrorEimPv"
)]
pub fn json_mem_allocator_notify_error(_this: u64, _error: i32, _size: u64, _user: u64) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// Initializer — library setup. Succeeds: the library is present, it just parses nothing.
// ---------------------------------------------------------------------------

/// `sce::Json::Initializer::Initializer()`.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON11_INITIALIZER_C1_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json11InitializerC1Ev"
)]
pub fn json_initializer_ctor(this: u64) -> u64 {
    this
}

/// `sce::Json::Initializer::~Initializer()`.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON11_INITIALIZER_D1_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json11InitializerD1Ev"
)]
pub fn json_initializer_dtor(this: u64) -> u64 {
    this
}

/// `sce::Json::Initializer::initialize(const InitParameter*)` — hand the library its
/// allocator and buffer sizes. Succeeds without recording any of it: the allocator would
/// only ever be called from a parse, and no parse ever runs.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON11_INITIALIZER10INITIALIZE_EPKNS0_13_INIT_PARAMETER_E,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json11Initializer10initializeEPKNS0_13InitParameterE"
)]
pub fn json_initializer_initialize(_this: u64, _init_param: u64) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// Value — a JSON node. Ours is always null.
// ---------------------------------------------------------------------------

/// `sce::Json::Value::Value()` — default construction. A no-op; see the module header for
/// why leaving the guest's storage untouched is safe when the destructor is also a no-op.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON5_VALUE_C1_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json5ValueC1Ev"
)]
pub fn json_value_ctor(this: u64) -> u64 {
    this
}

/// `sce::Json::Value::~Value()` — no allocation was made, so there is nothing to free.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON5_VALUE_D1_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json5ValueD1Ev"
)]
pub fn json_value_dtor(this: u64) -> u64 {
    this
}

/// `sce::Json::Value::count() const` — how many children. A null value has none.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON5_VALUE5COUNT_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json5Value5countEv"
)]
pub fn json_value_count(_this: u64) -> u64 {
    0
}

/// `sce::Json::Value::getString() const` — returns `const String&`, so a *reference* that
/// must stay valid after we return. The shared empty-string singleton is exactly that: it
/// outlives every caller and nothing can mutate it.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON5_VALUE9GET_STRING_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json5Value9getStringEv"
)]
pub fn json_value_get_string(_this: u64) -> u64 {
    empty_string()
}

/// `sce::Json::Value::operator[](const char*) const` — look up a member by name. Every
/// lookup lands on the same null value, which is what a real `sce::Json` returns for a
/// missing key: a null node, not an error.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON5_VALUEIX_EPKC,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json5ValueixEPKc"
)]
pub fn json_value_index_by_name(_this: u64, _key: u64) -> u64 {
    null_value()
}

/// `sce::Json::Value::operator[](size_t) const` — index into an array. Ours is empty, so
/// every index is out of range and yields the same null node.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON5_VALUEIX_EM,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json5ValueixEm"
)]
pub fn json_value_index_by_position(_this: u64, _index: u64) -> u64 {
    null_value()
}

// ---------------------------------------------------------------------------
// String — Json's own string type.
// ---------------------------------------------------------------------------

/// `sce::Json::String::String(const String&)` — copy construction. A no-op: every string
/// this library produces is the same empty one, so a copy of it is empty too, and the
/// destructor below frees nothing either way.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON6_STRING_C1_ERKS1,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json6StringC1ERKS1_"
)]
pub fn json_string_copy_ctor(this: u64, _other: u64) -> u64 {
    this
}

/// `sce::Json::String::~String()`.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON6_STRING_D1_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json6StringD1Ev"
)]
pub fn json_string_dtor(this: u64) -> u64 {
    this
}

/// `sce::Json::String::size() const`.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON6_STRING4SIZE_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json6String4sizeEv"
)]
pub fn json_string_size(_this: u64) -> u64 {
    0
}

/// `sce::Json::String::empty() const`.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON6_STRING5EMPTY_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json6String5emptyEv"
)]
pub fn json_string_empty(_this: u64) -> i32 {
    1
}

/// `sce::Json::String::c_str() const` — a borrowed C string that must stay valid. Points at
/// a guest-resident NUL byte, so `strlen` reads 0 and `printf("%s")` prints nothing.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON6_STRING5C_STR_EV,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json6String5c_strEv"
)]
pub fn json_string_c_str(_this: u64) -> u64 {
    empty_cstr()
}

/// `sce::Json::String::operator==(const char*) const` — compares equal only to the empty
/// string, because that is the only string this library ever holds. Answering a flat
/// "false" would be a lie a title could catch by comparing against `""`.
#[ps4_syscall(
    id = SyscallId::SYS_ZNK3SCE4_JSON6_STRINGEQ_EPKC,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZNK3sce4Json6StringeqEPKc"
)]
pub fn json_string_eq_cstr(_this: u64, other: u64) -> i32 {
    match ps4_core::guest_ptr::read_cstr(other, 4096) {
        Some(s) => i32::from(s.is_empty()),
        // An unreadable pointer is not equal to anything; it is also not a crash.
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// Parser — the entry point that decides everything above stays unreachable.
// ---------------------------------------------------------------------------

/// `sce::Json::Parser::parse(Value&, const char*, size_t)` — always fails, and *this* is
/// the load-bearing decision in the file. Succeeding would leave the caller holding a value
/// we cannot populate, and every accessor above would then quietly answer "empty" about a
/// document that had real content. Failing keeps the lie confined to one call the title
/// already has a branch for.
#[ps4_syscall(
    id = SyscallId::SYS_ZN3SCE4_JSON6_PARSER5PARSE_ERNS0_5_VALUE_EPKCM,
    lib = crate::libs::LIB_SCE_JSON,
    name = "_ZN3sce4Json6Parser5parseERNS0_5ValueEPKcm"
)]
pub fn json_parser_parse(_value: u64, _text: u64, _len: u64) -> i32 {
    JSON_PARSE_FAILED
}
