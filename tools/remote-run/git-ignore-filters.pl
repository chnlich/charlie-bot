#!/usr/bin/env perl
# Build the rsync filter rules that protect locally ignored paths from the sync's delete.
#
# The sync's `--filter=:- .gitignore` reads the root .gitignore only. git's own ignore machinery
# also reads nested .gitignore files, .git/info/exclude, and the global excludes, so a path the
# root file does not name would still be deleted on the remote side. This program reads the
# remote tree's paths, NUL-separated, on standard input, asks git which of them the local tree
# ignores, and writes one `--filter` argument per protected path, NUL-separated, to standard
# output: an H (hidden from both sides) and a P (protected from delete) rule for each path and
# each of its parent directories.

use strict;
use warnings;

sub fail {
  my ($message) = @_;
  print STDERR "git-ignore-filters: $message\n";
  exit 2;
}

sub spool {
  my ($data) = @_;
  my $path = "/tmp/git-ignore-filters.$$.spool";
  open my $file, '>', $path or fail("cannot write the spool file: $!");
  binmode $file;
  print $file $data;
  close $file or fail("cannot write the spool file: $!");
  return $path;
}

# Run one git command with the NUL file on its standard input and the output in a spool file, so
# neither direction can deadlock on a pipe. git's stderr passes through.
sub capture_nul {
  my ($root, $args, $input_path) = @_;
  my $output_path = "/tmp/git-ignore-filters.$$.out";
  my $redirects = '< ' . quotemeta($input_path) . ' > ' . quotemeta($output_path);
  my @command = ('git', '-C', $root, @$args);
  my $status = system(join(' ', map { quotemeta } @command) . " $redirects");
  if ($? == -1) {
    fail("cannot run $command[0]: $!");
  }
  my $code = $status >> 8;
  # check-ignore exits 1 when some paths are ignored, which is not a failure here.
  if ($code != 0 && $code != 1) {
    unlink $output_path;
    exit $code;
  }
  open my $file, '<', $output_path or fail("cannot read the output spool: $!");
  binmode $file;
  my $output = do { local $/; <$file> };
  close $file;
  unlink $output_path;
  return $output;
}

sub escape {
  my ($path) = @_;
  $path =~ s/([\\*?\[\]])/\\$1/g;
  return $path;
}

sub main {
  my $root = $ARGV[0];
  fail('usage: git-ignore-filters.pl <local tree root>') unless defined $root && -d $root;
  binmode STDIN;
  binmode STDOUT;
  # A tree outside git has no ignore rules and nothing to protect.
  if (system('git -C ' . quotemeta($root) . ' rev-parse --git-dir 2>/dev/null') != 0) {
    return;
  }
  my $remote_paths = do { local $/; my $input = <STDIN>; $input // '' };
  my $input_path = spool($remote_paths);

  my %paths;
  my $checked = capture_nul($root, ['check-ignore', '-z', '--stdin'], $input_path);
  my $listed = capture_nul($root,
    ['ls-files', '--others', '--ignored', '--exclude-standard', '--directory', '-z'], $input_path);
  unlink $input_path;
  $paths{$_} = 1 for grep { length } split /\0/, "$checked\0$listed";

  my %rules;
  for my $path (sort keys %paths) {
    $rules{"H\t$path"} = 1;
    $rules{"P\t$path"} = 1;
    my @parts = split m{/}, ($path =~ s{/+\z}{}r);
    $rules{"P\t" . join('/', @parts[0 .. $_]) . '/'} = 0 for 0 .. $#parts - 1;
  }
  for my $rule (sort keys %rules) {
    my ($action, $path) = split /\t/, $rule, 2;
    print "--filter=$action /" . escape($path) . "\0";
  }
  return;
}

main();
