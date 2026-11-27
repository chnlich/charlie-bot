#!/usr/bin/env perl
# Run one synced-tree command or stop/remove a build-host tree.
#
# The runner sends this program one base64 payload: the synced tree root, the directory to run in,
# the CPython version the local project pins, and the command. On the build host it prepares the
# command's world and then runs it. Preparation, in order:
#
# - a short TMPDIR on the /dev/shm tmpfs, so release trial socket paths fit the trial's sun_path
#   preflight and a test's fsync does not wait on the build host's disk;
# - node_modules, when the tree has a package lockfile and no node_modules;
# - a rust-toolchain.toml's channel with its components and targets, and its targets otherwise;
# - the environment: the runner's cargo, uv, and node, then ~/.local/go/bin and ~/.pixi/bin;
# - a build slot with sccache for cargo and cargo-capable uv commands.
#
# A command run makes this process a child subreaper, so tmux servers and other daemons are adopted
# instead of reparented away. It stops descendants when the command exits, receives a signal, or
# loses its SSH output channel. Before a sync replaces a tree, its check keeps the tree when the activity lock or a
# process (cwd, exe, open fd or command line) holds it. The sweep uses the same checks and never stops a process.
# A shell cannot make itself a child subreaper, which is why this program is perl.
#
# A tree's owner record and the tree change together under the tree owner lock: a claim writes the record and
# then creates the tree, and a sweep drops a record whose tree is missing or removes the tree and then its records.
# The sweep therefore never sees a claim half done. A tree's last-use record is written as the runner syncs or runs
# in the tree, and the sweep reads it without walking the tree.

use strict;
use warnings;

use Fcntl qw(:flock O_RDWR O_WRONLY O_CREAT O_TRUNC O_APPEND);
use Cwd qw(abs_path);
use Errno qw(EACCES EINTR ENOENT ESRCH EWOULDBLOCK EAGAIN);
use File::Path qw(rmtree remove_tree);
use File::Find qw(find);
use IO::Poll qw(POLLERR POLLHUP POLLNVAL POLLOUT);
use MIME::Base64 qw(decode_base64);
use POSIX qw(uname strftime WNOHANG);
use Time::HiRes qw(time clock_gettime CLOCK_MONOTONIC);

my $STOP_WAIT_SECONDS = 8.0;
my $TERM_PHASE_SECONDS = 2.0;
my $SLOT_POLL_SECONDS = 0.5;
my $SLOT_WARN_AFTER_SECONDS = 5.0;
my $SLOT_COUNT = 4;
my $SCCACHE_CACHE_SIZE = '50G';
my $CARGO_BUILD_JOBS = '4';
# TMPDIR lives on a tmpfs, where fsync returns at once. On a disk each fsync waits for the drive's flush, and a
# test that commits a SQLite database dozens of times then runs past the 0.1 s unit-test limit.
# The name is <prefix><pid>-<start ticks> in hex: at most 16 bytes for pid_max 327680 and a start below 2^36
# ticks, so TMPDIR is at most 25 bytes. The release trial's default temp root puts Chrome's socket at TMPDIR + 60 bytes
# and, for a 6-digit uid, tmux's socket at TMPDIR + 43 bytes. A serve test's --tree-root gives Chrome's socket at TMPDIR
# + 77 bytes. The preflight tests exercise the paths against trial.sh's socket-path limit.
my $TEMP_ROOT = '/dev/shm/';
my $TEMP_PREFIX = 'r';

my $PR_SET_PDEATHSIG = 1;
my $PR_SET_CHILD_SUBREAPER = 36;
my $SIGTERM_NUMBER = 15;
my %PRCTL_SYSCALL = ('x86_64' => 157, 'aarch64' => 167);

my $shutdown_signal;    # The number of the first signal received, once received.
my $descendant_stop_failed = 0;
my $base;               # The runner's state directory on this host, under $HOME.

sub fail {
  my ($code, $message) = @_;
  print STDERR "remote-exec: $message\n";
  exit $code;
}

sub prctl_or_die {
  my ($option, $value) = @_;
  my ($machine) = (uname())[4];
  my $number = $PRCTL_SYSCALL{$machine};
  fail(2, "no prctl syscall number for $machine") unless defined $number;
  syscall($number, $option, $value, 0, 0, 0) == 0 or fail(2, "prctl($option) failed: $!");
}

sub monotonic {
  return clock_gettime(CLOCK_MONOTONIC());
}

# The current time in whole epoch seconds, the unit of a tree's last-use record.
sub now_seconds {
  return int(time());
}

sub which {
  my ($path, $name) = @_;
  for my $directory (split /:/, $path) {
    return "$directory/$name" if -x "$directory/$name";
  }
  return;
}

# The base is $HOME-relative by rule: no absolute path, no leading ~, no climb out of $HOME.
sub resolve_base {
  my $value = $ENV{REMOTE_RUN_REMOTE_BASE};
  $value = 'scripts/remote-run' unless defined $value && $value ne '';
  fail(2, "REMOTE_RUN_REMOTE_BASE must be a \$HOME-relative path: $value")
    if $value =~ m{\A/} || $value =~ m{\A~/} || $value =~ m{(^|/)\.\.(/|\z)} || $value =~ /\n/;
  $value =~ s{/+\z}{};
  return "$ENV{HOME}/$value";
}

