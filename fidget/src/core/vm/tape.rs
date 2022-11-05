use crate::vm::Op;

#[derive(Clone, Default)]
pub struct Tape {
    tape: Vec<Op>,

    /// Total allocated slots
    pub(super) slot_count: u32,

    /// Number of registers, before we fall back to Load/Store operations
    pub(super) reg_limit: u8,
}

impl Tape {
    pub fn new(reg_limit: u8) -> Self {
        Self {
            tape: Vec::with_capacity(512),
            slot_count: 1,
            reg_limit,
        }
    }
    pub fn reset(&mut self, reg_limit: u8) {
        self.tape.clear();
        self.slot_count = 1;
        self.reg_limit = reg_limit;
    }
    pub fn reg_limit(&self) -> u8 {
        self.reg_limit
    }
    #[inline]
    pub fn slot_count(&self) -> usize {
        self.slot_count as usize
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.tape.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tape.is_empty()
    }
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Op> {
        self.tape.iter()
    }
    #[inline]
    pub fn push(&mut self, op: Op) {
        self.tape.push(op)
    }
}
