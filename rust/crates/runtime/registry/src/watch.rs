//! A watch on the served template and static trees, for a render that must see an edit on disk.
//!
//! A render must notice an edit at its own request: the page a user opens carries the template and asset bytes that
//! exist when it is built. Reading every served file's `(mtime, size)` per render proves that and costs two statx
//! calls per served file. An inotify watch answers the same question with one read: the kernel queues an event for
//! every write, create, remove, rename or permission change below a watched directory, and a read returns the events
//! queued before it. A render that reads no event serves the tree state it read before; a render that reads an event
//! reads the trees again.
//!
//! [`TreeWatch::revision`] returns a counter that moves when a read drains at least one event. A caller caches its
//! tree state beside the counter it read and reads the trees again when the counter moves.
//!
//! A read that drained events registers the watches again from the directories on disk. An event does not say which
//! directory it created, and an overflowed queue drops events entirely, so a watch added only for the events a read
//! saw would miss a directory created in a dropped one. Re-registering also follows a symlinked directory to the
//! target it names now, and finds the nearest existing ancestor of a tree that was removed and recreated.
//!
//! A served file read through a symlink is watched at the file's own target, because an edit there raises no event
//! below the watched directories. A tree the process cannot watch, because the filesystem has no inotify, the watch
//! limit is spent, or a symlink in it does not resolve, leaves the watch blind: every read then answers a moved
//! counter, so the caller reads the trees again on every render, as a render without a watch does.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The changes a render must see: a file or directory written, created, removed, renamed or re-permissioned.
const EVENTS: u32 = libc::IN_ATTRIB
  | libc::IN_CLOSE_WRITE
  | libc::IN_CREATE
  | libc::IN_DELETE
  | libc::IN_MODIFY
  | libc::IN_MOVED_FROM
  | libc::IN_MOVED_TO
  | libc::IN_DELETE_SELF
  | libc::IN_MOVE_SELF;

/// The bytes one read takes from the queue: several events, each with a full file name.
const READ_BYTES: usize = 4096;

/// A watch on the directories of one set of trees.
pub(crate) struct TreeWatch {
  revision: AtomicU64,
  state: Mutex<WatchState>,
}

#[derive(Default)]
struct WatchState {
  /// The queue's descriptor, or `None` when no watch could be installed and every read reports a change.
  descriptor: Option<OwnedFd>,
  /// The trees a read registers again after it drained an event.
  trees: Vec<PathBuf>,
}

impl TreeWatch {
  /// A watch on each of `trees` and every directory below it, and on the nearest existing directory above it.
  pub(crate) fn new(trees: &[PathBuf]) -> TreeWatch {
    let watch = TreeWatch { revision: AtomicU64::new(1), state: Mutex::new(WatchState::default()) };
    let descriptor = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if descriptor < 0 {
      tracing::warn!(error = %io::Error::last_os_error(), "tree_watch_unavailable");
      return watch;
    }
    {
      let mut state = watch.state.lock().expect("the tree watch mutex is not poisoned");
      state.descriptor = Some(unsafe { OwnedFd::from_raw_fd(descriptor) });
      state.trees = trees.to_vec();
      watch.register(&mut state);
    }
    watch
  }

