#!/usr/bin/env sh
set -eu

[ "$#" -eq 2 ] || {
  printf 'usage: %s <wakezilla-tray> <artifact-dir>\n' "$0" >&2
  exit 64
}

tray_binary=$1
artifact_dir=$2

for command in Xvfb dbus-monitor gdbus import sed xfce4-panel xfconf-query; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 1
  }
done

[ -x "$tray_binary" ] || {
  printf 'Wakezilla tray binary is not executable: %s\n' "$tray_binary" >&2
  exit 1
}

mkdir -p "$artifact_dir"
artifact_dir=$(CDPATH= cd -- "$artifact_dir" && pwd -P)
session_root=$(mktemp -d)
export XDG_CONFIG_HOME="$session_root/config"
export XDG_CACHE_HOME="$session_root/cache"
export XDG_RUNTIME_DIR="$session_root/runtime"
mkdir -p "$XDG_CONFIG_HOME/xfce4/xfconf/xfce-perchannel-xml" \
  "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

cleanup() {
  [ -z "${monitor_pid:-}" ] || kill "$monitor_pid" >/dev/null 2>&1 || true
  [ -z "${tray_pid:-}" ] || kill "$tray_pid" >/dev/null 2>&1 || true
  [ -z "${panel_pid:-}" ] || kill "$panel_pid" >/dev/null 2>&1 || true
  [ -z "${xvfb_pid:-}" ] || kill "$xvfb_pid" >/dev/null 2>&1 || true
  [ -z "${monitor_pid:-}" ] || wait "$monitor_pid" >/dev/null 2>&1 || true
  [ -z "${tray_pid:-}" ] || wait "$tray_pid" >/dev/null 2>&1 || true
  [ -z "${panel_pid:-}" ] || wait "$panel_pid" >/dev/null 2>&1 || true
  [ -z "${xvfb_pid:-}" ] || wait "$xvfb_pid" >/dev/null 2>&1 || true
  rm -rf "$session_root"
}
trap cleanup 0 1 2 15

xfconf-query --channel xfce4-panel --property /panels --reset --recursive || true
xfconf-query --channel xfce4-panel --property /plugins --reset --recursive || true
xfconf-query --channel xfce4-panel --property /configver --create --type int --set 2
xfconf-query --channel xfce4-panel --property /panels --create --force-array --type int --set 1
xfconf-query --channel xfce4-panel --property /panels/panel-1/position --create --type string --set 'p=6;x=0;y=0'
xfconf-query --channel xfce4-panel --property /panels/panel-1/position-locked --create --type bool --set true
xfconf-query --channel xfce4-panel --property /panels/panel-1/length --create --type uint --set 100
xfconf-query --channel xfce4-panel --property /panels/panel-1/size --create --type uint --set 32
xfconf-query --channel xfce4-panel --property /panels/panel-1/plugin-ids --create --force-array --type int --set 1
xfconf-query --channel xfce4-panel --property /plugins/plugin-1 --create --type string --set systray
xfconf-query --channel xfce4-panel --list --verbose >"$artifact_dir/xfce4-panel-config.log"

export DISPLAY=:99
Xvfb "$DISPLAY" -screen 0 1280x160x24 -nolisten tcp >"$artifact_dir/xvfb.log" 2>&1 &
xvfb_pid=$!

for _ in $(seq 1 40); do
  [ -S "/tmp/.X11-unix/X99" ] && break
  sleep 0.1
done
[ -S "/tmp/.X11-unix/X99" ] || {
  printf 'Xvfb did not start\n' >&2
  exit 1
}

xfce4-panel --disable-wm-check --sm-client-disable >"$artifact_dir/xfce4-panel.log" 2>&1 &
panel_pid=$!

for _ in $(seq 1 80); do
  if gdbus call --session \
    --dest org.kde.StatusNotifierWatcher \
    --object-path /StatusNotifierWatcher \
    --method org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
gdbus call --session \
  --dest org.kde.StatusNotifierWatcher \
  --object-path /StatusNotifierWatcher \
  --method org.freedesktop.DBus.Peer.Ping >/dev/null

dbus-monitor --session \
  "type='method_call',interface='org.kde.StatusNotifierWatcher'" \
  >"$artifact_dir/status-notifier-dbus.log" 2>&1 &
monitor_pid=$!
sleep 0.2

"$tray_binary" >"$artifact_dir/wakezilla-tray.stdout.log" \
  2>"$artifact_dir/wakezilla-tray.stderr.log" &
tray_pid=$!

gdbus call --session \
  --dest org.freedesktop.DBus \
  --object-path /org/freedesktop/DBus \
  --method org.freedesktop.DBus.ListNames \
  >"$artifact_dir/session-bus-names.log"

registered_items="$artifact_dir/registered-status-notifier-items.log"
for _ in $(seq 1 100); do
  gdbus call --session \
    --dest org.kde.StatusNotifierWatcher \
    --object-path /StatusNotifierWatcher \
    --method org.freedesktop.DBus.Properties.Get \
    org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems \
    >"$registered_items" 2>&1 || true
  if grep -Eq "<\\['[^']+" "$registered_items"; then
    break
  fi
  sleep 0.1
done

cat "$registered_items"
grep -Eq "<\\['[^']+" "$registered_items" || {
  printf 'Wakezilla tray did not register a StatusNotifierItem with the graphical panel\n' >&2
  exit 1
}

if ! import -display "$DISPLAY" -window root "$artifact_dir/linux-tray-desktop.png" \
  2>"$artifact_dir/screenshot.log"; then
  printf 'warning: could not capture the Xvfb screen; D-Bus registration remains verified\n' \
    >&2
fi