sub decode_payload {
  my ($encoded) = @_;
  my $raw = eval { decode_base64($encoded) };
  fail(2, 'invalid command payload: not base64') unless defined $raw;
  fail(2, 'command payload is missing its final NUL byte') unless $raw =~ /\0\z/;
  my @fields = split /\0/, $raw, -1;
  pop @fields;
  fail(2, 'command payload needs a tree, a directory, a Python version, and a command')
    unless @fields >= 4;
  my $tree = shift @fields;
  my $relative_dir = shift @fields;
  my $python_version = shift @fields;
  my $argv = \@fields;
  $tree =~ s{\A~}{$ENV{HOME}};
  fail(2, "tree root is outside $base: $tree")
    unless $tree =~ m{\A/} && index($tree, "$base/") == 0;
  fail(2, "tree-relative directory is invalid: $relative_dir")
    if $relative_dir =~ m{\A/} || $relative_dir =~ m{(^|/)\.\.(/|\z)};
  if ($python_version ne '') {
    fail(2, "invalid CPython version: $python_version")
      unless $python_version =~ /\A3\.[0-9]+\.[0-9]+\z/;
  }
  fail(2, 'command is empty') unless @$argv && $argv->[0] ne '';
  return ($tree, $relative_dir, $python_version, $argv);
}

# Every request to stop arrives here first, so one handler records it and the loops watch it.
sub install_signal_handlers {
  $SIG{PIPE} = 'IGNORE';
  for my $number ($SIGTERM_NUMBER, 2, 1) {
    $SIG{$number == $SIGTERM_NUMBER ? 'TERM' : $number == 2 ? 'INT' : 'HUP'} = sub {
      $shutdown_signal = $number if !defined $shutdown_signal;
    };
  }
}

sub direct_children {
  my ($pid) = @_;
  local $! = 0;
  opendir my $tasks, "/proc/$pid/task" or do {
    return [] if $!{ENOENT} || $!{ESRCH};
    fail(2, "cannot list child tasks for process $pid: $!");
  };
  my %children;
  my @tasks = grep !/\A\.\.?\z/, readdir $tasks;
  fail(2, "cannot read child tasks for process $pid: $!") if $!;
  for my $task (@tasks) {
    if (open my $children_file, '<', "/proc/$pid/task/$task/children") {
      local $! = 0;
      my $line = <$children_file>;
      if (!defined($line) && $!) {
        next if $!{ENOENT} || $!{ESRCH};
        fail(2, "cannot read child list for process $pid task $task: $!");
      }
      $line //= '';
      $children{$_} = 1 for split ' ', $line;
    } elsif (!$!{ENOENT} && !$!{ESRCH}) {
      fail(2, "cannot read child list for process $pid task $task: $!");
    }
  }
  return [sort { $a <=> $b } keys %children];
}

sub descendant_pids {
  my ($root_pid) = @_;
  my @found;
  my @pending = ($root_pid);
  my %seen;
  while (@pending) {
    my $parent = pop @pending;
    next if $seen{$parent}++;
    my $children = direct_children($parent);
    push @found, @$children;
    push @pending, @$children;
  }
  return \@found;
}

# Reap exited children by pid: waitpid(-1) could take the command's status before the wait loop does.
sub reap_adopted_children {
  my ($command_pid) = @_;
  for my $pid (@{direct_children($$)}) {
    waitpid $pid, WNOHANG if $pid != $command_pid;
  }
}

sub command_line {
  my ($pid) = @_;
  open my $file, '<', "/proc/$pid/cmdline" or return 'exited';
  local $/;
  my $raw = <$file> // '';
  my @words = grep { length } split /\0/, $raw;
  return @words ? join(' ', @words) : 'zombie';
}

# Print nothing until the wait ends: a write to stderr fails once ssh is gone, and that must not
# stop the signals.
sub stop_descendants {
  my $deadline = time() + $STOP_WAIT_SECONDS;
  my $term_deadline = time() + $TERM_PHASE_SECONDS;
  while (1) {
    # Reap the adopted children that exited: a zombie keeps its /proc entry, so an unreaped one
    # would hold the wait until the deadline.
    waitpid $_, WNOHANG for @{direct_children($$)};
    my $pids = descendant_pids($$);
    return 0 unless @$pids;
    if (time() >= $deadline) {
      my @lines = map {
        "remote-exec: process $_ outlived the ${STOP_WAIT_SECONDS}s stop wait: " . command_line($_)
      } @$pids;
      print STDERR "$_\n" for @lines;
      return 1;
    }
    my $name = time() < $term_deadline ? 'TERM' : 'KILL';
    for my $pid (reverse @$pids) {
      kill $name, $pid;    # The process may have exited after it was listed.
    }
    select undef, undef, undef, 0.05;
  }
}

sub process_start_ticks {
  my ($pid) = @_;
  open my $file, '<', "/proc/$pid/stat" or return;
  local $/;
  my $stat = <$file>;
  if (!defined $stat) {
    $! = ESRCH unless $!;
    return;
  }
  if ($stat eq '') {
    $! = ESRCH;
    return;
  }
  # The fields after the parenthesised command name start at field 3, so field 22 is index 19.
  my @fields = split ' ', ($stat =~ s/.*\)//r);
  $! = ESRCH unless defined $fields[19];
  return $fields[19];
}

sub path_is_inside_tree {
  my ($path, $tree) = @_;
  $path =~ s/ \(deleted\)\z//;
  return $path eq $tree || index($path, "$tree/") == 0;
}

sub tree_activity_lock_path {
  my ($name) = @_;
  # Keep this lock file after tree removal: processes may already hold its inode open.
  return "$base/tree-locks/$name.lock";
}

sub lock_tree_activity {
  my ($name) = @_;
  my $lock_dir = "$base/tree-locks";
  if (!-d $lock_dir) {
    mkdir $lock_dir, 0700 or fail(2, "cannot create tree activity lock directory $lock_dir: $!");
  }
  my $path = tree_activity_lock_path($name);
  sysopen my $handle, $path, O_RDWR | O_CREAT, 0600 or fail(2, "cannot open tree activity lock $path: $!");
  flock $handle, LOCK_EX or fail(2, "cannot lock build host tree $name: $!");
  return $handle;
}

