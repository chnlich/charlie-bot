//! The prefix the file server answers under: the public constant that the routes and their matcher both read.

/// `file_urls.FILE_SERVER_MOUNTS[0]`: the prefix the route is mounted under, without its trailing slash.
pub const MOUNT: &str = charliebot_compat::file_urls::FILE_SERVER_MOUNT;
