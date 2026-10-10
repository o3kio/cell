#!/bin/sh
# The package install smoke (P7-C, ADR-0009): run as root inside a
# distro container with the built .deb/.rpm in the current directory.
#
#   packaging/smoke.sh <describe> <package-version>
#
#   <describe>        the raw `git describe --tags --always --dirty`
#                     the binaries' --version must print (the P7-A
#                     stamp discipline, asserted end-to-end)
#   <package-version> the sanitized form of the same describe the
#                     dpkg/rpm Version field must carry
#                     (packaging/package-version.sh)
#
# What this smoke covers: package installation with dependency
# resolution, both binaries' version stamps, --check-config against
# the SHIPPED example configs at their installed paths, the shipped
# file layout, the hard lvm2 dependency, and systemd-analyze verify
# of the installed unit files.
#
# What this smoke does NOT cover (recorded, not skipped): unit START —
# it needs systemd as PID 1, which a CI container does not run; that
# path was verified on a real systemd in P7-B's sandbox smoke and is
# not re-proven here. Also not covered: package upgrade/downgrade
# transitions, and any architecture other than the one built.

set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "smoke: must run as root (package install)" >&2
    exit 1
fi

describe=${1:?usage: smoke.sh <describe> <package-version>}
package_version=${2:?usage: smoke.sh <describe> <package-version>}

if [ -f /etc/debian_version ]; then
    distro=debian
elif [ -f /etc/fedora-release ]; then
    distro=fedora
else
    echo "smoke: unsupported container distro (need debian or fedora)" >&2
    exit 1
fi

# 1. Install the package with dependency resolution. The hard lvm2
#    dependency must resolve and install from the distro repository;
#    the weak drbd-utils recommendation installs if the distro
#    carries it and is honestly absent otherwise.
if [ "$distro" = debian ]; then
    apt-get update
    apt-get install -y ./*.deb
    installed_version=$(dpkg-query -W -f '${Version}' volvisor)
    dpkg -s lvm2 >/dev/null
else
    # The official fedora container images ship `tsflags=nodocs` in
    # /etc/dnf/dnf.conf, so doc-flagged payload files would be
    # silently skipped. The smoke is here to verify what the package
    # carries, so it drops that base-image exclusion (a real host
    # without nodocs installs the docs; a host that keeps nodocs
    # excludes them by its own policy — both are correct, and the
    # package's payload is what the smoke asserts).
    sed -i '/^tsflags=nodocs/d' /etc/dnf/dnf.conf
    dnf install -y ./*.rpm
    installed_version=$(rpm -q --qf '%{VERSION}' volvisor)
    rpm -q lvm2 >/dev/null
fi

# 2. The version discipline, end to end: package and binaries carry
#    one version.
test "$installed_version" = "$package_version"
test "$(volvisord --version)" = "volvisord $describe"
test "$(volvisor-witnessd --version)" = "volvisor-witnessd $describe"

# 3. The shipped example configs pass --check-config at their
#    installed paths — the same surface the units' ExecStartPre runs.
volvisord --config /etc/volvisor/volvisor.toml --check-config
volvisor-witnessd --config /etc/volvisor/witnessd.toml --check-config

# 4. The shipped layout: units, configs, docs.
test -f /usr/lib/systemd/system/volvisord.service
test -f /usr/lib/systemd/system/volvisor-witnessd.service
test -f /etc/volvisor/volvisor.toml
test -f /etc/volvisor/witnessd.toml
test -f /usr/share/doc/volvisor/configuration.md
test -f /usr/share/doc/volvisor/packaging.md

# 5. Unit verification against the installed paths and the installed
#    binaries — the deepest unit check available without systemd as
#    PID 1 (see the note above).
if [ "$distro" = debian ]; then
    apt-get install -y systemd
else
    dnf install -y systemd
fi
systemd-analyze verify /usr/lib/systemd/system/volvisord.service /usr/lib/systemd/system/volvisor-witnessd.service

echo "smoke: PASS ($distro, package $package_version, $describe)"
