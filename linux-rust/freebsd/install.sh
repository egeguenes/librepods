#!/bin/sh
#
# One-time system setup for LibrePods on FreeBSD. Run as root from linux-rust/
# after `cargo build --release`:
#
#     doas sh freebsd/install.sh
#
# It is safe to run again (e.g. after rebuilding the agent).
#
# What it does:
#  * installs librepods-btagent and its rc.d service, enables it and sdpd,
#    disables hcsecd (the agent replaces it),
#  * loads cuse at boot (the agent runs virtual_oss for A2DP audio),
#  * gives every bonded device a short alias in /etc/bluetooth/hosts
#    (virtual_oss only accepts device paths up to 26 characters).

set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/../target/release/librepods-btagent"
KEYS=/var/db/librepods-bt.keys
HOSTS=/etc/bluetooth/hosts

[ "$(id -u)" -eq 0 ] || { echo "run as root (doas sh $0)"; exit 1; }
[ -x "$BIN" ] || { echo "$BIN not found; run 'cargo build --release' first"; exit 1; }
command -v virtual_oss >/dev/null || echo "warning: virtual_oss not found"
[ -f /usr/local/lib/virtual_oss/voss_bt.so ] || \
    echo "warning: virtual_oss Bluetooth backend missing: pkg install virtual_oss_bluetooth"

echo "==> Installing agent"
service librepods_btagent stop >/dev/null 2>&1 || true
install -m 0755 "$BIN" /usr/local/sbin/librepods-btagent
install -m 0555 "$HERE/librepods_btagent" /usr/local/etc/rc.d/librepods_btagent

echo "==> Services: hcsecd off, sdpd + librepods_btagent on"
service hcsecd onestop >/dev/null 2>&1 || true
sysrc hcsecd_enable=NO sdpd_enable=YES librepods_btagent_enable=YES

echo "==> cuse"
sysrc -f /boot/loader.conf cuse_load=YES
kldload -n cuse

echo "==> Bluetooth host aliases"
if [ -f "$KEYS" ]; then
    n=0
    grep -v '^#' "$KEYS" | awk '{print tolower($1)}' | while read -r addr; do
        [ -n "$addr" ] || continue
        if grep -qi "^[[:space:]]*$addr[[:space:]]" "$HOSTS"; then
            continue
        fi
        n=$((n + 1))
        name="airpods$( [ $n -gt 1 ] && echo $n || true )"
        printf '%s\t%s\n' "$addr" "$name" >> "$HOSTS"
        echo "    $addr -> $name"
    done
fi

echo "==> Starting"
service sdpd restart >/dev/null 2>&1 || service sdpd start
service librepods_btagent restart

cat <<EOF

Done. Pair new AirPods once with:
    doas service librepods_btagent stop
    doas /usr/local/sbin/librepods-btagent --pair AA:BB:CC:DD:EE:FF
    (put the AirPods in pairing mode, connect, then Ctrl+C)
    doas service librepods_btagent start
    doas sh $0        # adds the hosts alias for the new device
Then just run librepods. Agent log: /var/log/librepods-btagent.log
EOF
