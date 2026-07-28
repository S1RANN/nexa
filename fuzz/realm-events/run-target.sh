#!/bin/sh
set -eu

target=${1:?usage: ./run-target.sh TARGET}

case "$target" in
  bytecode_decode|verifier|source_map_decoder) max_len=1048576 ;;
  migration_fixture_parser) max_len=65536 ;;
  release_intrusive_list|realm_event_sequence) max_len=128 ;;
  register_planner|enum_match_lowering|try_operator_lowering|completion_routing|completion_ticket_terminal_race) max_len=64 ;;
  stateful_registry|migration_arena) max_len=256 ;;
  *) echo "unknown fuzz target: $target" >&2; exit 2 ;;
esac

mkdir -p "artifacts/$target"
status=0
cargo fuzz run "$target" "corpus/$target" -- \
  "-max_len=$max_len" \
  -timeout=10 \
  -rss_limit_mb=2048 \
  "-artifact_prefix=artifacts/$target/" || status=$?

for crash in "artifacts/$target"/crash-*; do
  test -e "$crash" || continue
  name=${crash##*/}
  cargo fuzz tmin "$target" "$crash" -- \
    "-exact_artifact_path=artifacts/$target/minimized-$name" \
    "-max_len=$max_len" \
    -timeout=10 \
    -rss_limit_mb=2048 || true
done

exit "$status"
