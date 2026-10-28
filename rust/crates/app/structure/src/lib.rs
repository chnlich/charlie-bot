//! The structure test's crate. The checks live in `tests/structure.rs`: they read the workspace and fail when a crate
//! sits outside its group, a dependency crosses a group direction or a runtime layer, a `#[path]` reaches out of its
//! crate, a crate's module graph carries a module cycle, or a name that has an owner is spelled outside it.
