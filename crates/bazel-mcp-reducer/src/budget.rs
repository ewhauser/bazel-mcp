#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    pub max_bytes: usize,
    pub max_items: usize,
}

impl Budget {
    #[must_use]
    pub const fn result_default() -> Self {
        Self {
            max_bytes: 4 * 1024,
            max_items: 20,
        }
    }

    #[must_use]
    pub const fn inspect_default() -> Self {
        Self {
            max_bytes: 8 * 1024,
            max_items: 50,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Budgeted<T> {
    pub value: T,
    pub truncated: bool,
}