  /// The revision after draining the events queued so far: unchanged while no event arrived since the last read,
  /// and greater after an event.
  pub(crate) fn revision(&self) -> u64 {
    let mut state = self.state.lock().expect("the tree watch mutex is not poisoned");
    let Some(descriptor) = state.descriptor.as_ref().map(OwnedFd::as_raw_fd) else {
      return self.revision.fetch_add(1, Ordering::AcqRel) + 1;
    };
    let mut buffer = [0u8; READ_BYTES];
    let mut moved = false;
    loop {
      let count = unsafe { libc::read(descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
      if count <= 0 {
        break;
      }
      moved = true;
    }
    if !moved {
      return self.revision.load(Ordering::Acquire);
    }
    // A read that drained events registers every tree again, so a directory the queue reported no event for (a
    // dropped event, or a moved directory's replacement) is still watched below the revision this read returns.
    self.register(&mut state);
    self.revision.fetch_add(1, Ordering::AcqRel) + 1
  }

  /// Watch every tree, and the nearest existing directory above each.
  fn register(&self, state: &mut WatchState) {
    let trees = state.trees.clone();
    for tree in &trees {
      self.add_tree(state, tree);
      self.add_nearest_existing_ancestor(state, tree);
    }
  }

  /// Watch the nearest directory above `tree` that exists, so creating, replacing or retargeting `tree` is reported.
  ///
  /// A registered package may name a tree the checkout does not have, and a tree may be a symlink whose target the
  /// next read resolves. In both cases the link or directory that names the tree changes in its parent, which raises
  /// no event below the target this watch covers; the next read registers the tree as the parent now names it.
  fn add_nearest_existing_ancestor(&self, state: &mut WatchState, tree: &Path) {
    for ancestor in tree.ancestors().skip(1) {
      if ancestor.is_dir() {
        self.add_directory(state, ancestor);
        return;
      }
    }
  }

  /// Watch `directory` and every directory below it, and the target of every symlink below it.
  fn add_tree(&self, state: &mut WatchState, directory: &Path) {
    let mut ancestors = Vec::new();
    self.add_tree_below(state, directory, &mut ancestors);
  }

  fn add_tree_below(&self, state: &mut WatchState, directory: &Path, ancestors: &mut Vec<(u64, u64)>) {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = fs::metadata(directory) else { return };
    if !metadata.is_dir() {
      return;
    }
    let identity = (metadata.dev(), metadata.ino());
    if ancestors.contains(&identity) {
      return;
    }
    self.add_directory(state, directory);
    ancestors.push(identity);
    if let Ok(entries) = fs::read_dir(directory) {
      for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
          // A symlinked file is served through its target, and an edit to that target raises no event below the
          // watched directories. Watch the target itself; a link that does not resolve has no directory to watch.
          let Ok(target) = fs::canonicalize(&path) else {
            tracing::warn!(path = %path.display(), "tree_watch_unresolved_symlink");
            self.blind(state);
            return;
          };
          if fs::metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
            self.add_tree_below(state, &target, ancestors);
          } else {
            self.add_directory(state, &target);
          }
        } else if kind.is_dir() {
          self.add_tree_below(state, &path, ancestors);
        }
      }
    }
    ancestors.pop();
  }

  /// Watch one directory or file, or leave the watch blind when the kernel refuses the watch.
  fn add_directory(&self, state: &mut WatchState, path: &Path) {
    let Some(descriptor) = state.descriptor.as_ref().map(OwnedFd::as_raw_fd) else { return };
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
      self.blind(state);
      return;
    };
    let watch = unsafe { libc::inotify_add_watch(descriptor, path.as_ptr(), EVENTS) };
    if watch < 0 {
      let error = io::Error::last_os_error();
      if error.kind() == io::ErrorKind::NotFound {
        // The path went away between the event that named it and this watch; nothing below it needs one now.
        return;
      }
      tracing::warn!(error = %error, "tree_watch_unavailable");
      self.blind(state);
    }
  }

  /// Stop relying on the queue: every read now reports a change.
  fn blind(&self, state: &mut WatchState) {
    state.descriptor = None;
    state.trees.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_write_below_a_watched_tree_moves_the_revision_once() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("page.html"), "one").unwrap();
    let watch = TreeWatch::new(std::slice::from_ref(&tree));
    let quiet = watch.revision();
    assert_eq!(watch.revision(), quiet, "a read that drained no event keeps the revision");

    fs::write(tree.join("page.html"), "two, longer").unwrap();
    let moved = watch.revision();
    assert!(moved > quiet, "the write moves the revision");
    assert_eq!(watch.revision(), moved, "the drained event is not reported again");
  }

  #[test]
  fn a_directory_created_below_a_watched_tree_reports_its_own_edits() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    fs::create_dir(&tree).unwrap();
    let watch = TreeWatch::new(std::slice::from_ref(&tree));
    let before = watch.revision();
    let created = tree.join("parts");
    fs::create_dir(&created).unwrap();
    let after_create = watch.revision();
    assert!(after_create > before, "the created directory moves the revision");

    fs::write(created.join("part.html"), "part").unwrap();
    assert!(watch.revision() > after_create, "an edit below the created directory moves the revision");
  }

  #[test]
  fn an_absent_tree_does_not_blind_the_watch() {
    let root = tempfile::tempdir().unwrap();
    let present = root.path().join("present");
    fs::create_dir(&present).unwrap();
    let absent = root.path().join("absent");
    let watch = TreeWatch::new(&[present, absent.clone()]);
    let quiet = watch.revision();
    assert_eq!(watch.revision(), quiet, "a tree the checkout does not have leaves the watch quiet");

    fs::create_dir(&absent).unwrap();
    let created = watch.revision();
    assert!(created > quiet, "creating the absent tree moves the revision");
    fs::write(absent.join("page.html"), "page").unwrap();
    assert!(watch.revision() > created, "an edit below the created tree moves the revision");
  }

  #[test]
  fn a_tree_removed_and_recreated_is_watched_again() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    fs::create_dir(&tree).unwrap();
    let watch = TreeWatch::new(std::slice::from_ref(&tree));
    let before = watch.revision();

    fs::remove_dir_all(&tree).unwrap();
    fs::create_dir(&tree).unwrap();
    let recreated = watch.revision();
    assert!(recreated > before, "removing and recreating the tree moves the revision");

    fs::write(tree.join("page.html"), "page").unwrap();
    assert!(watch.revision() > recreated, "an edit inside the recreated tree moves the revision");
  }

  #[test]
  fn an_edit_through_a_symlinked_file_moves_the_revision() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    let outside = root.path().join("outside");
    fs::create_dir(&tree).unwrap();
    fs::create_dir(&outside).unwrap();
    let target = outside.join("page.html");
    fs::write(&target, "one").unwrap();
    std::os::unix::fs::symlink(&target, tree.join("page.html")).unwrap();
    let watch = TreeWatch::new(&[tree]);
    let before = watch.revision();
    assert_eq!(watch.revision(), before, "reading the link's directories moves nothing");

    fs::write(&target, "two, longer").unwrap();
    assert!(watch.revision() > before, "an edit to the link's target moves the revision");
  }

  #[test]
  fn retargeting_a_symlinked_tree_moves_the_revision() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    fs::write(first.join("page.html"), "one").unwrap();
    fs::write(second.join("page.html"), "two").unwrap();
    let tree = root.path().join("tree");
    std::os::unix::fs::symlink(&first, &tree).unwrap();
    let watch = TreeWatch::new(std::slice::from_ref(&tree));
    let before = watch.revision();

    // A retargeted link changes the link's own directory, and raises no event below either target.
    let replacement = root.path().join("replacement");
    std::os::unix::fs::symlink(&second, &replacement).unwrap();
    fs::rename(&replacement, &tree).unwrap();
    let retargeted = watch.revision();
    assert!(retargeted > before, "retargeting the tree link moves the revision");
    fs::write(second.join("page.html"), "two, longer").unwrap();
    assert!(watch.revision() > retargeted, "an edit in the new target moves the revision");
  }

  #[test]
  fn a_link_that_does_not_resolve_leaves_the_watch_blind() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    fs::create_dir(&tree).unwrap();
    std::os::unix::fs::symlink(root.path().join("missing"), tree.join("missing.html")).unwrap();
    let watch = TreeWatch::new(&[tree]);
    let first = watch.revision();
    assert!(watch.revision() > first, "a blind watch reports every read as a change");
  }
}
