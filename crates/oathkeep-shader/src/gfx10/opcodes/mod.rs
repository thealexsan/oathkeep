// GFX10 opcode names, generated from Mesa's src/amd/compiler/aco_opcodes.py
// (tag mesa-22.3.0, GFX10 column), which carries this notice:
//
// Copyright (c) 2018 Valve Corporation
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice (including the next
// paragraph) shall be included in all copies or substantial portions of the
// Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
// IN THE SOFTWARE.
//
// Each table is sorted by opcode. VOP3 holds only VOP3-only opcodes; VOP3
// encodings of VOPC/VOP2/VOP1 ops (0x000-0x13f, 0x180-0x1ff) use those tables.


include!("scalar.rs");
include!("valu_basic.rs");
include!("valu_ext.rs");
include!("ds.rs");
include!("buf_img.rs");
include!("flat.rs");

#[cfg(test)]
mod tests;
