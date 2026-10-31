//! The structure test's crate. The checks live in `tests/structure.rs` and read the workspace. They fail when a crate
//! sits outside its group, a dependency crosses a group direction or a runtime layer, or a `#[path]` reaches out of
//! its crate. They also fail on a crate's module cycle, or on a name that has an owner spelled outside it.
