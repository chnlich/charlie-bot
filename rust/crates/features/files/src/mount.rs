//! The prefix the file server answers under. The request matcher and the directory listing's links read it.

/// `file_urls.FILE_SERVER_MOUNTS[0]`: the prefix the route is mounted under, without its trailing slash.
pub const MOUNT: &str = charliebot_compat::file_urls::FILE_SERVER_MOUNT;
