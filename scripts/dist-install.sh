#!/bin/sh
# Installs avb-endpointd's release build into a tree, as the packages lay
# it out:
#
#   scripts/dist-install.sh TARGET_DIR DESTDIR PREFIX
#
# TARGET_DIR holds avb-endpointd, built with its daemon feature, and
# avb-netraw (target/release); DESTDIR is the root to install under, and
# PREFIX is where the daemon goes (/usr). avb-netraw goes in
# /usr/lib/avb-endpointd, where the daemon and the apps look for it, and
# packages then grant it CAP_NET_RAW; nothing else holds a capability.
set -eu
if [ $# -ne 3 ]; then
    echo "usage: $0 TARGET_DIR DESTDIR PREFIX" >&2
    exit 2
fi
target=$1
root=$2
prefix=$3
here=$(dirname "$0")/..
units="$here/packaging/linux/units"
install -D -m 0755 "$target/avb-endpointd" "$root$prefix/bin/avb-endpointd"
install -D -m 0755 "$target/avb-netraw" "$root/usr/lib/avb-endpointd/avb-netraw"
# The person's unit, one for each interface, run from where the package
# puts the daemon.
install -d "$root/usr/lib/systemd/user"
sed "s|/usr/bin/avb-endpointd|$prefix/bin/avb-endpointd|" "$units/avb-endpointd@.service" \
    >"$root/usr/lib/systemd/user/avb-endpointd@.service"
chmod 0644 "$root/usr/lib/systemd/user/avb-endpointd@.service"
# The link units it runs, and the rule that lets the person at the
# computer start them.
for unit in avb-link@.service avb-link-reset@.service; do
    install -D -m 0644 "$units/$unit" "$root/usr/lib/systemd/system/$unit"
done
install -D -m 0644 "$units/50-avb-link.rules" \
    "$root/usr/share/polkit-1/rules.d/50-avb-link.rules"
install -D -m 0644 "$here/docs/endpointd.md" "$root/usr/share/doc/avb-endpointd/endpointd.md"
install -D -m 0644 "$here/LICENSE-MIT" "$root/usr/share/licenses/avb-endpointd/LICENSE-MIT"
install -D -m 0644 "$here/LICENSE-APACHE" "$root/usr/share/licenses/avb-endpointd/LICENSE-APACHE"
