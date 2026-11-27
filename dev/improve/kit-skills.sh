# The skills of a kit revision, for a merge outside a round. A round reads its skills from the copy under
# $HOME/improve/runner/bin/<revision>/skills, and its event log names those paths. The runner's checkout,
# $HOME/improve/runner/charlie-bot (remote-runner.sh's $RUNNER_ROOT/charlie-bot), holds the commit of every kit
# revision the runner built, so its git content holds each revision's skills tree whether or not the runner still keeps
# that revision's directory.
#
# kit_skills_dir <revision>: when the checkout's git content holds <revision>'s skills tree, print the skills directory
# that a round at <revision> names, and return 0. Otherwise print nothing and return 1; the caller refuses and names
# <revision>. Only <revision> is read, so a missing tree is never answered from another revision.
#
# Sourced by merge-outside-round.sh and github-post.sh.
kit_skills_dir() {
  local revision=$1 checkout="$HOME/improve/runner/charlie-bot"
  git -C "$checkout" rev-parse --verify --quiet "$revision:skills" > /dev/null 2>&1 || return 1
  printf '%s\n' "$HOME/improve/runner/bin/$revision/skills"
}