# Returns a held handle when the lock is free, 'locked' when another run holds it, or undef when
# this tree predates per-tree activity locks.
sub try_lock_tree_activity {
  my ($name, $create) = @_;
  my $lock_dir = "$base/tree-locks";
  if ($create && !-d $lock_dir) {
    mkdir $lock_dir, 0700 or -d $lock_dir or fail(2, "cannot create tree activity lock directory $lock_dir: $!");
  }
  my $path = tree_activity_lock_path($name);
  return unless -e $path || -l $path;
  fail(2, "tree activity lock is not a regular file: $path") unless -f $path && !-l $path;
  sysopen my $handle, $path, O_RDWR or fail(2, "cannot open tree activity lock $path: $!");
  if (flock $handle, LOCK_EX | LOCK_NB) {
    return $handle;
  }
  return 'locked' if $!{EWOULDBLOCK} || $!{EAGAIN};
  fail(2, "cannot check tree activity lock $path: $!");
}

# The newest mtime and allocated bytes across the tree, including ignored build output and .git.
sub tree_metrics {
  my ($tree) = @_;
  my ($latest_mtime, $allocated_bytes) = (undef, 0);
  my %seen_inodes;
  my $find_warning = '';
  {
    local $SIG{__WARN__} = sub { $find_warning .= $_[0] };
    find(
      {
        no_chdir => 1,
        wanted => sub {
          my $path = $File::Find::name;
          my @stat = lstat $path;
          fail(2, "cannot inspect build host tree entry $path: $!") unless @stat;
          $latest_mtime = $stat[9] if !defined($latest_mtime) || $stat[9] > $latest_mtime;
          my $inode = "$stat[0]:$stat[1]";
          if (!$seen_inodes{$inode}++) {
            fail(2, "cannot inspect allocated size of build host tree entry $path") unless defined $stat[12];
            $allocated_bytes += $stat[12] * 512;
          }
        },
      },
      $tree,
    );
  }
  fail(2, "cannot scan build host tree $tree: $find_warning") if $find_warning ne '';
  fail(2, "cannot determine build host tree age: $tree has no entries") unless defined $latest_mtime;
  return ($latest_mtime, $allocated_bytes);
}

# A tree's last use is the epoch second when the runner last synced or ran in it. The sweep reads this record instead
# of walking the tree, so a sweep costs one small read per tree.
sub last_use_path {
  my ($name) = @_;
  return "$base/tree-last-use/$name";
}

# The recorded last use, or undef when the tree has no last-use record.
sub read_last_use {
  my ($name) = @_;
  my $path = last_use_path($name);
  return unless -e $path || -l $path;
  fail(2, "tree last-use record is not a regular file: $path") unless -f $path && !-l $path;
  open my $file, '<', $path or fail(2, "cannot read tree last-use record $path: $!");
  my $text = <$file>;
  close $file or fail(2, "cannot close tree last-use record $path: $!");
  fail(2, "tree last-use record is invalid: $path") unless defined $text && $text =~ /\A([0-9]+)\n\z/;
  return 0 + $1;
}

# Replaces the tree's last-use record in one rename, so a concurrent sweep reads either the old record or the new one.
sub write_last_use {
  my ($name, $epoch) = @_;
  my $dir = "$base/tree-last-use";
  mkdir $dir, 0700 or -d $dir or fail(2, "cannot create tree last-use directory $dir: $!");
  my $path = last_use_path($name);
  my $temp = "$path.tmp.$$";
  sysopen my $file, $temp, O_WRONLY | O_CREAT | O_TRUNC, 0600
    or fail(2, "cannot write tree last-use record $temp: $!");
  my $written = print {$file} "$epoch\n";
  close($file) && $written or fail(2, "cannot write tree last-use record $temp: $!");
  rename $temp, $path or fail(2, "cannot move tree last-use record into place at $path: $!");
}

# The tree's last use. A tree without a last-use record takes its content age once. A real sweep records that age,
# so the next sweep reads the record instead of walking the tree.
sub tree_last_use {
  my ($name, $tree, $dry_run) = @_;
  my $recorded = read_last_use($name);
  return $recorded if defined $recorded;
  my ($content_mtime) = tree_metrics($tree);
  write_last_use($name, $content_mtime) unless $dry_run;
  return $content_mtime;
}

# Removes a tree's owner record and last-use record, each when present.
sub forget_tree_records {
  my ($name, $owner_path) = @_;
  for my $path ($owner_path, last_use_path($name)) {
    next unless -e $path || -l $path;
    unlink $path or fail(2, "cannot remove $path: $!");
  }
}

sub tree_is_dirty {
  my ($tree) = @_;
  return 0 unless -e "$tree/.git" || -l "$tree/.git";
  local $ENV{GIT_OPTIONAL_LOCKS} = 0;
  open my $git, '-|', 'git', '-C', $tree, 'status', '--porcelain', '--untracked-files=all'
    or fail(2, "cannot check Git status in build host tree $tree: $!");
  local $/;
  my $status = <$git> // '';
  close $git or fail(2, "Git status failed in build host tree $tree: $!");
  return $status ne '';
}

