#!/usr/bin/env bash
# Times git and sprig on the same generated history at several repo sizes and
# prints CSV (files,command,tool,ms) to stdout. Charts: bench/chart.py.
#
#   cargo build --release && bench/run.sh [sizes...] > bench/results.csv
#   (default sizes: 1000 3000 10000 30000; SPRIG=path overrides the binary)
#
# Each size gets COMMITS commits of small edits (3 files each), made with git
# and imported into sprig, then `sprig gc`. HOME points at a scratch dir so
# neither tool reads your config (sprig doesn't sign; git has no hooks).
set -euo pipefail

SIZES=${*:-1000 3000 10000 30000}
COMMITS=${COMMITS:-100}
REPS=${REPS:-20}
SPRIG=$(realpath "${SPRIG:-target/release/sprig}")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/sprig-bench.XXXXXX")
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT
export HOME=$WORK/home GIT_CONFIG_NOSYSTEM=1 SPRIG_AUTHOR="bench <bench@example.com>"
mkdir -p "$HOME"
git config --global user.name bench
git config --global user.email bench@example.com
git config --global init.defaultBranch master
# no background gc: it races the commit loop ("Could not read <sha>"); gc'd once below
git config --global gc.auto 0
git config --global maintenance.auto false

# avg <reps> <cmd...>: mean wall time in ms (3 decimals) over reps runs.
avg() {
	local reps=$1 s r
	shift
	s=$(date +%s%N)
	for ((r = 0; r < reps; r++)); do "$@" >/dev/null 2>&1 || { echo "failed in $PWD: $*" >&2; exit 1; }; done
	echo "scale=3; ($(date +%s%N) - $s) / ($reps * 1000000)" | bc
}
gcwait() { while [ -e .sprig/gc.lock ]; do sleep 0.1; done; } # auto gc runs detached
row() { echo "$FILES,$1,$2,$3"; }
both() { # both <name> <args...>: same arguments to git and sprig
	local name=$1
	shift
	(cd "$G" && row "$name" git "$(avg "$REPS" git "$@")")
	(cd "$S" && row "$name" sprig "$(avg "$REPS" "$SPRIG" "$@")")
}

echo "files,command,tool,ms"
for FILES in $SIZES; do
	G=$WORK/$FILES/g S=$WORK/$FILES/s
	mkdir -p "$G" "$S"
	dirs=$(((FILES + 99) / 100))
	(
		cd "$G"
		git init -q
		for ((f = 0; f < FILES; f++)); do
			d=d$((f / 100))
			[ -d $d ] || mkdir $d
			printf 'package %s\n\nfunc F%d() int {\n\treturn %d\n}\n' $d $f $f >$d/f$f.go
		done
		git add -A && git commit -qm init
		for ((c = 1; c < COMMITS; c++)); do
			for k in 0 1 2; do
				f=$(((c * 7 + k * 31) % FILES))
				echo "// edit $c" >>d$((f / 100))/f$f.go
			done
			git commit -qam "edit $c"
		done
		git branch other HEAD~50
		git gc -q
	)
	(
		cd "$S"
		"$SPRIG" init >/dev/null
		"$SPRIG" import-git "$G" >/dev/null
		"$SPRIG" checkout -f master >/dev/null
		gcwait
		"$SPRIG" gc >/dev/null
	)
	sleep 1.2 # past the racy-clean window, so both index/stat caches are trusted
	(cd "$G" && git status >/dev/null)
	(cd "$S" && "$SPRIG" status >/dev/null)
	echo "size $FILES ready" >&2

	both "startup" version
	both "status" status
	both "log -n 100" log -n 100
	both "show HEAD~10" show HEAD~10
	both "diff HEAD~50 HEAD" diff HEAD~50 HEAD

	mod=d0/f0.go
	(cd "$G" && echo x >>$mod) && (cd "$S" && echo x >>$mod)
	both "status (1 modified)" status
	both "diff (working tree)" diff
	(cd "$G" && git checkout -q -- $mod) && (cd "$S" && "$SPRIG" checkout -f master >/dev/null)

	(cd "$S" && exec "$SPRIG" watch >/dev/null 2>&1) &
	wp=$!
	sleep 1
	(cd "$S" && "$SPRIG" status >/dev/null)
	(cd "$S" && row "status (sprig watch)" sprig "$(avg "$REPS" "$SPRIG" status)")
	(cd "$G" && row "status (sprig watch)" git "$(avg "$REPS" git status)")
	kill $wp && wait $wp 2>/dev/null || true

	n=$((REPS / 2))
	switch() { local k; for ((k = 0; k < n; k++)); do "$@" other >/dev/null && "$@" master >/dev/null; done; }
	(cd "$G" && row "checkout (50 back)" git "$(echo "scale=3; $(avg 1 switch git checkout -q) / ($n * 2)" | bc)")
	(cd "$S" && row "checkout (50 back)" sprig "$(echo "scale=3; $(avg 1 switch "$SPRIG" checkout) / ($n * 2)" | bc)")

	commits() { local k; for ((k = 0; k < n; k++)); do echo "// c$k" >>d0/f1.go && "$@" >/dev/null; done; }
	(cd "$G" && row "commit (1 file)" git "$(echo "scale=3; $(avg 1 commits git commit -qam c) / $n" | bc)")
	(cd "$S" && row "commit (1 file)" sprig "$(echo "scale=3; $(avg 1 commits "$SPRIG" commit -m c) / $n" | bc)")

	gs=$(cd "$G" && git gc -q && du -sk .git | cut -f1)
	ss=$(cd "$S" && gcwait && "$SPRIG" gc >/dev/null && du -sk .sprig | cut -f1)
	row "storage (KB)" git "$gs"
	row "storage (KB)" sprig "$ss"
	(cd "$S" && gcwait)
	rm -rf "${WORK:?}/$FILES"
done
