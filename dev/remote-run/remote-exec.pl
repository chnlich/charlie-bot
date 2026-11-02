#!/usr/bin/env perl
# Run one synced-tree command with a build slot, a short private TMPDIR, and descendant cleanup.
#
# The runner sends this program one base64 payload: the synced tree root, the directory to run in,
# the CPython version the local project pins, and the command. On the build host it prepares the
# command's world and then runs it. Preparation, in order:
#
# - a short TMPDIR under /tmp, so tmux socket paths stay under the 108-byte sun_path limit;
# - node_modules, when the tree has a package lockfile and no node_modules;
# - a rust-toolchain.toml's channel with its components and targets, and its targets otherwise;
# - the environment: the runner's cargo, uv, and node, then ~/.local/go/bin and ~/.pixi/bin;
# - a build slot with sccache for cargo and cargo-capable uv commands.
#
# It makes itself a child subreaper, so tmux servers and other daemons the command starts are
# adopted instead of reparented away, and it stops every descendant when the command ends. A shell
# cannot do either: the subreaper call is the prctl syscall, which is why this program is perl.

use strict;
use warnings;

use Fcntl qw(:flock O_RDWR O_WRONLY O_CREAT O_APPEND);
use File::Path qw(rmtree);
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
# <pid>-<start ticks> stays under 17 bytes for pid_max 327680 and a 10-digit tick count, so
# /tmp/<prefix><pid>-<start> stays under the 25-byte budget the tmux socket path leaves for TMPDIR.
my $TEMP_PREFIX = 'rr-';

my $PR_SET_PDEATHSIG = 1;
my $PR_SET_CHILD_SUBREAPER = 36;
my $SIGTERM_NUMBER = 15;
my %PRCTL_SYSCALL = ('x86_64' => 157, 'aarch64' => 167);

my $shutdown_signal;    # The number of the first signal received, once received.
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
  opendir my $tasks, "/proc/$pid/task" or return [];
  my %children;
  for my $task (grep !/\A\.\.?\z/, readdir $tasks) {
    if (open my $children_file, '<', "/proc/$pid/task/$task/children") {
      my $line = <$children_file> // '';
      $children{$_} = 1 for split ' ', $line;
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
    return unless @$pids;
    if (time() >= $deadline) {
      my @lines = map {
        "remote-exec: process $_ outlived the ${STOP_WAIT_SECONDS}s stop wait: " . command_line($_)
      } @$pids;
      print STDERR "$_\n" for @lines;
      return;
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
  my $stat = <$file> // return;
  # The fields after the parenthesised command name start at field 3, so field 22 is index 19.
  my @fields = split ' ', ($stat =~ s/.*\)//r);
  return $fields[19];
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
  for my $entry (glob '/tmp/' . "$TEMP_PREFIX*") {
    next unless -d $entry && -o $entry;
    my ($owner_pid, $owner_start) = $entry =~ m{\A/tmp/\Q$TEMP_PREFIX\E([0-9]+)-([0-9]+)\z} or next;
    my $start_ticks = process_start_ticks($owner_pid);
    next if defined $start_ticks && $start_ticks == $owner_start;
    next if temp_dir_in_use($entry);
    rmtree $entry;    # A concurrent remote-exec may have removed it first.
  }
}

sub create_temp_dir {
  my $path = '/tmp/' . $TEMP_PREFIX . $$ . '-' . process_start_ticks($$);
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
  waitpid $pid, 0;
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
  fail(2, 'usage: remote-exec <base64 command payload>') unless @ARGV == 1;
  install_signal_handlers();
  my $parent_pid = getppid();
  prctl_or_die($PR_SET_CHILD_SUBREAPER, 1);
  prctl_or_die($PR_SET_PDEATHSIG, $SIGTERM_NUMBER);
  # The parent died between the fork and the prctl call: take the signal now.
  $shutdown_signal = $SIGTERM_NUMBER if getppid() != $parent_pid;
  $base = resolve_base();
  my ($tree, $relative_dir, $python_version, $argv) = decode_payload($ARGV[0]);

  my $cwd = $relative_dir eq '.' ? $tree : "$tree/$relative_dir";
  fail(2, "command directory does not exist: $cwd") unless -d $cwd;

  remove_stale_temp_dirs();
  my $temp_dir = create_temp_dir();
  my $env = build_environment($python_version, $temp_dir);

  install_node_modules($env, $tree);
  install_toolchain($env, $tree, $cwd);

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
      return 128 + ($shutdown_signal // $SIGTERM_NUMBER);
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
    return 128 + $shutdown_signal;
  }

  my $command_pid = fork();
  fail(2, "cannot fork the command: $!") unless defined $command_pid;
  if ($command_pid == 0) {
    chdir $cwd or fail(127, "cannot enter the command directory $cwd: $!");
    %ENV = %$env;
    exec @command or fail(127, "cannot start the command: $!");
  }

  while (!defined $status && !defined $shutdown_signal) {
    reap_adopted_children($command_pid);
    if (waitpid($command_pid, WNOHANG) == $command_pid) {
      $status = $?;
      last;
    }
    select undef, undef, undef, 0.1;
  }
  if (!defined $status) {
    kill 'TERM', $command_pid if kill 0, $command_pid;
    waitpid $command_pid, 0;
  } else {
    my $remaining = descendant_pids($$);
    print STDERR "remote-exec: command left child processes; stopping process ids @$remaining\n"
      if @$remaining;
  }

  # Every exit after the start stops what the command left running, adopted orphans included.
  stop_descendants();
  if (defined $slot_handle) {
    release_slot($slot_handle, $slot_started_at, $slot_wait_seconds, $slot_acquired_at, $argv);
  }
  rmtree $temp_dir;
  return 128 + $shutdown_signal if !defined $status;
  return $status & 127 ? 128 + ($status & 127) : $status >> 8;
}

exit main();