# The mode-0700 base prevents another unprivileged uid from entering a tree. Scan visible processes for references to it.
sub process_uses_tree {
  my ($tree, $ignored_cmdline_pids) = @_;
  local $! = 0;
  opendir my $proc, '/proc' or fail(2, "cannot list /proc while checking $tree: $!");
  my @entries = readdir $proc;
  fail(2, "cannot read /proc while checking $tree: $!") if $!;
  for my $entry (grep { /\A[0-9]+\z/ } @entries) {
    my $pid = 0 + $entry;
    my @proc_stat = stat "/proc/$pid";
    if (!@proc_stat) {
      next if $!{ENOENT} || $!{ESRCH} || $!{EACCES};
      fail(2, "cannot inspect process $pid while checking $tree: $!");
    }
    for my $name (qw(cwd exe)) {
      my $path = readlink "/proc/$pid/$name";
      if (!defined $path) {
        next if $!{ENOENT} || $!{ESRCH} || $!{EACCES};
        fail(2, "cannot inspect process $pid $name while checking $tree: $!");
      }
      return { pid => $pid, kind => $name } if path_is_inside_tree($path, $tree);
    }
    local $! = 0;
    if (opendir my $fds, "/proc/$pid/fd") {
      $! = 0;
      my @fd_entries = grep { /\A[0-9]+\z/ } readdir $fds;
      if ($!) {
        next if $!{ENOENT} || $!{ESRCH};
        fail(2, "cannot read process $pid file descriptors while checking $tree: $!");
      }
      for my $fd (@fd_entries) {
        my $path = readlink "/proc/$pid/fd/$fd";
        if (!defined $path) {
          next if $!{ENOENT} || $!{ESRCH} || $!{EACCES};
          fail(2, "cannot inspect process $pid file descriptor $fd while checking $tree: $!");
        }
        return { pid => $pid, kind => "fd/$fd" } if path_is_inside_tree($path, $tree);
      }
    } elsif (!$!{ENOENT} && !$!{ESRCH} && !$!{EACCES}) {
      fail(2, "cannot inspect process $pid file descriptors while checking $tree: $!");
    }
    next if $ignored_cmdline_pids->{$pid};
    if (open my $cmdline, '<', "/proc/$pid/cmdline") {
      local $/;
      my $command = <$cmdline> // '';
      # The driver receives the absolute --run-tree path; a home-relative suffix can name another checkout.
      return { pid => $pid, kind => 'cmdline' } if index($command, $tree) >= 0;
    } else {
      next if $!{ENOENT} || $!{ESRCH} || $!{EACCES};
      fail(2, "cannot read process $pid command line while checking $tree: $!");
    }
  }
  return;
}

# Serializes every change to an owner record and its tree. The pre-sync check changes neither.
sub lock_tree_owners {
  my $path = "$base/tree-owners.lock";
  sysopen my $handle, $path, O_RDWR | O_CREAT, 0600 or fail(2, "cannot open the tree owner lock $path: $!");
  flock $handle, LOCK_EX or fail(2, "cannot lock the tree owners at $path: $!");
  return $handle;
}

# The tree's name and the state owner's uid, once the caller host and the tree's place under the trees directory
# and the state directory's mode are checked. Every tree mode starts here.
sub tree_request {
  my ($caller_host, $requested_tree) = @_;
  fail(2, "invalid caller host: $caller_host") unless $caller_host =~ /\A[A-Za-z0-9._-]+\z/;
  my $trees_root = abs_path("$base/trees");
  fail(2, "cannot resolve build tree directory $base/trees: $!") unless defined $trees_root;
  my ($requested_parent, $name) = $requested_tree =~ m{\A(.+)/([A-Za-z0-9._-]+)\z};
  fail(2, "refusing to operate on a path outside the build host trees: $requested_tree") unless defined $name;
  my $canonical_parent = abs_path($requested_parent);
  fail(2, "cannot resolve build tree parent $requested_parent: $!") unless defined $canonical_parent;
  fail(2, "refusing to operate on a path outside the build host trees: $requested_tree")
    unless $canonical_parent eq $trees_root;

  my @base_stat = stat $base;
  fail(2, "cannot inspect remote-run state directory $base: $!") unless @base_stat;
  fail(2, "remote-run state directory must be private (mode 0700): $base") if $base_stat[2] & 0077;
  return ($name, $base_stat[4]);
}

# The record's caller host, local root, and format. A path-only record is the legacy format.
sub read_owner_record {
  my ($owner_path) = @_;
  fail(2, "tree owner record is missing or is a symlink: $owner_path") unless -f $owner_path && !-l $owner_path;
  open my $owner_file, '<', $owner_path or fail(2, "cannot read tree owner record $owner_path: $!");
  my $owner;
  {
    local $/;
    $owner = <$owner_file> // '';
  }
  $owner =~ s/\n\z//;
  if ($owner =~ /\A(\/[^\n]*)\z/) {
    return (undef, $1, 'legacy');
  }
  my ($recorded_host, $root) = $owner =~ /\Ahost=([A-Za-z0-9._-]+)\nroot=(\/[^\n]*)\z/;
  fail(2, "tree owner record is invalid: $owner_path") unless defined $recorded_host;
  return ($recorded_host, $root, 'current');
}

# The tree at $requested_tree, or nothing when no entry is there. A symlink, a non-directory, or a tree the state
# owner does not own is an error.
sub present_tree {
  my ($requested_tree, $name, $base_uid) = @_;
  return unless -e $requested_tree || -l $requested_tree;
  fail(2, "build host tree is not a directory or is a symlink: $requested_tree")
    unless -d $requested_tree && !-l $requested_tree;
  my $tree = abs_path($requested_tree);
  fail(2, "cannot resolve build host tree $requested_tree: $!") unless defined $tree;
  my $trees_root = abs_path("$base/trees");
  fail(2, "refusing to operate on a path outside the build host trees: $requested_tree")
    unless $tree eq "$trees_root/$name";
  my @tree_stat = stat $tree;
  fail(2, "cannot inspect build host tree $tree: $!") unless @tree_stat;
  fail(2, "build host tree $tree is not owned by the remote-run state owner") unless $tree_stat[4] == $base_uid;
  return ($tree, $tree_stat[4]);
}

sub write_owner_record {
  my ($owner_path, $caller_host, $root) = @_;
  sysopen my $file, $owner_path, O_WRONLY | O_CREAT | O_TRUNC, 0644
    or fail(2, "cannot write tree owner record $owner_path: $!");
  my $written = print {$file} "host=$caller_host\nroot=$root\n";
  return if $written && close $file;
  fail_claim($owner_path, "cannot write tree owner record $owner_path: $!");
}

# Removes the record that a failed claim wrote, then fails with $message.
sub fail_claim {
  my ($owner_path, $message) = @_;
  unlink $owner_path or fail(2, "$message; cannot remove the record $owner_path: $!");
  fail(2, $message);
}

