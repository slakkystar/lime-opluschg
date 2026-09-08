#!/bin/bash

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/dist/vendor.oplus.hardware.charger-V6-service}"

INTERFACE_VERSION="$(sed -n 's/^pub const INTERFACE_VERSION: i32 = \([0-9][0-9]*\);$/\1/p' "$ROOT/src/lib.rs")"
VINTF_VERSION="$(sed -n 's/^[[:space:]]*<version>\([0-9][0-9]*\)<\/version>.*$/\1/p' "$ROOT/charger-hal-service.xml")"
[ -n "$INTERFACE_VERSION" ]
[ "$INTERFACE_VERSION" = "11" ]
[ "$VINTF_VERSION" = "6" ]

if ! rg -q '^rsbinder = \{ version = "=0\.10\.0", features = \["android_10_plus"\] \}$' "$ROOT/Cargo.toml" \
    || ! rg -q '^rsbinder-aidl = "=0\.10\.0"$' "$ROOT/Cargo.toml"; then
    echo "error: Binder dependency no longer enables Android 10 through 17 compatibility" >&2
    exit 1
fi

if rg -n 'disable_background_scheduling|setpriority|nice\(' "$ROOT/src/main.rs"; then
    echo "error: Binder runtime scheduling was modified" >&2
    exit 1
fi

if ! rg -q '^const SCREEN_OFF_CHARGING_POLL_INTERVAL: Duration = Duration::from_secs\(5\);$' "$ROOT/src/adapter.rs" \
    || ! rg -q '^const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs\(30\);$' "$ROOT/src/adapter.rs" \
    || ! rg -q '^const SCREEN_WAKE_SCAN_DEFER: Duration = Duration::from_millis\(750\);$' "$ROOT/src/adapter.rs"; then
    echo "error: charging probe/full-refresh/wake-defer timing changed" >&2
    exit 1
fi

POLL_PRIORITY_BODY="$(sed -n '/fn lower_poll_thread_priority/,/^}/p' "$ROOT/src/adapter.rs")"
if ! printf '%s\n' "$POLL_PRIORITY_BODY" | rg -q 'setpriority.*10'; then
    echo "error: poll worker no longer yields to display work" >&2
    exit 1
fi

PROBE_BODY="$(sed -n '/fn power_source_probe_changed/,/^    }/p' "$ROOT/src/adapter.rs")"
if printf '%s\n' "$PROBE_BODY" | rg 'poll_once|thread::sleep|write_'; then
    echo "error: lightweight power-source probe became a full or blocking scan" >&2
    exit 1
fi

UEVENT_BODY="$(sed -n '/fn monitor_power_supply_uevents/,/^}/p' "$ROOT/src/adapter.rs")"
if ! printf '%s\n' "$UEVENT_BODY" | rg -q 'request_uevent_probe' \
    || printf '%s\n' "$UEVENT_BODY" | rg -q 'request_refresh'; then
    echo "error: power-supply uevents can trigger an unfiltered full scan" >&2
    exit 1
fi
if ! rg -q -U 'uevent_probe_pending\s*\.swap' "$ROOT/src/adapter.rs"; then
    echo "error: power-supply uevent bursts are no longer coalesced" >&2
    exit 1
fi

if ! rg -q 'fn poll_once<F>.*should_cancel' "$ROOT/src/adapter.rs" \
    || ! rg -q 'let scan_completed = Adapter::poll_once' "$ROOT/src/adapter.rs" \
    || ! rg -q 'if !scan_completed' "$ROOT/src/adapter.rs"; then
    echo "error: full scans are no longer cancellable during screen transitions" >&2
    exit 1
fi

WAKE_CANCEL_CHECKS="$(rg -c 'screen_wake_pending\.load\(Ordering::Acquire\)' "$ROOT/src/adapter.rs")"
if [ "$WAKE_CANCEL_CHECKS" -lt 4 ]; then
    echo "error: scan tail no longer yields before cache publication and charge control" >&2
    exit 1
fi

if rg -n '/(sys|proc)/[^" ]*(oplus|oppo)|/proc/wireless' "$ROOT/src"; then
    echo "error: OPlus-only kernel path found in source" >&2
    exit 1
fi

SCREEN_BODY="$(sed -n '/pub fn notify_screen_status/,/^    }/p' "$ROOT/src/adapter.rs")"
if printf '%s\n' "$SCREEN_BODY" | rg 'read_|write_|sleep|\.lock\(|request_refresh|thread::|setpriority|tracing::|try_send|wake_poll_worker'; then
    echo "error: blocking or I/O work found in notify_screen_status" >&2
    exit 1
fi
if ! printf '%s\n' "$SCREEN_BODY" | rg -q 'screen_on\.swap' \
    || ! printf '%s\n' "$SCREEN_BODY" | rg -q 'screen_wake_pending\.store'; then
    echo "error: notify_screen_status is not a pure atomic transition marker" >&2
    exit 1
fi

WAKE_BODY="$(sed -n '/fn wake_poll_worker/,/^    }/p' "$ROOT/src/adapter.rs")"
if printf '%s\n' "$WAKE_BODY" | rg 'recv|sleep|\.lock\(|\.send\('; then
    echo "error: blocking work found in wake_poll_worker" >&2
    exit 1
fi
if ! printf '%s\n' "$WAKE_BODY" | rg -q 'try_send'; then
    echo "error: wake_poll_worker no longer uses try_send" >&2
    exit 1
fi

DECIMAL_BODY="$(sed -n '/pub fn get_decimal_soc/,/^    }/p' "$ROOT/src/adapter.rs")"
if printf '%s\n' "$DECIMAL_BODY" | rg 'read_|write_|sleep|request_refresh|thread::'; then
    echo "error: I/O or scheduling work found in get_decimal_soc" >&2
    exit 1
fi

if rg -n 'group .*wakelock|write /sys/power/wake' "$ROOT/charger-hal-service.rc"; then
    echo "error: init service requests wake-lock access" >&2
    exit 1
fi

if [ -f "$BIN" ]; then
    if strings -a "$BIN" | rg -i 'wake_lock|wake_unlock|alarmtimer|timerfd_create|autosuspend|suspend_blocker|/sys/power/wake'; then
        echo "error: wake-capable API found in service binary" >&2
        exit 1
    fi
    if strings -a "$BIN" | rg '/(sys|proc)/[^ ]*(oplus|oppo)|/proc/wireless'; then
        echo "error: OPlus-only kernel path found in service binary" >&2
        exit 1
    fi
fi

echo "PASS: metadata, screen path, kernel paths, and wake APIs are clean"
