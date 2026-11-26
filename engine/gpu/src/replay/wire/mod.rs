//! What a capture holds and how it is written: plain little-endian bytes,
//! fields in order. Read on the machine that wrote it; nothing here is kept
//! compatible between versions beyond the number in [`VERSION`].

/// Changes with every change of the layout.
pub const VERSION: u32 = 1;
/// The first bytes of `frame.bin`.
pub const MAGIC: [u8; 8] = *b"VICFRAME";

/// Why bytes could not be read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bad(pub &'static str);

impl std::fmt::Display for Bad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the capture cannot be read: {}", self.0)
    }
}

impl std::error::Error for Bad {}

/// Bytes being read.
pub struct Dec<'a> {
    pub bytes: &'a [u8],
}

impl<'a> Dec<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Dec { bytes }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        if self.bytes.len() < n {
            return Err(Bad("it ends too early"));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
}

/// A value that can be written and read back.
pub trait Wire: Sized {
    fn put(&self, out: &mut Vec<u8>);
    fn get(from: &mut Dec) -> Result<Self, Bad>;
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl Wire for $t {
            fn put(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn get(from: &mut Dec) -> Result<Self, Bad> {
                let bytes = from.take(size_of::<$t>())?;
                Ok(<$t>::from_le_bytes(bytes.try_into().map_err(|_| Bad("a number"))?))
            }
        }
    )*};
}
numbers!(u8, u16, u32, i32, u64, f32);

impl Wire for bool {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        Ok(u8::get(from)? != 0)
    }
}

impl<T: Wire> Wire for Vec<T> {
    fn put(&self, out: &mut Vec<u8>) {
        (self.len() as u32).put(out);
        for x in self {
            x.put(out);
        }
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        let n = u32::get(from)? as usize;
        // At least a byte each: a wild count is refused, not allocated.
        if n > from.bytes.len() {
            return Err(Bad("a list longer than the file"));
        }
        (0..n).map(|_| T::get(from)).collect()
    }
}

impl<T: Wire> Wire for Option<T> {
    fn put(&self, out: &mut Vec<u8>) {
        self.is_some().put(out);
        if let Some(x) = self {
            x.put(out);
        }
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        Ok(if bool::get(from)? {
            Some(T::get(from)?)
        } else {
            None
        })
    }
}

impl<T: Wire + Copy + Default, const N: usize> Wire for [T; N] {
    fn put(&self, out: &mut Vec<u8>) {
        for x in self {
            x.put(out);
        }
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        let mut all = [T::default(); N];
        for x in &mut all {
            *x = T::get(from)?;
        }
        Ok(all)
    }
}

impl Wire for String {
    fn put(&self, out: &mut Vec<u8>) {
        (self.len() as u32).put(out);
        out.extend_from_slice(self.as_bytes());
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        let n = u32::get(from)? as usize;
        String::from_utf8(from.take(n)?.to_vec()).map_err(|_| Bad("a name that is not text"))
    }
}

/// Bytes copied whole.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Blob(pub Vec<u8>);

impl Wire for Blob {
    fn put(&self, out: &mut Vec<u8>) {
        (self.0.len() as u64).put(out);
        out.extend_from_slice(&self.0);
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        let n = u64::get(from)? as usize;
        Ok(Blob(from.take(n)?.to_vec()))
    }
}

macro_rules! tuples {
    ($($t:ident $i:tt),*) => {
        impl<$($t: Wire),*> Wire for ($($t,)*) {
            fn put(&self, out: &mut Vec<u8>) {
                $(self.$i.put(out);)*
            }
            fn get(from: &mut Dec) -> Result<Self, Bad> {
                Ok(($($t::get(from)?,)*))
            }
        }
    };
}
tuples!(A 0, B 1);
tuples!(A 0, B 1, C 2);

macro_rules! wire_struct {
    ($(#[$meta:meta])* $name:ident { $($(#[$fmeta:meta])* $f:ident : $t:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name { $($(#[$fmeta])* pub $f: $t),* }
        impl Wire for $name {
            fn put(&self, out: &mut Vec<u8>) {
                $(self.$f.put(out);)*
            }
            fn get(from: &mut Dec) -> Result<Self, Bad> {
                Ok($name { $($f: <$t>::get(from)?),* })
            }
        }
    };
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $tag:literal $var:ident { $($f:ident : $t:ty),* $(,)? }),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq)]
        pub enum $name { $($(#[$vmeta])* $var { $($f: $t),* }),* }
        impl Wire for $name {
            fn put(&self, out: &mut Vec<u8>) {
                match self {
                    $($name::$var { $($f),* } => {
                        out.push($tag);
                        $($f.put(out);)*
                    })*
                }
            }
            fn get(from: &mut Dec) -> Result<Self, Bad> {
                Ok(match u8::get(from)? {
                    $($tag => $name::$var { $($f: <$t>::get(from)?),* },)*
                    _ => return Err(Bad("a kind that does not exist")),
                })
            }
        }
    };
}


mod cmd;
mod frame;
mod types;

pub use cmd::*;
pub use frame::*;
pub use types::*;

#[cfg(test)]
pub(crate) mod tests;