sub check_tree_before_sync {
  my ($caller_host, $requested_tree, $encoded_root) = @_;
  my ($name, $base_uid) = tree_request($caller_host, $requested_tree);
  my $owner_lock = lock_tree_owners();
  my $root = decode_base64($encoded_root // '');
  fail(2, "the local root of build host tree $name is not an absolute path") unless $root =~ m{\A/[^\n]*\z};
  my $owner_path = "$base/tree-owners/$name";
  if (!-e $owner_path && !-l $owner_path) {
    my ($tree) = present_tree($requested_tree, $name, $base_uid);
    return 'absent' unless defined $tree;
    fail(2, "build host tree $name exists without an owner record");
  }
  my ($recorded_host, $recorded_root, $format) = read_owner_record($owner_path);
  if ($format eq 'legacy') {
    fail(2, "tree $name has a legacy owner record for a different local directory") unless $root eq $recorded_root;
  } else {
    fail(2, "tree $name belongs to caller host $recorded_host, not $caller_host") unless $recorded_host eq $caller_host;
    fail(2, "tree $name belongs to local directory $recorded_root, not $root") unless $recorded_root eq $root;
  }
  my ($tree) = present_tree($requested_tree, $name, $base_uid);
  return 'absent' unless defined $tree;
  my $activity_lock = try_lock_tree_activity($name, 1);
  return 'kept-locked' if defined($activity_lock) && !ref($activity_lock);
  my $process = process_uses_tree($tree, { $$ => 1, getppid() => 1 });
  return "kept-in-use:$process->{pid}:$process->{kind}" if defined $process;
  return 'ready';
}

# Removes the tree, then its owner record and last-use record. A failed removal leaves all of them in place.
sub remove_tree_and_record {
  my ($name, $tree, $owner_path) = @_;
  my $errors = [];
  remove_tree($tree, { error => \$errors });
  if (@$errors || -e $tree) {
    my @details = map { my ($path, $message) = %$_; "$path: $message" } @$errors;
    push @details, "$tree still exists" if -e $tree;
    fail(2, "cannot remove build host tree $tree: " . join('; ', @details));
  }
  forget_tree_records($name, $owner_path);
}

# One owner record's sweep, decided under the owner lock. A tree whose current owner record names a local root that is
# gone is removed at once. Every other tree is removed once its last use is older than the retention age: a tree with a
# live root, a legacy record, or no record. Each removal passes the activity lock and the process check first, and this
# sweep never stops a process. A missing tree drops its record whatever the root state, because no tree is then shared.
sub sweep_build_tree {
  my ($caller_host, $requested_tree, $root_state, $retention_days, $dry_run) = @_;
  fail(2, "the recorded local directory must be live or gone: $root_state") unless $root_state =~ /\A(?:live|gone)\z/;
  fail(2, "tree retention age must be a positive whole number of days: $retention_days")
    unless $retention_days =~ /\A[1-9][0-9]*\z/;
  $dry_run = 0 unless defined $dry_run;
  fail(2, "dry-run must be 0 or 1: $dry_run") unless $dry_run =~ /\A[01]\z/;
  my ($name, $base_uid) = tree_request($caller_host, $requested_tree);
  my $lock = lock_tree_owners();
  my $owner_path = "$base/tree-owners/$name";
  my $owner_format = 'none';
  if (-e $owner_path || -l $owner_path) {
    my ($recorded_host, undef, $format) = read_owner_record($owner_path);
    return 'kept-foreign' if $format eq 'current' && $recorded_host ne $caller_host;
    $owner_format = $format;
  }
  my ($tree, $tree_uid) = present_tree($requested_tree, $name, $base_uid);
  if (!defined $tree) {
    return 'absent' if $owner_format eq 'none';
    return 'would-drop' if $dry_run;
    forget_tree_records($name, $owner_path);
    return 'dropped';
  }
  my $activity_lock = try_lock_tree_activity($name, !$dry_run);
  return 'kept-locked' if defined($activity_lock) && !ref($activity_lock);
  my $ignored_cmdline_pids = { $$ => 1, getppid() => 1 };

  if ($owner_format eq 'current' && $root_state eq 'gone') {
    my $process = process_uses_tree($tree, $ignored_cmdline_pids);
    return "kept-in-use:$process->{pid}:$process->{kind}" if defined $process;
    if ($dry_run) {
      my (undef, $bytes) = tree_metrics($tree);
      return "would-remove-gone:$bytes";
    }
    remove_tree_and_record($name, $tree, $owner_path);
    return 'removed-gone';
  }

  my $last_use = tree_last_use($name, $tree, $dry_run);
  return 'kept-young' if time() - $last_use < $retention_days * 86400;
  return 'kept-dirty' if tree_is_dirty($tree);
  my $process = process_uses_tree($tree, $ignored_cmdline_pids);
  return "kept-in-use:$process->{pid}:$process->{kind}" if defined $process;
  my (undef, $bytes) = tree_metrics($tree);
  return "would-remove-age:$bytes" if $dry_run;
  remove_tree_and_record($name, $tree, $owner_path);
  return "removed-age:$bytes";
}

# A caller host's claim on $requested_tree for its local root, under the owner lock. The record is written before the
# tree is created, so a claim that stops between the two leaves a record the sweep drops, never a tree without its
# record. A tree that exists is accepted only with the record of this same claim.
sub claim_build_tree {
  my ($caller_host, $requested_tree, $encoded_root) = @_;
  my ($name, $base_uid) = tree_request($caller_host, $requested_tree);
  my $root = decode_base64($encoded_root);
  fail(2, "the local root of build host tree $name is not an absolute path") unless $root =~ m{\A/[^\n]*\z};
  my $lock = lock_tree_owners();
  my $owner_path = "$base/tree-owners/$name";
  my $recorded = -e $owner_path || -l $owner_path;
  my $upgrade_legacy = 0;
  if ($recorded) {
    my ($recorded_host, $recorded_root, $format) = read_owner_record($owner_path);
    if ($format eq 'legacy') {
      fail(2, "tree $name belongs to legacy local directory $recorded_root; this claim is local directory $root")
        unless $recorded_root eq $root;
      $upgrade_legacy = 1;
    } else {
      fail(2, "tree $name belongs to caller host $recorded_host at $recorded_root; "
          . "this claim is caller host $caller_host at $root")
        unless $recorded_host eq $caller_host && $recorded_root eq $root;
    }
  }
  my $activity_lock = lock_tree_activity($name);
  my ($tree) = present_tree($requested_tree, $name, $base_uid);
  if (defined $tree) {
    fail(2, "build host tree $name exists without an owner record") unless $recorded;
    write_owner_record($owner_path, $caller_host, $root) if $upgrade_legacy;
  } else {
    write_owner_record($owner_path, $caller_host, $root) if !$recorded || $upgrade_legacy;
    mkdir $requested_tree, 0777 or fail_claim($owner_path, "cannot create build host tree $requested_tree: $!");
  }
  write_last_use($name, now_seconds());
}

sub caller_output_channel_closed {
  my $poll = IO::Poll->new();
  $poll->mask(\*STDOUT, POLLOUT);
  my $ready = $poll->poll(0);
  if ($ready < 0) {
    return 0 if $!{EINTR};
    fail(2, "cannot check the caller output channel: $!");
  }
  return ($poll->events(\*STDOUT) & (POLLERR | POLLHUP | POLLNVAL)) != 0;
}

# A descendant that outlived its remote-exec keeps the TMPDIR it was started with.
sub temp_dir_in_use {
  my ($path) = @_;
  my $marker = "TMPDIR=$path\0";
  for my $environ (glob '/proc/[0-9]*/environ') {
    if (open my $file, '<', $environ) {
      local $/;
      my $environ_text = <$file> // '';
      return 1 if index($environ_text, $marker) >= 0;
    }
  }
  return 0;
}

sub remove_stale_temp_dirs {
  for my $entry (glob "$TEMP_ROOT$TEMP_PREFIX*") {
    next unless -d $entry && -o $entry;
    my ($owner_pid, $owner_start) = $entry =~ m{\A\Q$TEMP_ROOT$TEMP_PREFIX\E([0-9a-f]+)-([0-9a-f]+)\z} or next;
    my $start_ticks = process_start_ticks(hex $owner_pid);
    next if defined $start_ticks && sprintf('%x', $start_ticks) eq $owner_start;
    next if temp_dir_in_use($entry);
    rmtree $entry;    # A concurrent remote-exec may have removed it first.
  }
}

sub create_temp_dir {
  my $path = sprintf '%s%s%x-%x', $TEMP_ROOT, $TEMP_PREFIX, $$, process_start_ticks($$);
  fail(2, "cannot create the command's temp directory $path: $!") unless mkdir $path, 0700;
  return $path;
}

sub acquire_slot {
  my ($wait_started) = @_;
  my @handles;
  for my $index (1 .. $SLOT_COUNT) {
    my $path = "$base/slots/slot-$index.lock";
    fail(2, "build slot is missing: $path; run setup") unless -f $path;
    if (sysopen my $handle, $path, O_RDWR) {
      push @handles, $handle;
    } else {
      fail(2, "cannot open the build slot $path: $!");
    }
  }
  my $warned;
  while (!defined $shutdown_signal) {
    if (caller_output_channel_closed()) {
      $shutdown_signal = $SIGTERM_NUMBER;
      last;
    }
    for my $handle (@handles) {
      next unless flock $handle, LOCK_EX | LOCK_NB;
      my $acquired_at = monotonic();
      my $waited = $acquired_at - $wait_started;
      if (!$warned && $waited > $SLOT_WARN_AFTER_SECONDS) {
        print STDERR "remote-exec: waiting for a build slot\n";
        $warned = 1;
      }
      close $_ for grep { $_ != $handle } @handles;
      return ($handle, $waited, $acquired_at);
    }
    if (!$warned && monotonic() - $wait_started > $SLOT_WARN_AFTER_SECONDS) {
      print STDERR "remote-exec: waiting for a build slot\n";
      $warned = 1;
    }
    select undef, undef, undef, $SLOT_POLL_SECONDS;
  }
  close $_ for @handles;
  return (undef, monotonic() - $wait_started, 0);
}

sub release_slot {
  my ($handle, $started_at, $wait_seconds, $acquired_at, $argv) = @_;
  my $released_at;
  if (flock $handle, LOCK_UN) {
    $released_at = monotonic();
  }
  close $handle;
  return unless defined $released_at;
  my $path = "$base/slot-waits.log";
  if (sysopen my $log, $path, O_WRONLY | O_CREAT | O_APPEND, 0600) {
    my $timestamp = strftime('%Y-%m-%dT%H:%M:%S', gmtime $started_at)
      . sprintf('.%03dZ', ($started_at - int($started_at)) * 1000);
    my @words = grep { defined } @$argv;
    my $command_words = join ' ',
      map { /\A[A-Za-z0-9_@%+=:,.\/-]+\z/ ? $_ : "'$_'" } @words[0 .. ($#words > 2 ? 2 : $#words)];
    flock $log, LOCK_EX;
    print $log "$timestamp\t" . sprintf('%.3f', $wait_seconds) . "\t"
      . sprintf('%.3f', $released_at - $acquired_at) . "\t$command_words\n";
    close $log;
  } else {
    print STDERR "remote-exec: cannot append to the slot wait log $path: $!\n";
  }
}

sub build_environment {
  my ($python_version, $temp_dir) = @_;
  my %env = %ENV;
  # bash 5.2 reads /etc/bash.bashrc and ~/.bashrc instead of BASH_ENV for a non-interactive `-c`
  # shell when SSH_CLIENT or SSH2_CLIENT is set and the shell level is below 2. The ssh session
  # sets SHLVL=0, so keeping these variables makes every nested `bash -c` ignore BASH_ENV.
  delete @env{qw(SSH_CLIENT SSH2_CLIENT)};
  $env{CARGO_HOME} = "$base/cargo";
  $env{RUSTUP_HOME} = "$base/rustup";
  $env{UV_CACHE_DIR} = "$base/cache/uv";
  $env{UV_PYTHON_INSTALL_DIR} = "$base/python";
  $env{PATH} = join ':', "$base/node/bin", "$base/bin", "$base/cargo/bin",
    "$ENV{HOME}/.local/go/bin", "$ENV{HOME}/.pixi/bin", "$ENV{HOME}/.local/bin", $env{PATH};
  $env{UV_PYTHON} = $python_version if $python_version ne '';
  $env{TMPDIR} = $temp_dir;
  return \%env;
}

sub run_step {
  my ($env, $cwd, $label, @argv) = @_;
  my $pid = fork();
  fail(2, "cannot fork the $label step: $!") unless defined $pid;
  if ($pid == 0) {
    chdir $cwd or fail(2, "the $label step cannot enter $cwd: $!");
    %ENV = %$env;
    exec @argv or fail(127, "cannot start the $label step: $!");
  }
  while (1) {
    my $waited = waitpid($pid, WNOHANG);
    last if $waited == $pid;
    if ($waited < 0) {
      next if $!{EINTR};
      fail(2, "cannot wait for the $label step: $!");
    }
    if (defined $shutdown_signal || caller_output_channel_closed()) {
      $shutdown_signal = $SIGTERM_NUMBER if !defined $shutdown_signal;
      $descendant_stop_failed ||= stop_descendants();
      return;
    }
    select undef, undef, undef, 0.1;
  }
  if (defined $shutdown_signal) {
    $descendant_stop_failed ||= stop_descendants();
    return;
  }
  my $status = $?;
  fail($status >> 8 ? $status >> 8 : 2, "the $label step failed with status $status")
    if $status != 0;
  return;
}

# Install node_modules once: the lockfile says the tree needs them, and the upload leaves a remote
# node_modules in place because the local one is ignored.
sub install_node_modules {
  my ($env, $tree) = @_;
  return unless -f "$tree/package-lock.json" && !-d "$tree/node_modules";
  run_step($env, $tree, 'node_modules install',
    "$base/node/bin/npm", 'install', '--no-audit', '--no-fund', '--no-progress');
}

sub quoted_list {
  my ($text) = @_;
  return map { s/\A"|"\z//gr } grep { length } split /[,\s]+/, $text;
}

# Read a rust-toolchain.toml's channel, components, and targets. The file's shape is one
# [toolchain] table with quoted string values and a possibly multiline targets array.
sub read_toolchain_file {
  my ($path) = @_;
  open my $file, '<', $path or return;
  my $text = do { local $/; <$file> };
  my ($channel) = $text =~ /^\s*channel\s*=\s*"([^"]+)"/m;
  my ($targets_text) = $text =~ /^\s*targets\s*=\s*\[(.*?)]/ms;
  my ($components_text) = $text =~ /^\s*components\s*=\s*\[(.*?)]/ms;
  return unless defined $channel;
  return ($channel, [quoted_list($targets_text // '')], [quoted_list($components_text // '')]);
}

sub toolchain_is_installed {
  my ($env, $channel) = @_;
  my $list = `RUSTUP_HOME=$env->{RUSTUP_HOME} $base/cargo/bin/rustup toolchain list 2>/dev/null`;
  return $list =~ /^\Q$channel\E\s/m;
}

# A missing channel installs with its minimal profile plus the file's components and targets, the
# same set rustup itself installs when it reads a rust-toolchain.toml. An installed channel gets
# only its listed targets topped up.
sub install_toolchain {
  my ($env, $tree, $cwd) = @_;
  my ($file, $directory);
  for my $candidate ($cwd, $tree) {
    my $path = "$candidate/rust-toolchain.toml";
    if (-f $path) {
      ($file, $directory) = ($path, $candidate);
      last;
    }
  }
  return unless defined $file;
  my ($channel, $targets, $components) = read_toolchain_file($file);
  return unless defined $channel;
  my @add = map { ('--target', $_) } @$targets;
  my @components = map { ('--component', $_) } @$components;
  if (!toolchain_is_installed($env, $channel)) {
    run_step($env, $directory, "rustup toolchain install for $file",
      "$base/cargo/bin/rustup", 'toolchain', 'install', $channel, '--profile', 'minimal',
      @components, @add);
    return;
  }
  return unless @$targets;
  run_step($env, $directory, "rustup target add for $file",
    "$base/cargo/bin/rustup", 'target', 'add', '--toolchain', $channel, @$targets);
}

sub cargo_capable {
  my ($argv) = @_;
  return 1 if $argv->[0] eq 'cargo';
  return $argv->[0] eq 'uv' && @$argv > 1 && ($argv->[1] eq 'run' || $argv->[1] eq 'sync');
}

sub main {
  if (@ARGV && $ARGV[0] eq '--check-tree') {
    fail(2, 'usage: remote-exec --check-tree <caller host> <tree path> <base64 local root>') unless @ARGV == 4;
    shift @ARGV;
    install_signal_handlers();
    $base = resolve_base();
    my $outcome = check_tree_before_sync(@ARGV);
    print "$outcome\n";
    return 0;
  }
  if (@ARGV && $ARGV[0] eq '--sweep-tree') {
    fail(2, 'usage: remote-exec --sweep-tree <caller host> <tree path> <live|gone> <retention days> <dry-run 0|1>')
      unless @ARGV == 6;
    shift @ARGV;
    install_signal_handlers();
    $base = resolve_base();
    my $outcome = sweep_build_tree(@ARGV);
    print "$outcome\n";
    return 0;
  }
  if (@ARGV && $ARGV[0] eq '--claim-tree') {
    fail(2, 'usage: remote-exec --claim-tree <caller host> <tree path> <base64 local root>') unless @ARGV == 4;
    shift @ARGV;
    install_signal_handlers();
    $base = resolve_base();
    claim_build_tree(@ARGV);
    return 0;
  }
  fail(2, 'usage: remote-exec --run-tree <tree path> <base64 command payload>')
    unless @ARGV == 3 && $ARGV[0] eq '--run-tree';
  shift @ARGV;
  my $requested_tree = shift @ARGV;
  my $payload = shift @ARGV;
  install_signal_handlers();
  my $parent_pid = getppid();
  prctl_or_die($PR_SET_CHILD_SUBREAPER, 1);
  prctl_or_die($PR_SET_PDEATHSIG, $SIGTERM_NUMBER);
  # The parent died between the fork and the prctl call: take the signal now.
  $shutdown_signal = $SIGTERM_NUMBER if getppid() != $parent_pid;
  $base = resolve_base();
  my ($tree, $relative_dir, $python_version, $argv) = decode_payload($payload);
  $requested_tree =~ s{\A~}{$ENV{HOME}};
  fail(2, "visible tree path does not match the command payload: $requested_tree != $tree")
    unless $requested_tree eq $tree;
  my ($name) = tree_request('driver', $requested_tree);
  my $activity_lock = lock_tree_activity($name);
  write_last_use($name, now_seconds());
  my $status = run_command_in_tree($tree, $relative_dir, $python_version, $argv);
  write_last_use($name, now_seconds());
  return $status;
}

# Runs the command in its tree and returns its exit status. The caller holds the tree's activity lock for the run.
sub run_command_in_tree {
  my ($tree, $relative_dir, $python_version, $argv) = @_;
  my $cwd = $relative_dir eq '.' ? $tree : "$tree/$relative_dir";
  fail(2, "command directory does not exist: $cwd") unless -d $cwd;
  return 128 + $SIGTERM_NUMBER if caller_output_channel_closed();

  remove_stale_temp_dirs();
  my $temp_dir = create_temp_dir();
  my $env = build_environment($python_version, $temp_dir);

  install_node_modules($env, $tree);
  if (defined $shutdown_signal) {
    rmtree $temp_dir;
    return $descendant_stop_failed ? 2 : 128 + $shutdown_signal;
  }
  install_toolchain($env, $tree, $cwd);
  if (defined $shutdown_signal) {
    rmtree $temp_dir;
    return $descendant_stop_failed ? 2 : 128 + $shutdown_signal;
  }

  my $cargo = "$base/cargo/bin/cargo";
  fail(127, "cargo is missing: $cargo; run setup") if $argv->[0] eq 'cargo' && !-f $cargo;
  my $slot_handle;
  my $slot_started_at = time();
  my ($slot_wait_seconds, $slot_acquired_at) = (0, 0);
  if (cargo_capable($argv)) {
    my $sccache = "$base/bin/sccache";
    fail(127, "sccache is missing: $sccache; run setup") unless -x $sccache;
    $env->{RUSTC_WRAPPER} = $sccache;
    $env->{SCCACHE_DIR} = "$base/cache/sccache";
    $env->{SCCACHE_CACHE_SIZE} = $SCCACHE_CACHE_SIZE;
    $env->{SCCACHE_IDLE_TIMEOUT} = '0';
    $env->{CARGO_BUILD_JOBS} = $CARGO_BUILD_JOBS;
    ($slot_handle, $slot_wait_seconds, $slot_acquired_at) = acquire_slot(monotonic());
    if (!defined $slot_handle) {
      $descendant_stop_failed ||= stop_descendants();
      rmtree $temp_dir;
      return $descendant_stop_failed ? 2 : 128 + ($shutdown_signal // $SIGTERM_NUMBER);
    }
  }

  my @command = @$argv;
  $command[0] = $cargo if $argv->[0] eq 'cargo';
  if ($python_version ne '' && $argv->[0] eq 'uv' && cargo_capable($argv)) {
    @command = (@$argv[0, 1], '--python', $python_version, @$argv[2 .. $#$argv]);
  }
  fail(127, 'required remote tool nice is missing') unless which($env->{PATH}, 'nice');
  @command = ('nice', '-n', '10', @command);
  my $status;
  if (defined $shutdown_signal) {
    $descendant_stop_failed ||= stop_descendants();
    if (defined $slot_handle) {
      release_slot($slot_handle, $slot_started_at, $slot_wait_seconds, $slot_acquired_at, $argv);
    }
    rmtree $temp_dir;
    return $descendant_stop_failed ? 2 : 128 + $shutdown_signal;
  }

  my $command_pid = fork();
  fail(2, "cannot fork the command: $!") unless defined $command_pid;
  if ($command_pid == 0) {
    chdir $cwd or fail(127, "cannot enter the command directory $cwd: $!");
    %ENV = %$env;
    exec @command or fail(127, "cannot start the command: $!");
  }

  while (!defined $status && !defined $shutdown_signal) {
    if (caller_output_channel_closed()) {
      $shutdown_signal = $SIGTERM_NUMBER;
      last;
    }
    reap_adopted_children($command_pid);
    if (waitpid($command_pid, WNOHANG) == $command_pid) {
      $status = $?;
      last;
    }
    select undef, undef, undef, 0.1;
  }
  if (defined $status) {
    my $remaining = descendant_pids($$);
    print STDERR "remote-exec: command left child processes; stopping process ids @$remaining\n"
      if @$remaining;
  }

  # Every exit after the start stops what the command left running, adopted orphans included.
  $descendant_stop_failed ||= stop_descendants();
  if (defined $slot_handle) {
    release_slot($slot_handle, $slot_started_at, $slot_wait_seconds, $slot_acquired_at, $argv);
  }
  rmtree $temp_dir;
  return 2 if $descendant_stop_failed;
  return 128 + $shutdown_signal if !defined $status;
  return $status & 127 ? 128 + ($status & 127) : $status >> 8;
}

exit main();
