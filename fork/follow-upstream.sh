#!/usr/bin/env bash
# Rebase this fork's patch series onto a new upstream release and run every
# check that can be run without a live tunnel.
#
#   fork/follow-upstream.sh                       # onto the tracked release branch
#   fork/follow-upstream.sh v2.6.5                # onto a tag
#   fork/follow-upstream.sh upstream/releases/v2.6.5
#
# It stops at the first failure and never pushes. Publishing the result is a
# separate, deliberate step — see fork/SOP.md.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die()  { printf '\n\033[31m!! %s\033[0m\n' "$*" >&2; exit 1; }
note() { printf '   %s\n' "$*"; }

run() { note "\$ $*"; "$@" || die "failed: $*"; }

# ------------------------------------------------------------ preconditions

step "Preconditions"
[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash first"
[[ -d .git/rebase-merge || -d .git/rebase-apply ]] && die "a rebase is already in progress"
git config --get rerere.enabled >/dev/null || note "hint: git config rerere.enabled true"

branch=$(git rev-parse --abbrev-ref HEAD)
before=$(git rev-parse HEAD)
note "branch $branch at $before"

step "Fetching upstream"
run git fetch upstream --tags --prune

target="${1:-}"
if [[ -z "$target" ]]; then
  target=$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null)
  target="upstream/${target#origin/}"
fi
git rev-parse --verify --quiet "$target^{commit}" >/dev/null ||
  die "unknown target ref: $target"
note "target $target ($(git rev-parse --short "$target"))"

base=$(git merge-base "$target" HEAD)
if [[ "$base" == "$(git rev-parse "$target")" ]]; then
  note "already on top of $target — nothing to rebase"
else
  step "Patch series to replay"
  git log --oneline "$base..HEAD" | cat

  step "Rebasing onto $target"
  if ! git rebase "$target"; then
    cat <<'EOF'

Rebase stopped on a conflict. Resolve it, `git add` the files and
`git rebase --continue`, then re-run this script to pick up the checks.
`git rebase --abort` puts everything back.

Conflicts are expected only in the files listed in fork/SOP.md. A conflict
anywhere else means upstream moved something the patch depends on — read the
upstream change before resolving.
EOF
    exit 1
  fi
fi

# --------------------------------------------------------------- inspection

step "Patch series after rebase (review this)"
git range-diff "$base..$before" "$target..HEAD" | cat || true
cat <<'EOF'

   Nothing but context-line shifts should appear above. Any changed hunk is a
   place where upstream code moved under the patch — check it made sense.
EOF

step "Files this fork touches in upstream code"
git diff --stat "$(git merge-base "$target" HEAD)" -- \
  easytier/src/common easytier/src/socket easytier/src/instance \
  easytier-core/src | cat

# ------------------------------------------------------------------- checks

step "cargo fmt --check"
run cargo fmt -p easytier -p easytier-core --check

step "cargo build (default features)"
run cargo build -p easytier

step "cargo build (--features macos-ne, the non-macOS hook path)"
run cargo check -p easytier --features macos-ne

step "Invariant guards"
run cargo test -p easytier --test underlay_sockets
run cargo test -p easytier --lib arch::macos

step "Full unit test suite"
note "expect the pre-existing environment failures listed in fork/SOP.md"
cargo test -p easytier --lib 2>&1 | tail -25
cargo test -p easytier-core --lib 2>&1 | tail -5

cat <<EOF

$(printf '\033[1m==> Static checks done.\033[0m')

Not verified yet: the tunnel itself. Nothing about routing, socket scoping or
egress is covered above.

Next, in order:
  1. fork/mini.sh gate                <- builds, ships to the test box, runs the
                                         full-tunnel gate there; must pass before
                                         anything is pinned
  2. (no test box at hand? bring the tunnel up locally and run
     fork/verify-fulltunnel.sh — but read fork/SOP.md §4 on why that is weaker)
  3. git tag mac-base-\$(date +%Y%m%d) && git push origin --tags
  4. git push --force-with-lease origin $branch
  5. bump the submodule in easytier-mac to the new commit

Steps 3-5 only after step 2 is green. See fork/SOP.md.
EOF
