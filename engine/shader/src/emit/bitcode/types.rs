use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Ty(pub(in crate::emit::bitcode) u32);

/// What the lowering made an instruction for, where it says so (counted by
/// [`census`], nothing else reads it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    /// The select that keeps a register's old value in lanes a guard masks.
    Guard,
    /// An index clamp the translation adds (the console has none).
    Clamp,
    /// The iteration counter and limit test the translation adds to a loop.
    Counter,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::emit::bitcode) enum TypeDef {
    Void,
    Int(u32),
    Float,
    Double,
    /// Pointee and address space.
    Ptr(Ty, u32),
    Struct(Option<String>, Vec<Ty>),
    Array(u32, Ty),
    Func(Ty, Vec<Ty>),
}

/// A value: a global variable, a function, a module constant, or an
/// instruction result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum V {
    Global(u32),
    Func(u32),
    Const(u32),
    Inst(u32),
}

/// A global variable declaration (thread-group shared memory).
pub(in crate::emit::bitcode) struct Global {
    pub(in crate::emit::bitcode) name: String,
    pub(in crate::emit::bitcode) ty: Ty,
    /// Its pointer type.
    pub(in crate::emit::bitcode) ptr: Ty,
    pub(in crate::emit::bitcode) space: u32,
    /// An internal constant with this initialiser (a constant table).
    pub(in crate::emit::bitcode) init: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::emit::bitcode) enum Const {
    Int(i64),
    Float(u32),
    Undef,
    Aggregate(Vec<u32>),
}

/// LLVM binary opcodes (the same codes serve the float forms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bin {
    Add = 0,
    Sub = 1,
    Mul = 2,
    UDiv = 3,
    SDiv = 4,
    URem = 5,
    Shl = 7,
    LShr = 8,
    AShr = 9,
    And = 10,
    Or = 11,
    Xor = 12,
}

/// `atomicrmw` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rmw {
    Xchg = 0,
    Add = 1,
    Sub = 2,
    And = 3,
    Or = 5,
    Xor = 6,
    Max = 7,
    Min = 8,
    UMax = 9,
    UMin = 10,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cast {
    Trunc = 0,
    ZExt = 1,
    SExt = 2,
    FpToUi = 3,
    FpToSi = 4,
    UiToFp = 5,
    SiToFp = 6,
    Bitcast = 11,
}

/// Compare predicates: floats 0-15, integers 32-41.
pub(crate) mod pred {
    pub const FALSE: u8 = 0;
    pub const OEQ: u8 = 1;
    pub const OGT: u8 = 2;
    pub const OGE: u8 = 3;
    pub const OLT: u8 = 4;
    pub const OLE: u8 = 5;
    // Not emitted: the AMD D3D12 driver treats it as UNE, so `v_cmp_lg`
    // lowers as OLT | OGT instead.
    #[allow(dead_code)]
    pub const ONE: u8 = 6;
    pub const ORD: u8 = 7;
    pub const UNO: u8 = 8;
    pub const UEQ: u8 = 9;
    pub const UGT: u8 = 10;
    pub const UGE: u8 = 11;
    pub const ULT: u8 = 12;
    pub const ULE: u8 = 13;
    pub const UNE: u8 = 14;
    pub const TRUE: u8 = 15;
    pub const EQ: u8 = 32;
    pub const NE: u8 = 33;
    pub const IUGT: u8 = 34;
    pub const IUGE: u8 = 35;
    pub const IULT: u8 = 36;
    pub const IULE: u8 = 37;
    pub const SGT: u8 = 38;
    pub const SGE: u8 = 39;
    pub const SLT: u8 = 40;
    pub const SLE: u8 = 41;
}

#[derive(Debug, Clone)]
pub(in crate::emit::bitcode) enum Op {
    Bin(Bin, V, V),
    Cast(Cast, V),
    Cmp(u8, V, V),
    Select(V, V, V),
    Extract(V, u32),
    Call(u32, Vec<V>),
    /// Filled in later: (value, block).
    Phi(Vec<(V, usize)>),
    /// `getelementptr` over the pointee type, from a pointer.
    Gep(Ty, V, Vec<V>),
    Load(V),
    /// Pointer, value.
    Store(V, V),
    AtomicRmw(Rmw, V, V),
    /// Pointer, expected, new; returns `{i32, i1}`.
    CmpXchg(V, V, V),
}

#[derive(Debug, Clone)]
pub(in crate::emit::bitcode) struct Inst {
    pub(in crate::emit::bitcode) ty: Ty,
    pub(in crate::emit::bitcode) op: Op,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Term {
    Br(usize),
    CondBr(V, usize, usize),
    Ret,
}

#[derive(Debug, Clone, Default)]
pub(in crate::emit::bitcode) struct Block {
    pub(in crate::emit::bitcode) phis: Vec<u32>,
    pub(in crate::emit::bitcode) body: Vec<u32>,
    pub(in crate::emit::bitcode) term: Option<Term>,
}

/// Function attributes of a `dx.op` declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attr {
    ReadNone = 1,
    NoUnwind = 2,
    ReadOnly = 3,
    /// Barriers.
    NoDuplicate = 4,
}

pub(in crate::emit::bitcode) struct Func {
    pub(in crate::emit::bitcode) name: String,
    pub(in crate::emit::bitcode) ty: Ty,
    pub(in crate::emit::bitcode) attr: Option<Attr>,
    pub(in crate::emit::bitcode) body: bool,
}

/// A metadata node operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Md(pub(in crate::emit::bitcode) u32);

pub(in crate::emit::bitcode) enum MdDef {
    Str(String),
    Value(Ty, V),
    Node(Vec<Option<Md>>),
}

#[derive(Default)]
pub(crate) struct Module {
    pub(in crate::emit::bitcode) types: Vec<TypeDef>,
    pub(in crate::emit::bitcode) type_ids: HashMap<TypeDef, u32>,
    pub(in crate::emit::bitcode) consts: Vec<(Ty, Const)>,
    pub(in crate::emit::bitcode) const_ids: HashMap<(Ty, Const), u32>,
    pub(in crate::emit::bitcode) globals: Vec<Global>,
    pub(in crate::emit::bitcode) funcs: Vec<Func>,
    pub(in crate::emit::bitcode) func_ids: HashMap<String, u32>,
    pub(in crate::emit::bitcode) insts: Vec<Inst>,
    pub(in crate::emit::bitcode) blocks: Vec<Block>,
    pub(in crate::emit::bitcode) mds: Vec<MdDef>,
    pub(in crate::emit::bitcode) named: Vec<(String, Vec<Md>)>,
    /// See [`Tag`]: by instruction.
    pub(in crate::emit::bitcode) tags: HashMap<u32, Tag>,
}

