use super::census;
use super::types::*;

impl Module {
    pub(super) fn ty(&mut self, def: TypeDef) -> Ty {
        if let Some(&t) = self.type_ids.get(&def) {
            return Ty(t);
        }
        let t = self.types.len() as u32;
        self.types.push(def.clone());
        self.type_ids.insert(def, t);
        Ty(t)
    }

    pub fn void(&mut self) -> Ty {
        self.ty(TypeDef::Void)
    }
    pub fn int(&mut self, bits: u32) -> Ty {
        self.ty(TypeDef::Int(bits))
    }
    pub fn float(&mut self) -> Ty {
        self.ty(TypeDef::Float)
    }
    pub fn double(&mut self) -> Ty {
        self.ty(TypeDef::Double)
    }
    pub fn ptr(&mut self, to: Ty) -> Ty {
        self.ty(TypeDef::Ptr(to, 0))
    }
    pub fn ptr_in(&mut self, to: Ty, space: u32) -> Ty {
        self.ty(TypeDef::Ptr(to, space))
    }
    pub fn array(&mut self, n: u32, of: Ty) -> Ty {
        self.ty(TypeDef::Array(n, of))
    }
    pub fn named_struct(&mut self, name: &str, fields: Vec<Ty>) -> Ty {
        self.ty(TypeDef::Struct(Some(name.into()), fields))
    }
    pub fn func_ty(&mut self, ret: Ty, params: Vec<Ty>) -> Ty {
        self.ty(TypeDef::Func(ret, params))
    }

    pub(super) fn is_void(&self, t: Ty) -> bool {
        self.types[t.0 as usize] == TypeDef::Void
    }

    pub(super) fn constant(&mut self, ty: Ty, c: Const) -> V {
        if let Some(&i) = self.const_ids.get(&(ty, c.clone())) {
            return V::Const(i);
        }
        let i = self.consts.len() as u32;
        self.consts.push((ty, c.clone()));
        self.const_ids.insert((ty, c), i);
        V::Const(i)
    }

    pub fn int_const(&mut self, ty: Ty, v: i64) -> V {
        self.constant(ty, Const::Int(v))
    }
    pub fn i32(&mut self, v: u32) -> V {
        let t = self.int(32);
        self.int_const(t, i64::from(v as i32))
    }
    pub fn i8(&mut self, v: u8) -> V {
        let t = self.int(8);
        self.int_const(t, i64::from(v as i8))
    }
    pub fn bool(&mut self, v: bool) -> V {
        let t = self.int(1);
        self.int_const(t, -i64::from(v))
    }
    pub fn f32(&mut self, bits: u32) -> V {
        let t = self.float();
        self.constant(t, Const::Float(bits))
    }
    pub fn undef(&mut self, ty: Ty) -> V {
        self.constant(ty, Const::Undef)
    }
    /// A constant struct or array of constants.
    pub fn aggregate(&mut self, ty: Ty, fields: &[V]) -> V {
        let ids = fields
            .iter()
            .map(|f| match f {
                V::Const(i) => *i,
                _ => panic!("aggregate of a non-constant"),
            })
            .collect();
        self.constant(ty, Const::Aggregate(ids))
    }

    /// The value of an integer constant; `None` for anything else.
    pub fn int_value(&self, v: V) -> Option<i64> {
        match v {
            V::Const(i) => match self.consts[i as usize].1 {
                Const::Int(x) => Some(x),
                _ => None,
            },
            _ => None,
        }
    }

    /// Gives constant `old` the content of `new` (both stay valid).
    pub fn replace_const(&mut self, old: V, new: V) {
        if let (V::Const(o), V::Const(n)) = (old, new) {
            self.consts[o as usize] = self.consts[n as usize].clone();
        }
    }

    /// A function declaration (or `main` with `body`), found by name.
    pub fn function(&mut self, name: &str, ty: Ty, attr: Option<Attr>, body: bool) -> V {
        if let Some(&i) = self.func_ids.get(name) {
            return V::Func(i);
        }
        let i = self.funcs.len() as u32;
        self.funcs.push(Func {
            name: name.into(),
            ty,
            attr,
            body,
        });
        self.func_ids.insert(name.into(), i);
        V::Func(i)
    }

    /// An external global of type `ty` in address space `space` (DXIL
    /// declares thread-group shared memory this way).
    pub fn global(&mut self, name: &str, ty: Ty, space: u32) -> V {
        let ptr = self.ptr_in(ty, space);
        self.globals.push(Global {
            name: name.into(),
            ty,
            ptr,
            space,
            init: None,
        });
        V::Global(self.globals.len() as u32 - 1)
    }

    /// An internal constant global initialised with the constant `init`.
    pub fn constant_global(&mut self, name: &str, init: V) -> V {
        let V::Const(i) = init else {
            panic!("a global initialised with a non-constant")
        };
        let ty = self.consts[i as usize].0;
        let g = self.global(name, ty, 0);
        if let V::Global(n) = g {
            self.globals[n as usize].init = Some(i);
        }
        g
    }

    /// Notes what instruction `v` is for (see [`Tag`]); only when modules
    /// are counted.
    pub fn tag(&mut self, v: V, tag: Tag) {
        if let V::Inst(i) = v
            && census::enabled()
        {
            self.tags.insert(i, tag);
        }
    }

    pub fn new_block(&mut self) -> usize {
        self.blocks.push(Block::default());
        self.blocks.len() - 1
    }

