//! The prefix the file server answers under. The request matcher and the directory listing's links read it.

/// The prefix the route is mounted under, without its trailing slash. It is the compat crate's
/// `file_urls::FILE_SERVER_MOUNT`, which the other crates that build file links also read.
pub const MOUNT: &str = charliebot_compat::file_urls::FILE_SERVER_MOUNT;
