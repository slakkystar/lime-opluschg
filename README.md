# OPlus Charger HAL Adapter

Rust/NDK implementation of the native `ICharger` HAL for Xiaomi devices
running ColorOS. It reads standard `power_supply` and Xiaomi `qcom-battery`
sysfs nodes and exposes:

```text
vendor.oplus.hardware.charger.ICharger/default
```

This is a ROM integration component, not an APK.

> [!WARNING]
> This service runs as root and writes device-specific charging nodes. Test it
> only on a recoverable device with a complete backup. Incorrect integration
> can break charging, boot, or battery reporting.

## Compatibility

- Android API 30 through 37. `rsbinder` also supports API 29.
- `aarch64-linux-android` / `arm64-v8a` only.
- Native link target: Android API 26.
- Tested toolchain: Rust stable and Android NDK r29.
- VINTF AIDL version: 6.
- Binder stable-interface metadata: version 11.

The API range describes Binder protocol compatibility. Hardware behavior still
depends on the target kernel, ROM, SELinux policy, and available sysfs nodes.

## Features

- Battery, USB, PD, charge-pump, and wireless charging state.
- Background cache refreshed by power-supply uevents and timed polling.
- Fast-charge classification with USB data-port protection.
- Charge limit, bypass charging, and cooldown controls when supported by the device.
- Compatibility stubs for unsupported ColorOS calls.

## Build

Requirements: Linux, Rust stable, `aarch64-linux-android`, Android NDK r29, and
`ripgrep` for static validation.

```bash
rustup target add aarch64-linux-android
export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/29.0.14206865"
./build.sh release
```

For a debug build:

```bash
./build.sh debug
```

The release binary and Soong package are generated under `target/` and
`dist/`. These directories are ignored by Git.

## ROM Integration

1. Add the source tree to the ROM/device build tree.
2. Run `./build.sh release` before invoking Soong.
3. Include `Android.bp`, `charger-hal-service.rc`, and `charger-hal-service.xml`.
4. Build the `vendor.oplus.hardware.charger-V6-service` module. It installs to `/odm/bin/hw`.
5. Add target-specific SELinux rules and verify all writable sysfs nodes.
6. Check VINTF merging, init logs, and Binder registration before flashing.

The project does not provide generic SELinux policy and does not support live
binary replacement as a complete installation method.

## Validation

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
./tools/validate-static.sh
```

After installation, run the device lock/wake test from a connected ADB device:

```bash
REQUIRE_CHARGING=1 MAX_WAKE_MS=1000 ./tools/validate-device.sh 10
```

This test checks service restarts, uninterruptible sleep, wake locks, and wake
latency. Do not run it on a primary device.

## Limitations

- This is a device-specific Xiaomi/ColorOS compatibility layer, not a generic Android HAL.
- Charging node names, units, permissions, and control behavior vary by kernel.
- Some OPlus methods are stubs because the target Xiaomi kernel lacks the corresponding hardware.
- Authentication and short-circuit health values include target-specific compatibility behavior.
- Keep VINTF version 6 and Binder metadata version 11 together; changing only one can break integration.

## License

Project source is licensed under Apache-2.0. See [`LICENSE`](LICENSE).
