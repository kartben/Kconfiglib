//! String interning.
//!
//! Kconfig trees repeat the same identifiers thousands of times: every
//! `depends on` names symbols that already exist, and the same file paths
//! recur throughout the include stack. Interning turns those into `u32`
//! handles, so the rest of the loader compares and copies integers instead of
//! strings.

use rustc_hash::FxHashMap;
use std::ops::Index;

/// A handle to an interned string. Cheap to copy, compare, and hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StrId(pub u32);

#[derive(Default)]
pub struct Interner {
    ids: FxHashMap<Box<str>, StrId>,
    strings: Vec<Box<str>>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the handle for `s`, allocating one if this is the first time we
    /// have seen it.
    pub fn intern(&mut self, s: &str) -> StrId {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = StrId(self.strings.len() as u32);
        let boxed: Box<str> = s.into();
        self.strings.push(boxed.clone());
        self.ids.insert(boxed, id);
        id
    }

    /// Looks `s` up without interning it.
    pub fn get(&self, s: &str) -> Option<StrId> {
        self.ids.get(s).copied()
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl Index<StrId> for Interner {
    type Output = str;

    fn index(&self, id: StrId) -> &str {
        &self.strings[id.0 as usize]
    }
}
