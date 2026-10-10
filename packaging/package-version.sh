#!/bin/sh
# The package version for a build (P7-C, ADR-0009's version
# discipline): the same `git describe --tags --always --dirty` the
# P7-A build.rs compiles into the binaries, sanitized to the syntax
# both dpkg and rpm accept in their Version fields.
#
# Neither format allows '-'. The convention, chosen so version
# ordering matches describe reachability:
#   - the leading 'v' of a tag is dropped (v0.1.0 -> 0.1.0);
#   - everything after the tag ('-12-gHASH') becomes a '+'
#     suffix: '+' sorts AFTER the bare tag version in dpkg and
#     rpm alike, exactly as a post-tag commit follows the tag;
#   - the remaining '-' inside that suffix become '.';
#   - a '-dirty' tree appends '.dirty';
#   - a TAGLESS describe (the '--always' bare hash — the repo
#     carries no reachable tag) cannot be a version at all (dpkg
#     requires a leading digit), so it maps onto the crate's own
#     version as a pre-tag snapshot: 0.1.0~gHASH ('~' sorts
#     BEFORE the tag in both dpkg and rpm, which is what a build
#     from an untagged tree is).
# Examples:
#   v0.1.0                     -> 0.1.0
#   v0.1.0-dirty               -> 0.1.0.dirty
#   v0.1.0-12-g52ad001         -> 0.1.0+12.g52ad001
#   v0.1.0-12-g52ad001-dirty   -> 0.1.0+12.g52ad001.dirty
#   d411c68 (no tags yet)      -> 0.1.0~gd411c68
# The binary's own `--version` prints the RAW describe; the
# package carries this sanitized form of the same describe, and the
# release workflow asserts the two agree before packaging.
#
# Usage: packaging/package-version.sh   (from the repository root)

set -eu

describe=$(git describe --tags --always --dirty)

dirty=
case "$describe" in
    *-dirty) dirty=".dirty"; describe=${describe%-dirty} ;;
esac

if git describe --tags >/dev/null 2>&1; then
    # A tag is reachable: describe is TAG or TAG-N-gHASH.
    case "$describe" in
        v*) version=${describe#v} ;;
        *) version=$describe ;;
    esac
    case "$version" in
        *-*)
            base=${version%%-*}
            rest=${version#*-}
            version="$base+$(printf '%s' "$rest" | tr '-' '.')"
            ;;
    esac
else
    # No tag reachable: describe is the bare abbreviated commit hash.
    # The crate version (the first `version = ` in the workspace
    # manifest's [workspace.package]) is the base; fail loudly if the
    # manifest shape ever changes.
    crate_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
    if [ -z "$crate_version" ]; then
        echo "package-version.sh: could not read the workspace version from Cargo.toml" >&2
        exit 1
    fi
    version="$crate_version~g$describe"
fi

printf '%s\n' "$version$dirty"
