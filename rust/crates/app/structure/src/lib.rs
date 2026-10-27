//! The structure test's crate. The checks live in `tests/structure.rs`: they read the workspace and fail when a crate
//! sits outside its group, a dependency crosses a group direction or a runtime layer, a `#[path]` reaches out of its
//! crate, or a crate's module graph carries a cycle that `exceptions.txt` does not record.
