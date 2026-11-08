#!/usr/bin/env bash
# Checks a release before it is built (RELEASING.md); run it locally before
# pushing the tag:  .github/scripts/release-check.sh 0.2.0 [notes.md]
#  - the workspace version is <version> and every crate inherits it;
#  - CHANGELOG.md has a section "# Release v<version>" with one "## <component>"
#    subsection per component;
# and writes that section (the GitHub release notes) to [notes.md].
set -euo pipefail
cd "$(dirname "$0")/../.."
version="${1:?usage: release-check.sh <version> [notes.md]}"
notes="${2:-/dev/null}"
components=(scheduler runner dbwriter backend endpoint sourcesync regencyctl frontend all-in-one)
fail=0

declared=$(awk '/^\[workspace.package\]/{on=1; next} /^\[/{on=0} on && /^version *=/{gsub(/.*= *"|".*/, ""); print; exit}' Cargo.toml)
if [ "$declared" != "$version" ]; then
    echo "Cargo.toml [workspace.package] has version '$declared', not $version"; fail=1
fi
for manifest in */Cargo.toml; do
    if ! grep -q '^version.workspace = true' "$manifest"; then
        echo "$manifest does not use version.workspace = true"; fail=1
    fi
done
if ! grep -q "^name = \"regencylib\"" Cargo.lock || ! awk -v v="$version" '
        /^name = "(backend|dbwriter|endpoint|frontend|regencylib|runner|scheduler|sourcesync|regencyctl|regency-aio)"$/ {n=1; next}
        n && /^version/ {if ($3 != "\"" v "\"") bad=1; n=0}
        END {exit bad}' Cargo.lock; then
    echo "Cargo.lock is not at $version: run cargo update -w"; fail=1
fi

section=$(awk -v v="# Release v${version}" '
    $0 == v {on=1; next}
    on && /^# / {exit}
    on {print}' CHANGELOG.md)
if [ -z "$(echo "$section" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no section '# Release v${version}'"; fail=1
else
    for c in "${components[@]}"; do
        if ! grep -qx "## $c" <<<"$section"; then
            echo "CHANGELOG.md, release v${version}: no '## $c' subsection (write 'No change.' if nothing changed)"; fail=1
        fi
    done
fi

[ "$fail" = 0 ] || exit 1
printf '%s\n' "$section" > "$notes"
echo "release v${version}: OK"
