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
#
# Tag convention (enforced below, not just recorded): release tags
# are `v<semver>` — digits, dots, no hyphens. A hyphen-bearing tag
# (a pre-release like v0.1.0-rc1) is REFUSED: its only possible
# sanitization is a '+' suffix that sorts AFTER the plain release
# (v0.1.0-rc1 -> 0.1.0+rc1 > 0.1.0), which is the wrong order for
# a pre-release, so the script fails loudly instead of shipping a
# misordered version. Pre-release tags are outside this pipeline's
# convention; if one is ever needed, the ordering mapping must be
# designed first ('~' pre-release semantics), not improvised here.
# Examples:
#   v0.1.0                     -> 0.1.0
#   v0.1.0-dirty               -> 0.1.0.dirty
#   v0.1.0-12-g52ad001         -> 0.1.0+12.g52ad001
#   v0.1.0-12-g52ad001-dirty   -> 0.1.0+12.g52ad001.dirty
#   d411c68 (no tags yet)      -> 0.1.0~gd411c68
#   v0.1.0-rc1                 -> REFUSED (hyphen-bearing tag)
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
    # A tag is reachable: describe is TAG or TAG-N-gHASH. Recover
    # the bare tag name by stripping the distance suffix, then
    # enforce the release-tag convention on it.
    tag=$(git describe --tags | sed 's/-[0-9][0-9]*-g[0-9a-f][0-9a-f]*$//')
    case "$tag" in
        *-*)
            echo "package-version.sh: refusing tag '$tag':" >&2
            echo "release tags are v<semver> with no hyphens. A hyphen-bearing" >&2
            echo "tag (a pre-release like v0.1.0-rc1) would sanitize to a '+'" >&2
            echo "suffix sorting AFTER the plain release (v0.1.0-rc1 ->" >&2
            echo "0.1.0+rc1 > 0.1.0) — the wrong order for a pre-release." >&2
            exit 1
            ;;
    esac
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
