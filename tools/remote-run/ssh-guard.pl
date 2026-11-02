#!/usr/bin/env perl
# Relay SSH output and stop the connection when Tailscale needs approval.
#
# Every ssh call of the runner goes through this guard: the connectivity check, the rsync transports,
# and the remote command. It prints ssh's stderr as it arrives, and when Tailscale's login link
# appears in it, stops ssh and exits 86 so the caller prints the run's stop text. It sets the parent
# death signal, so a caller killed with SIGKILL takes ssh down with it instead of leaving a remote
# command attached, and it forwards the signals it receives.

use strict;
use warnings;

use IO::Select;
use POSIX qw(uname);
use Time::HiRes qw(time);

my $TAILSCALE_STATUS = 86;
my $TAILSCALE_LINK = qr{(https://login\.tailscale\.com/[^\s"'<>]+)};
my $STOP_AFTER_TERM_SECONDS = 1.0;
my $SELECT_TIMEOUT_SECONDS = 0.1;
# The scan window holds the link even when one chunk ends in the middle of it.
my $SCAN_WINDOW_BYTES = 2048;
my $SCAN_KEEP_BYTES = 256;

my $PR_SET_PDEATHSIG = 1;
my $SIGTERM_NUMBER = 15;
my $SIGKILL_NUMBER = 9;
# The prctl syscall number per machine: the parent death signal needs a raw syscall, and nothing
# else in the runner does.
my %PRCTL_SYSCALL = ('x86_64' => 157, 'aarch64' => 167);

my $received_signal;
my ($ssh_pid, $stop_deadline, $kill_sent);

sub fail {
  my ($message) = @_;
  print STDERR "ssh-guard: $message\n";
  exit 2;
}

sub prctl_or_die {
  my ($option, $value) = @_;
  my ($machine) = (POSIX::uname())[4];
  my $number = $PRCTL_SYSCALL{$machine};
  fail("no prctl syscall number for $machine") unless defined $number;
  syscall($number, $option, $value, 0, 0, 0) == 0 or fail("prctl($option) failed: $!");
}

sub set_parent_death_signal {
  my $parent_pid = getppid();
  prctl_or_die($PR_SET_PDEATHSIG, $SIGTERM_NUMBER);
  # The parent died between the fork and the prctl call: take the signal now.
  kill 'TERM', $$ if getppid() != $parent_pid;
}

sub parse_arguments {
  my $server_alive = 3;
  while (@ARGV && $ARGV[0] =~ /^--/) {
    my $argument = shift @ARGV;
    if ($argument =~ /^--server-alive=(0|3)$/) {
      $server_alive = $1;
    } elsif ($argument eq '--server-alive' && @ARGV && $ARGV[0] =~ /^(0|3)$/) {
      $server_alive = shift @ARGV;
    } else {
      fail("unknown option: $argument");
    }
  }
  my $host = shift @ARGV;
  fail('a host is required') unless defined $host && $host =~ /\S/;
  fail('remote command is empty') unless @ARGV;
  return ($server_alive, $host, join ' ', @ARGV);
}

sub ssh_path {
  for my $directory (split /:/, $ENV{PATH} // '') {
    return "$directory/ssh" if -x "$directory/ssh";
  }
  fail('required local tool ssh is missing');
}

sub forward_signal {
  my ($name) = @_;
  $received_signal = $name;
  if (defined $ssh_pid && kill 0, $ssh_pid) {
    kill 'TERM', $ssh_pid;
    $stop_deadline = time() + $STOP_AFTER_TERM_SECONDS unless defined $stop_deadline;
  }
}

# Read ssh's stderr, print it through, and watch it for the Tailscale login link. The link means
# Tailscale's check mode holds the connection, so the guard stops ssh and the caller reports 86.
sub relay_stderr {
  my ($err_read) = @_;
  my $select = IO::Select->new($err_read);
  my $scan_buffer = '';
  my $tailscale_link;
  while ($select->count) {
    my @ready = $select->can_read($SELECT_TIMEOUT_SECONDS);
    for my $handle (@ready) {
      my $bytes_read = sysread($handle, my $chunk, 65536);
      if (!defined $bytes_read || $bytes_read == 0) {
        $select->remove($handle);
        next;
      }
      print STDERR $chunk;
      if (!defined $tailscale_link && $chunk =~ /$TAILSCALE_LINK/) {
        $tailscale_link = $1;
        print STDERR "Tailscale SSH approval is required at $tailscale_link; stopping this SSH command.\n";
        if (defined $ssh_pid && kill 0, $ssh_pid) {
          kill 'TERM', $ssh_pid;
          $stop_deadline = time() + $STOP_AFTER_TERM_SECONDS;
        }
      }
    }
    if (defined $stop_deadline && !$kill_sent && time() >= $stop_deadline && kill 0, $ssh_pid) {
      print STDERR "ssh-guard: ssh did not stop after SIGTERM; sending SIGKILL\n";
      kill 'KILL', $ssh_pid;
      $kill_sent = 1;
    }
  }
  return $tailscale_link;
}

sub main {
  # A write to stderr fails once the caller is gone; that must not stop the stop of ssh.
  $SIG{PIPE} = 'IGNORE';
  my ($server_alive, $host, $remote_command) = parse_arguments();
  my $ssh = ssh_path();
  for my $name (qw(TERM INT HUP)) {
    $SIG{$name} = sub { forward_signal($name) };
  }
  set_parent_death_signal();

  pipe my $err_read, my $err_write or fail("cannot create the stderr pipe: $!");
  $ssh_pid = fork();
  fail("cannot fork ssh: $!") unless defined $ssh_pid;
  if ($ssh_pid == 0) {
    close $err_read;
    open STDERR, '>&', $err_write or fail("cannot redirect ssh stderr: $!");
    # ssh dies with this guard: a guard that timeout stopped must not leave ssh holding the
    # connection.
    my $guard_pid = getppid();
    prctl_or_die($PR_SET_PDEATHSIG, $SIGKILL_NUMBER);
    kill 'KILL', $$ if getppid() != $guard_pid;
    my @options = ('-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
      '-o', "ServerAliveInterval=$server_alive");
    push @options, ('-o', 'ServerAliveCountMax=2') if $server_alive;
    exec $ssh, @options, $host, $remote_command or fail("cannot start ssh: $!");
  }
  close $err_write;

  my $tailscale_link = relay_stderr($err_read);
  waitpid $ssh_pid, 0;
  my $wait_status = $?;
  my $exit_code = $wait_status >> 8;
  my $signal_number = $wait_status & 127;
  return $TAILSCALE_STATUS if defined $tailscale_link;
  return 128 + $SIGTERM_NUMBER if defined $received_signal && $received_signal eq 'TERM';
  return 130 if defined $received_signal && $received_signal eq 'INT';
  return 129 if defined $received_signal && $received_signal eq 'HUP';
  return 128 + $signal_number if $signal_number;
  return $exit_code;
}

exit main();
