#!/usr/bin/env bash
# Runs the benchmark against every tutorial part and compares them.
#
#   bench/run_all.sh                          every part-* tag, 1M keys
#   bench/run_all.sh part-02-sstables HEAD    just these refs
#   bench/run_all.sh -- --keys 100000         pass options through to boostdb-bench
#
# Each ref is checked out into a temporary git worktree, this directory's harness is copied
# into it (so every part runs the same harness, whatever its own bench/ holds), and the
# harness is built in release mode against that part's code. Results go to
# bench/results/<ref>.json, and bench/results/summary.md compares every JSON file there.
# Set RESULTS_DIR to write somewhere else.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
results="${RESULTS_DIR:-$here/results}"

refs=()
bench_args=()
while (($#)); do
    case "$1" in
        --) shift; bench_args=("$@"); break ;;
        -h | --help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) refs+=("$1"); shift ;;
    esac
done
if ((${#refs[@]} == 0)); then
    while IFS= read -r tag; do refs+=("$tag"); done \
        < <(git -C "$repo" tag --list 'part-*' --sort=version:refname)
fi
if ((${#refs[@]} == 0)); then
    echo "run_all.sh: no part-* tags found" >&2
    exit 1
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/boostdb-bench.XXXXXX")"
cleanup() {
    for wt in "$tmp"/worktree-*; do
        if [ -d "$wt" ]; then git -C "$repo" worktree remove --force "$wt" || true; fi
    done
    rm -rf "$tmp"
}
trap cleanup EXIT

mkdir -p "$results"
for ref in "${refs[@]}"; do
    commit="$(git -C "$repo" rev-parse --verify "$ref^{commit}")"
    name="${ref//\//-}"
    wt="$tmp/worktree-$name"
    echo "==> $ref ($commit)" >&2

    git -C "$repo" worktree add --detach --quiet "$wt" "$commit"
    rm -rf "$wt/bench"
    mkdir "$wt/bench"
    cp -R "$here/Cargo.toml" "$here/build.rs" "$here/src" "$wt/bench/"
    if [ -f "$here/Cargo.lock" ]; then cp "$here/Cargo.lock" "$wt/bench/"; fi

    # Parts from before Db::stats() report null for the internal counters.
    features=()
    if ! grep -rq 'pub fn stats(' "$wt/src"; then features=(--no-default-features); fi

    target="$here/target/run_all/$name"
    CARGO_TARGET_DIR="$target" cargo build --release --quiet \
        --manifest-path "$wt/bench/Cargo.toml" ${features[@]+"${features[@]}"}

    "$target/release/boostdb-bench" run \
        --tag "$ref" \
        --commit "$commit" \
        --subject "$(git -C "$repo" log -1 --format=%s "$commit")" \
        --run-at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --work-dir "$tmp/data-$name" \
        --out "$results/$name.json" \
        ${bench_args[@]+"${bench_args[@]}"}
    echo "    wrote $results/$name.json" >&2

    git -C "$repo" worktree remove --force "$wt"
done

cargo run --release --quiet --manifest-path "$here/Cargo.toml" -- summary "$results" \
    > "$results/summary.md"
echo "==> wrote $results/summary.md" >&2