    pub fn type_of(&self, v: V) -> Ty {
        match v {
            V::Global(i) => self.globals[i as usize].ptr,
            V::Const(i) => self.consts[i as usize].0,
            V::Inst(i) => self.insts[i as usize].ty,
            V::Func(i) => {
                let t = self.funcs[i as usize].ty;
                *self
                    .type_ids
                    .get(&TypeDef::Ptr(t, 0))
                    .map(|t| Ty(*t))
                    .as_ref()
                    .expect("function pointer type")
            }
        }
    }

    pub(super) fn push(&mut self, block: usize, ty: Ty, op: Op) -> V {
        let i = self.insts.len() as u32;
        self.insts.push(Inst { ty, op });
        // Terminators are kept apart, so a block can still grow at its end.
        self.blocks[block].body.push(i);
        V::Inst(i)
    }

    pub fn bin(&mut self, block: usize, op: Bin, a: V, b: V) -> V {
        let ty = self.type_of(a);
        self.push(block, ty, Op::Bin(op, a, b))
    }
    pub fn cast(&mut self, block: usize, op: Cast, v: V, to: Ty) -> V {
        self.push(block, to, Op::Cast(op, v))
    }
    pub fn cmp(&mut self, block: usize, pred: u8, a: V, b: V) -> V {
        let t = self.int(1);
        self.push(block, t, Op::Cmp(pred, a, b))
    }
    pub fn select(&mut self, block: usize, c: V, t: V, f: V) -> V {
        let ty = self.type_of(t);
        self.push(block, ty, Op::Select(c, t, f))
    }
    pub fn extract(&mut self, block: usize, agg: V, index: u32) -> V {
        let ty = match &self.types[self.type_of(agg).0 as usize] {
            TypeDef::Struct(_, f) => f[index as usize],
            TypeDef::Array(_, t) => *t,
            t => panic!("extractvalue from {t:?}"),
        };
        self.push(block, ty, Op::Extract(agg, index))
    }
    pub fn call(&mut self, block: usize, f: V, args: Vec<V>) -> V {
        let V::Func(i) = f else {
            panic!("call of a non-function")
        };
        let TypeDef::Func(ret, _) = self.types[self.funcs[i as usize].ty.0 as usize] else {
            unreachable!()
        };
        self.push(block, ret, Op::Call(i, args))
    }
    /// A phi at the top of `block`; incoming values come with [`Module::set_phi`].
    pub fn phi(&mut self, block: usize, ty: Ty) -> V {
        let i = self.insts.len() as u32;
        self.insts.push(Inst {
            ty,
            op: Op::Phi(Vec::new()),
        });
        self.blocks[block].phis.push(i);
        V::Inst(i)
    }
    /// The address of element `index` of the array `ptr` points to.
    pub fn element(&mut self, block: usize, ptr: V, index: V) -> V {
        let TypeDef::Ptr(arr, space) = self.types[self.type_of(ptr).0 as usize] else {
            panic!("element of a non-pointer")
        };
        let TypeDef::Array(_, elem) = self.types[arr.0 as usize] else {
            panic!("element of a non-array")
        };
        let ty = self.ptr_in(elem, space);
        let zero = self.i32(0);
        self.push(block, ty, Op::Gep(arr, ptr, vec![zero, index]))
    }
    pub fn load(&mut self, block: usize, ptr: V) -> V {
        let TypeDef::Ptr(ty, _) = self.types[self.type_of(ptr).0 as usize] else {
            panic!("load from a non-pointer")
        };
        self.push(block, ty, Op::Load(ptr))
    }
    pub fn store(&mut self, block: usize, ptr: V, value: V) {
        let void = self.void();
        self.push(block, void, Op::Store(ptr, value));
    }
    pub fn atomic(&mut self, block: usize, op: Rmw, ptr: V, value: V) -> V {
        let ty = self.type_of(value);
        self.push(block, ty, Op::AtomicRmw(op, ptr, value))
    }
    /// The old value of a compare-exchange.
    pub fn cmpxchg(&mut self, block: usize, ptr: V, expected: V, new: V) -> V {
        let (i32t, i1) = (self.type_of(new), self.int(1));
        let pair = self.ty(TypeDef::Struct(None, vec![i32t, i1]));
        let r = self.push(block, pair, Op::CmpXchg(ptr, expected, new));
        self.extract(block, r, 0)
    }

    pub fn set_phi(&mut self, phi: V, incoming: Vec<(V, usize)>) {
        let V::Inst(i) = phi else { panic!() };
        self.insts[i as usize].op = Op::Phi(incoming);
    }
    pub fn terminate(&mut self, block: usize, t: Term) {
        self.blocks[block].term = Some(t);
    }

    pub fn md_str(&mut self, s: &str) -> Md {
        self.mds.push(MdDef::Str(s.into()));
        Md(self.mds.len() as u32 - 1)
    }
    pub fn md_value(&mut self, v: V) -> Md {
        let ty = self.type_of(v);
        self.mds.push(MdDef::Value(ty, v));
        Md(self.mds.len() as u32 - 1)
    }
    pub fn md_node(&mut self, ops: Vec<Option<Md>>) -> Md {
        self.mds.push(MdDef::Node(ops));
        Md(self.mds.len() as u32 - 1)
    }
    pub fn named_md(&mut self, name: &str, nodes: Vec<Md>) {
        self.named.push((name.into(), nodes));
    }
}
