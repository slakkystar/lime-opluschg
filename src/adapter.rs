// Xiaomi → ColorOS ICharger adapter. Reads standard + qcom-battery sysfs,
// exposes vendor.oplus.hardware.charger.ICharger/default.

use parking_lot::Mutex;
use std::fs;
#[cfg(target_os = "android")]
use std::io;
#[cfg(target_os = "android")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
#[cfg(target_os = "android")]
use std::sync::Weak;
use std::thread;
use std::time::{Duration, Instant};

// ── sysfs helpers ──

fn try_read_int(path: &str) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(clamp_i64_to_i32)
}
fn read_int(path: &str) -> i32 {
    try_read_int(path).unwrap_or(0)
}
fn read_string(path: &str) -> String {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}
fn try_read_int_any(paths: &[&str]) -> Option<i32> {
    paths.iter().find_map(|path| try_read_int(path))
}
fn read_int_any(paths: &[&str]) -> i32 {
    try_read_int_any(paths).unwrap_or(0)
}
fn update_int_from_paths(target: &mut i32, paths: &[&str]) {
    if let Some(value) = try_read_int_any(paths) {
        *target = value;
    }
}
fn update_non_empty_string_from_paths(target: &mut String, paths: &[&str]) {
    for path in paths {
        if let Ok(value) = fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() {
                target.clear();
                target.push_str(value);
                return;
            }
        }
    }
}
fn read_positive_int_any(paths: &[&str]) -> i32 {
    for p in paths {
        let value = read_int(p);
        if value > 0 {
            return value;
        }
    }
    0
}
fn read_string_any(paths: &[&str]) -> String {
    for p in paths {
        if let Ok(value) = fs::read_to_string(p) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}
fn read_non_empty_string_any(paths: &[&str]) -> String {
    for p in paths {
        if path_exists(p) {
            let value = read_string(p);
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}
fn write_string_any(paths: &[&str], value: &str) -> bool {
    let mut wrote = false;
    for p in paths {
        if path_exists(p) && fs::write(p, value).is_ok() {
            wrote = true;
        }
    }
    wrote
}
fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}
fn clamp_u64_to_i32(value: u64) -> i32 {
    value.min(i32::MAX as u64) as i32
}
fn abs_i32_to_i64(value: i32) -> i64 {
    (value as i64).abs()
}
fn normalize_capacity_mah(value: i32) -> i32 {
    if value <= 0 {
        0
    } else if value > 100_000 {
        value / 1000
    } else {
        value
    }
}
fn normalize_battery_capacity(value: i32) -> i32 {
    let percent = if value > 100 { value / 100 } else { value };
    percent.clamp(0, 100)
}
fn parse_first_int(value: &str) -> Option<i32> {
    value
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .find(|part| !part.is_empty() && *part != "-")
        .and_then(|part| part.parse::<i32>().ok())
}
fn parse_keyed_int(value: &str, key: &str) -> Option<i32> {
    value.split('+').find_map(|part| {
        let (name, raw) = part.split_once('=')?;
        if name == key {
            raw.trim().parse::<i32>().ok()
        } else {
            None
        }
    })
}
fn parse_plus_ints(value: &str) -> Vec<i32> {
    value
        .split('+')
        .filter_map(|part| part.trim().parse::<i32>().ok())
        .collect()
}
fn restricted_charge_control_value(max_value: i32) -> String {
    if max_value > 1 {
        (max_value - 1).to_string()
    } else {
        CHARGE_CONTROL_LIMIT_RESTRICTED_FALLBACK.to_string()
    }
}

fn is_data_port_usb_type(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_uppercase().as_str(),
        "USB" | "SDP" | "USB_SDP" | "CDP" | "USB_CDP" | "PC" | "PC_PORT"
    )
}

#[cfg(target_os = "android")]
fn lower_poll_thread_priority() {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

#[cfg(not(target_os = "android"))]
fn lower_poll_thread_priority() {}

#[cfg(any(target_os = "android", test))]
const POWER_SUPPLY_UEVENT_FIELD: &[u8] = b"SUBSYSTEM=power_supply";

#[cfg(any(target_os = "android", test))]
fn is_power_supply_uevent(message: &[u8]) -> bool {
    message
        .split(|byte| *byte == 0)
        .any(|field| field == POWER_SUPPLY_UEVENT_FIELD)
}

#[cfg(target_os = "android")]
fn monitor_power_supply_uevents(adapter: Weak<Adapter>) -> io::Result<()> {
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    address.nl_groups = 1;
    let bind_result = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bind_result < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = [0_u8; 4096];
    loop {
        let received = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if received > 0 && is_power_supply_uevent(&buffer[..received as usize]) {
            let Some(adapter) = adapter.upgrade() else {
                return Ok(());
            };
            adapter.request_uevent_probe();
        }
    }
}

// ── sysfs paths ──

const PSY_BATTERY: &str = "/sys/class/power_supply/battery";
const PSY_USB: &str = "/sys/class/power_supply/usb";
const PSY_AC: &str = "/sys/class/power_supply/ac";
const PSY_WIRELESS: &str = "/sys/class/power_supply/wireless";
const PSY_CP: &str = "/sys/class/power_supply/cp";
const PSY_DC: &str = "/sys/class/power_supply/dc";
const QCOM_BATT: &str = "/sys/class/qcom-battery";

const PD_VERIFIED_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/pd_verifed",
    "/sys/class/power_supply/usb/pd_authentication",
    "/sys/class/subpmic-battery/pd_verifed",
    "/sys/class/Charging_Adapter/pd_adapter/usbpd_verifed",
];
const QUICK_CHG_TYPE_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/quick_charge_type",
    "/sys/class/power_supply/usb/quick_charge_type",
    "/sys/class/power_supply/battery/quick_charge_type",
];
const QCOM_REAL_TYPE_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/real_type",
    "/sys/class/power_supply/usb/real_type",
    "/sys/class/qcom-battery/usb_real_type",
];
const PC_PORT_ONLINE_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/pc_port_online",
    "/sys/class/power_supply/usb/pc_port_online",
    "/sys/class/qcom-battery/pc_port_online",
];
const USB_CURRENT_NOW_PATHS: &[&str] = &[
    "/sys/class/power_supply/usb/current_now",
    "/sys/class/power_supply/usb/input_current_now",
];
const USB_CONNECTOR_TEMP_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/connector_temp",
    "/sys/class/power_supply/usb/usb_temp",
];
const CP_BUS_VOLTAGE_PATHS: &[&str] = &["/sys/class/qcom-battery/bq2597x_bus_voltage"];
const CP_BUS_CURRENT_PATHS: &[&str] = &["/sys/class/qcom-battery/bq2597x_bus_current"];
const CP_ONLINE_PATHS: &[&str] = &[
    "/sys/class/power_supply/cp/online",
    "/sys/class/qcom-battery/bq2597x_chip_ok",
    "/sys/class/qcom-battery/master_smb1396_online",
    "/sys/class/qcom-battery/slave_smb1396_online",
];
const FG_FCC_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/batt_fcc",
    "/sys/class/qcom-battery/fg1_fcc",
    "/sys/class/power_supply/battery/charge_full",
];
const CHARGE_FULL_DESIGN_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/charge_full_design",
    "/sys/class/qcom-battery/fg1_design_capacity",
    "/sys/class/qcom-battery/fg2_design_capacity",
];
const FG_RM_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/batt_rm",
    "/sys/class/qcom-battery/fg1_rm",
    "/sys/class/power_supply/battery/charge_counter",
];
const CHARGE_COUNTER_PATHS: &[&str] = &["/sys/class/power_supply/battery/charge_counter"];
const FG_RSOC: &str = "/sys/class/qcom-battery/fg1_rsoc";
const BATTERY_CAPACITY_PATHS: &[&str] = &["/sys/class/power_supply/battery/capacity", FG_RSOC];
const FG_CYCLE_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/fg1_cycle",
    "/sys/class/power_supply/battery/cycle_count",
];
const FG_SOH_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/fg1_soh",
    "/sys/class/qcom-battery/soh",
    "/sys/class/power_supply/bms/soh",
];
const FG_QMAX_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/fg1_qmax",
    "/sys/class/power_supply/battery/qmax",
];
const BATTERY_TYPE_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/battery_type",
    "/sys/class/power_supply/battery/technology",
];
const INPUT_CURRENT_MAX_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/fg1_current_max",
    "/sys/class/qcom-battery/constant_power",
    "/sys/class/power_supply/usb_main/constant_charge_current_max",
    "/sys/class/power_supply/usb/current_max",
    "/sys/class/power_supply/usb_main/input_current_max",
    // restrict_cur is a static limit/threshold (AICL/thermal restrict),
    // not the actual charging current. On some qpnp-smb5 platforms
    // (Redmi 9T and similar) this node is always present with the same
    // value regardless of whether charging is active, so it must not
    // rank above the live "current_max"/"input_current_settled" nodes -
    // otherwise the power calculation gets permanently stuck on this value.
    "/sys/class/qcom-battery/restrict_cur",
];

// "Settled" nodes reflect what the PMIC has actually negotiated with the
// adapter right now (0 when idle, a live value while charging) - this is
// the most reliable source for computing real charging power on qpnp-smb5.
const INPUT_CURRENT_SETTLED_PATHS: &[&str] = &[
    "/sys/class/power_supply/main/input_current_settled",
    "/sys/class/power_supply/usb/input_current_settled",
];
const INPUT_VOLTAGE_SETTLED_PATHS: &[&str] = &[
    "/sys/class/power_supply/main/input_voltage_settled",
    "/sys/class/power_supply/usb/input_voltage_settled",
];
const ADAPTER_POWER_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/apdo_max",
    "/sys/class/qcom-battery/power_max",
    "/sys/class/power_supply/usb/apdo_max",
    "/sys/class/power_supply/usb/power_max",
    "/sys/class/qcom-battery/referance_power",
];
const REMAINING_TIME_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/remaining_time",
    "/sys/class/power_supply/battery/time_to_full_now",
];
const FASTCHG_MODE_PATHS: &[&str] = &[
    "/sys/class/qcom-battery/fastchg_mode",
    "/sys/class/power_supply/bms/fastcharge_mode",
];
const SHORT_CIRCUIT_HEALTHY: i32 = 1;
const CHARGE_CONTROL_LIMIT_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/charge_control_limit",
    "/sys/class/qcom-battery/charge_control_limit",
];
const CHARGE_CONTROL_LIMIT_MAX_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/charge_control_limit_max",
    "/sys/class/qcom-battery/charge_control_limit_max",
];
const COOL_MODE_PATHS: &[&str] = &[
    "/sys/class/power_supply/main/cool_mode",
    "/sys/class/qcom-battery/cool_mode",
];
const COOL_DOWN_PATHS: &[&str] = &["/sys/class/power_supply/battery/cool_down"];
const CHARGE_STOP_THRESHOLD_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/charge_limit",
    "/sys/class/qcom-battery/charge_limit",
    "/sys/class/power_supply/battery/charge_control_end_threshold",
    "/sys/class/power_supply/battery/charge_stop_threshold",
];
const CHARGE_LIMIT_STATE_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/smart_chg",
    "/sys/class/qcom-battery/smart_chg",
    "/sys/class/power_supply/battery/night_charging",
    "/sys/class/qcom-battery/night_charging",
];
const INPUT_SUSPEND_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/input_suspend",
    "/sys/class/qcom-battery/input_suspend",
];
const BYPASS_STATUS_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/bypass_charging",
    "/sys/class/qcom-battery/bypass_charging",
    "/sys/class/power_supply/battery/bypass_charge",
    "/sys/class/qcom-battery/bypass_charge",
    "/sys/class/power_supply/battery/charge_bypass",
    "/sys/class/qcom-battery/charge_bypass",
];
const CHARGE_CONTROL_LIMIT_RESTRICTED_FALLBACK: i32 = 15;
const CHARGE_CONTROL_LIMIT_RELEASED: &str = "0";
const CHARGE_LIMIT_HYSTERESIS_PERCENT: i32 = 1;
const POWER_RECHECK_MIN_W: i32 = 30;
const POWER_RECHECK_MAX_W: i32 = 35;
const POWER_RECHECK_DELAY_MS: u64 = 10;
const SCREEN_ON_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SCREEN_OFF_CHARGING_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SCREEN_OFF_IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const SCREEN_WAKE_SCAN_DEFER: Duration = Duration::from_millis(750);
const INITIAL_REFRESH_WAIT: Duration = Duration::from_millis(250);
const SYNTHETIC_DECIMAL_MIN_CENTI: i32 = 5;
const SYNTHETIC_DECIMAL_SEED_MAX_CENTI: i32 = 50;
const SYNTHETIC_DECIMAL_MAX_CENTI: i32 = 99;
const SYNTHETIC_DECIMAL_STEP_CENTI: i32 = 1;
const SYNTHETIC_DECIMAL_MAX_STEP_CENTI: i32 = 3;

// Individual qcom-battery nodes
const CP_MASTER_IIN: &str = "/sys/class/qcom-battery/master_smb1396_iin";
const CP_SLAVE_IIN: &str = "/sys/class/qcom-battery/slave_smb1396_iin";
const TYPEC_MODE: &str = "/sys/class/qcom-battery/typec_mode";
const CC_ORIENTATION: &str = "/sys/class/qcom-battery/cc_orientation";
const CURRENT_STATE: &str = "/sys/class/qcom-battery/current_state";
const SPORT_MODE: &str = "/sys/class/qcom-battery/sport_mode";
const WIRELESS_TYPE: &str = "/sys/class/qcom-battery/wireless_type";
const SMART_CHG: &str = "/sys/class/qcom-battery/smart_chg";
const NIGHT_CHARGING: &str = "/sys/class/qcom-battery/night_charging";
const SMART_BATT: &str = "/sys/class/qcom-battery/smart_batt";
const RESTRICT_CHG: &str = "/sys/class/qcom-battery/restrict_chg";
const BATT_SN_PATHS: &[&str] = &[
    "/sys/class/power_supply/battery/battery_sn",
    "/sys/class/qcom-battery/batt_sn",
    "/sys/class/power_supply/bms/serial_number",
    "/sys/class/power_supply/battery/serial_number",
];
const BATT_CONT_ONLINE: &str = "/sys/class/qcom-battery/battcont_online";
const FG_AI: &str = "/sys/class/qcom-battery/fg1_ai";
const FG_AVG_CURRENT: &str = "/sys/class/qcom-battery/fg1_avg_current";
const FG_VENDOR: &str = "/sys/class/qcom-battery/fg_vendor";
const UI_SOC_DECIMAL_PATHS: &[&str] = &[
    "/proc/ui_soc_decimal",
    "/sys/class/power_supply/bms/soc_decimal",
    "/sys/class/qcom-battery/soc_decimal",
];
const UI_SOC_DECIMAL_RATE_PATHS: &[&str] = &[
    "/sys/class/power_supply/bms/soc_decimal_rate",
    "/sys/class/qcom-battery/soc_decimal_rate",
];
const FG_CELL1_VOL: &str = "/sys/class/qcom-battery/fg1_cell1_vol";
const FG_CELL2_VOL: &str = "/sys/class/qcom-battery/fg1_cell2_vol";
const FG_CELL1_RASCALE: &str = "/sys/class/qcom-battery/fg1_cell1_rascale";
const MAX_LIFE_TEMP: &str = "/sys/class/qcom-battery/max_life_temp";
const MAX_LIFE_VOL: &str = "/sys/class/qcom-battery/max_life_vol";
const OVER_VOL_DURATION: &str = "/sys/class/qcom-battery/over_vol_duration";
const MOISTURE_STATUS: &str = "/sys/class/qcom-battery/moisture_detection_status";
const THERMAL_BOARD_TEMP: &str = "/sys/class/qcom-battery/thermal_board_temp";
const DIE_TEMPERATURE: &str = "/sys/class/qcom-battery/die_temperature";
const SLAVE_DIE_TEMPERATURE: &str = "/sys/class/qcom-battery/slave_die_temperature";
const FLASH_ACTIVE: &str = "/sys/class/qcom-battery/flash_active";
const HIFI_CONNECT: &str = "/sys/class/qcom-battery/hifi_connect";
const VBUS_DISABLE: &str = "/sys/class/qcom-battery/vbus_disable";
const OTG_UI_SUPPORT: &str = "/sys/class/qcom-battery/otg_ui_support";
const FAKE_SOC: &str = "/sys/class/qcom-battery/fake_soc";
const FAKE_SOH: &str = "/sys/class/qcom-battery/fake_soh";
const FAKE_CYCLE: &str = "/sys/class/qcom-battery/fake_cycle";
const FAKE_TEMP: &str = "/sys/class/qcom-battery/fake_temp";

// ── State machine ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChargeState {
    Unknown,
    Disconnected,
    SlowCharging,
    NormalCharging,
    FastCharging,
    FlashCharging,
    SuperCharging,
}

#[derive(Debug, Clone)]
pub struct ChargerInfo {
    pub usb_online: i32,
    pub usb_type: String,
    pub usb_real_type: String,
    pub usb_voltage_now: i32,
    pub usb_current_now: i32,
    pub usb_temp: i32,
    pub connector_temp: i32,
    pub ac_online: i32,
    pub pc_port_online: i32,
    pub wireless_online: i32,
    pub wireless_type: String,
    pub typec_mode: String,
    pub cc_orientation: i32,
    pub battery_present: bool,
    pub battery_status: String,
    pub battery_health: String,
    pub battery_capacity: i32,
    pub battery_temp: i32,
    pub battery_current_now: i32,
    pub battery_voltage_now: i32,
    pub battery_charge_type: String,
    pub battery_technology: String,
    pub charge_full: i32,
    pub charge_full_design: i32,
    pub charge_counter: i32,
    pub cycle_count: i32,
    pub fg_fcc: i32,
    pub fg_rm: i32,
    pub fg_rsoc: i32,
    pub fg_soh: i32,
    pub fg_cycle: i32,
    pub fg_qmax: i32,
    pub fg_ai: i32,
    pub fg_avg_current: i32,
    pub fg_vendor: String,
    pub battery_type: String,
    pub gauge_type: String,
    pub gauge_info: String,
    pub cell1_vol: i32,
    pub cell2_vol: i32,
    pub cell1_rascale: i32,
    pub fast_charge_type: String,
    pub charge_technology: String,
    pub quick_charge_type: String,
    pub pd_verified: i32,
    pub(crate) charge_state: ChargeState,
    pub input_current_max: i32,
    pub input_voltage_max: i32,
    pub input_current_settled: i32,
    pub input_voltage_settled: i32,
    pub fastchg_mode: i32,
    pub current_state: String,
    pub sport_mode: i32,
    pub cp_online: i32,
    pub cp_status: String,
    pub cp_bus_voltage: i32,
    pub cp_bus_current: i32,
    pub cp_master_iin: i32,
    pub cp_slave_iin: i32,
    pub adapter_power_w: i32,
    pub remaining_time: i32,
    pub restrict_chg: i32,
    pub input_suspend: i32,
    pub smart_chg: i32,
    pub night_charging: i32,
    pub smart_batt: i32,
    pub die_temperature: i32,
    pub slave_die_temperature: i32,
    pub thermal_board_temp: i32,
    pub batt_sn: String,
    pub authentic: i32,
    pub batt_cont_online: i32,
    pub max_life_temp: i32,
    pub max_life_vol: i32,
    pub over_vol_duration: i32,
    pub moisture_detected: bool,
    pub flash_active: bool,
    pub hifi_connect: bool,
    pub vbus_disable: bool,
    pub otg_ui_support: i32,
    pub fake_soc: i32,
    pub fake_soh: i32,
    pub fake_cycle: i32,
    pub fake_temp: i32,
}

#[derive(Debug)]
struct DecimalSocState {
    capacity: i32,
    target: i32,
    random: u32,
}

impl Default for DecimalSocState {
    fn default() -> Self {
        Self {
            capacity: -1,
            target: 0,
            random: 0,
        }
    }
}

struct FastChargeInputs<'a> {
    quick_charge_type: &'a str,
    fastchg_mode: i32,
    sport_mode: i32,
    pd_verified: i32,
    cp_online: i32,
    usb_type: &'a str,
    adapter_power_w: i32,
    online: bool,
    usb_online: i32,
    pc_port_online: i32,
}

#[derive(Debug, Clone, Default)]
pub struct FastChargeSnapshot {
    pub online: bool,
    pub usb_online: i32,
    pub ac_online: i32,
    pub wireless_online: i32,
    pub fast_type: i32,
    pub charge_tech: i32,
    pub quick_charge_type: String,
    pub pd_verified: i32,
    pub cp_online: i32,
    pub fastchg_mode: i32,
    pub sport_mode: i32,
    pub adapter_power_w: i32,
    pub usb_type: String,
    pub pc_port_online: i32,
}

impl FastChargeSnapshot {
    fn is_data_port(&self) -> bool {
        self.usb_online != 0 && (self.pc_port_online != 0 || is_data_port_usb_type(&self.usb_type))
    }

    pub fn is_fast_charge(&self) -> bool {
        self.online
            && !self.is_data_port()
            && (self.fast_type > 0
                || self.fastchg_mode != 0
                || self.sport_mode != 0
                || self.pd_verified != 0
                || self.cp_online != 0
                || self.adapter_power_w >= 10)
    }

    pub fn is_svooc_active(&self) -> bool {
        self.online
            && !self.is_data_port()
            && (self.fast_type >= 2
                || self.fastchg_mode != 0
                || self.sport_mode != 0
                || self.cp_online != 0
                || self.adapter_power_w >= 20)
    }

    pub fn is_pps_active(&self) -> bool {
        let usb = self.usb_type.to_lowercase();
        let quick = self.quick_charge_type.to_lowercase();
        self.online
            && !self.is_data_port()
            && (self.charge_tech >= 3
                || self.pd_verified != 0
                || usb.contains("pd")
                || usb.contains("pps")
                || quick.contains("pd")
                || quick.contains("pps"))
    }

    pub fn should_show_power(&self) -> bool {
        !self.is_data_port() && self.quick_charge_type.trim() == "4"
    }
}

impl Default for ChargerInfo {
    fn default() -> Self {
        ChargerInfo {
            usb_online: 0,
            usb_type: String::new(),
            usb_real_type: String::new(),
            usb_voltage_now: 0,
            usb_current_now: 0,
            usb_temp: 0,
            connector_temp: 0,
            ac_online: 0,
            pc_port_online: 0,
            wireless_online: 0,
            wireless_type: String::new(),
            typec_mode: String::new(),
            cc_orientation: 0,
            battery_present: true,
            battery_status: "Unknown".into(),
            battery_health: "Unknown".into(),
            battery_capacity: 0,
            battery_temp: 0,
            battery_current_now: 0,
            battery_voltage_now: 0,
            battery_charge_type: "Unknown".into(),
            battery_technology: String::new(),
            charge_full: 0,
            charge_full_design: 0,
            charge_counter: 0,
            cycle_count: 0,
            fg_fcc: 0,
            fg_rm: 0,
            fg_rsoc: 0,
            fg_soh: 0,
            fg_cycle: 0,
            fg_qmax: 0,
            fg_ai: 0,
            fg_avg_current: 0,
            fg_vendor: String::new(),
            battery_type: String::new(),
            gauge_type: String::new(),
            gauge_info: String::new(),
            cell1_vol: 0,
            cell2_vol: 0,
            cell1_rascale: 0,
            fast_charge_type: "0".into(),
            charge_technology: "0".into(),
            quick_charge_type: String::new(),
            pd_verified: 0,
            charge_state: ChargeState::Unknown,
            input_current_max: 0,
            input_voltage_max: 0,
            input_current_settled: 0,
            input_voltage_settled: 0,
            fastchg_mode: 0,
            current_state: String::new(),
            sport_mode: 0,
            cp_online: 0,
            cp_status: String::new(),
            cp_bus_voltage: 0,
            cp_bus_current: 0,
            cp_master_iin: 0,
            cp_slave_iin: 0,
            adapter_power_w: 0,
            remaining_time: 0,
            restrict_chg: 0,
            input_suspend: 0,
            smart_chg: 0,
            night_charging: 0,
            smart_batt: 0,
            die_temperature: 0,
            slave_die_temperature: 0,
            thermal_board_temp: 0,
            batt_sn: String::new(),
            authentic: 1,
            batt_cont_online: 0,
            max_life_temp: 0,
            max_life_vol: 0,
            over_vol_duration: 0,
            moisture_detected: false,
            flash_active: false,
            hifi_connect: false,
            vbus_disable: false,
            otg_ui_support: 0,
            fake_soc: 0,
            fake_soh: 0,
            fake_cycle: 0,
            fake_temp: 0,
        }
    }
}

// ── Adapter ──

pub struct Adapter {
    pub info: Mutex<ChargerInfo>,
    pub charger_info_json: Mutex<String>,
    pub reverse_chg_info: Mutex<String>, // STUB: hardcoded format, no real reverse charge
    pub battery_balance_info: Mutex<String>, // populated from dual-cell voltage
    pub usb_eye_diagram: Mutex<String>,  // STUB: hardcoded "0,0,0,0,0,0,0,0,0,0"
    pub battery_auth_status: Mutex<String>,
    pub battery_type_cache: Mutex<String>,
    fast_charge_snapshot: Mutex<FastChargeSnapshot>,
    poll_wake_tx: SyncSender<()>,
    refresh_pending: AtomicBool,
    uevent_probe_pending: AtomicBool,
    screen_on: AtomicBool,
    screen_wake_pending: AtomicBool,
    battery_capacity_cache: AtomicI32,
    decimal_soc_seed: AtomicI32,
    decimal_soc_rate: AtomicI32,
    decimal_soc: Mutex<DecimalSocState>,
    pub quick_mode_gain: Mutex<String>, // STUB: hardcoded "0,0"
    pub soh_debug_info: Mutex<String>,

    // ── Settable compatibility state for features absent from the Xiaomi kernel ──
    pub bcc_anode_type: Mutex<String>,    // NO-OP
    pub eis_switch_status: Mutex<String>, // NO-OP
    pub sili_ic_alg_cfg: Mutex<String>,   // NO-OP
    pub chg_up_limit_state: Mutex<String>,
    pub chg_up_limit_value: Mutex<String>,
    pub charge_limit_active: Mutex<bool>,
    pub bypass_charge_status: Mutex<String>,
    pub charge_control_active: Mutex<bool>,
    charge_control_update_lock: Mutex<()>,
    charge_control_applied: Mutex<Option<bool>>,
    pub cooldown: Mutex<String>,           // NO-OP
    pub anti_expansion_dis: Mutex<String>, // NO-OP

    pub battery_log_enabled: AtomicBool, // NO-OP: no battery log push on Xiaomi
}

impl Adapter {
    pub fn new() -> Arc<Self> {
        let info = ChargerInfo::default();
        let (poll_wake_tx, poll_wake_rx) = sync_channel(1);
        let (initial_ready_tx, initial_ready_rx) = sync_channel(1);

        let balance = if info.cell1_vol > 0 && info.cell2_vol > 0 {
            format!("{},{},0,0,0,0,0,0", info.cell1_vol, info.cell2_vol)
        } else {
            "0,0,0,0,0,0,0,0".into()
        };
        let fast_snapshot = Adapter::fast_charge_snapshot_from_info(&info);
        let adapter = Arc::new(Adapter {
            charger_info_json: Mutex::new(Adapter::build_charger_info_json(&info)),
            reverse_chg_info: Mutex::new("0,0,0".into()),
            battery_balance_info: Mutex::new(balance),
            usb_eye_diagram: Mutex::new("0,0,0,0,0,0,0,0,0,0".into()),
            battery_auth_status: Mutex::new(info.authentic.to_string()),
            battery_type_cache: Mutex::new(Adapter::best_battery_type(&info)),
            fast_charge_snapshot: Mutex::new(fast_snapshot),
            poll_wake_tx,
            refresh_pending: AtomicBool::new(false),
            uevent_probe_pending: AtomicBool::new(false),
            screen_on: AtomicBool::new(false),
            screen_wake_pending: AtomicBool::new(false),
            battery_capacity_cache: AtomicI32::new(info.battery_capacity),
            decimal_soc_seed: AtomicI32::new(0),
            decimal_soc_rate: AtomicI32::new(0),
            decimal_soc: Mutex::new(DecimalSocState::default()),
            quick_mode_gain: Mutex::new("0,0".into()),
            soh_debug_info: Mutex::new(Adapter::build_soh_debug_info(&info)),
            bcc_anode_type: Mutex::new("0".into()),
            eis_switch_status: Mutex::new("0".into()),
            sili_ic_alg_cfg: Mutex::new("0".into()),
            chg_up_limit_state: Mutex::new("0".into()),
            chg_up_limit_value: Mutex::new("90".into()),
            charge_limit_active: Mutex::new(false),
            bypass_charge_status: Mutex::new("0".into()),
            charge_control_active: Mutex::new(false),
            charge_control_update_lock: Mutex::new(()),
            charge_control_applied: Mutex::new(Some(false)),
            cooldown: Mutex::new("0".into()),
            anti_expansion_dis: Mutex::new("0".into()),
            info: Mutex::new(info),
            battery_log_enabled: AtomicBool::new(false),
        });

        let adapter_weak = Arc::downgrade(&adapter);
        if let Err(err) = thread::Builder::new()
            .name("charger-hal-poll".into())
            .spawn(move || {
                lower_poll_thread_priority();
                let mut initial_ready_tx = Some(initial_ready_tx);
                let mut last_full_refresh = Instant::now()
                    .checked_sub(FULL_REFRESH_INTERVAL)
                    .unwrap_or_else(Instant::now);
                loop {
                    let interval = {
                        let Some(adapter) = adapter_weak.upgrade() else {
                            break;
                        };
                        let screen_on = adapter.screen_on.load(Ordering::Relaxed);
                        let online = adapter.fast_charge_snapshot.lock().online;
                        Self::poll_interval(screen_on, online)
                    };
                    let timed_out = match poll_wake_rx.recv_timeout(interval) {
                        Ok(()) => false,
                        Err(RecvTimeoutError::Timeout) => true,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    let Some(adapter_clone) = adapter_weak.upgrade() else {
                        break;
                    };
                    if adapter_clone
                        .screen_wake_pending
                        .swap(false, Ordering::AcqRel)
                    {
                        thread::sleep(SCREEN_WAKE_SCAN_DEFER);
                        adapter_clone.wake_poll_worker();
                        continue;
                    }
                    let refresh_requested =
                        adapter_clone.refresh_pending.swap(false, Ordering::Relaxed);
                    let uevent_probe_requested = adapter_clone
                        .uevent_probe_pending
                        .swap(false, Ordering::Relaxed);
                    if !timed_out && !refresh_requested && !uevent_probe_requested {
                        continue;
                    }
                    let maintenance_due = last_full_refresh.elapsed() >= FULL_REFRESH_INTERVAL;
                    if !refresh_requested
                        && !maintenance_due
                        && (timed_out || uevent_probe_requested)
                    {
                        let snapshot = adapter_clone.fast_charge_snapshot.lock().clone();
                        if !Self::power_source_probe_changed(&snapshot) {
                            continue;
                        }
                    }
                    let mut next_info = adapter_clone.info.lock().clone();
                    let scan_completed = Adapter::poll_once(&mut next_info, || {
                        adapter_clone.screen_wake_pending.load(Ordering::Acquire)
                    });
                    if !scan_completed || adapter_clone.screen_wake_pending.load(Ordering::Acquire)
                    {
                        adapter_clone.request_refresh();
                        continue;
                    }
                    let charger_info_json = Adapter::build_charger_info_json(&next_info);
                    let soh_debug_info = Adapter::build_soh_debug_info(&next_info);
                    let battery_type = Adapter::best_battery_type(&next_info);
                    let battery_auth_status = next_info.authentic.to_string();
                    let balance_info = Adapter::build_balance_info(&next_info);
                    let fast_snapshot = Adapter::fast_charge_snapshot_from_info(&next_info);
                    let decimal_seed = read_int_any(UI_SOC_DECIMAL_PATHS);
                    let decimal_rate = read_int_any(UI_SOC_DECIMAL_RATE_PATHS);
                    let capacity_cache = next_info.battery_capacity;
                    if adapter_clone.screen_wake_pending.load(Ordering::Acquire) {
                        adapter_clone.request_refresh();
                        continue;
                    }

                    *adapter_clone.info.lock() = next_info;
                    *adapter_clone.charger_info_json.lock() = charger_info_json;
                    *adapter_clone.soh_debug_info.lock() = soh_debug_info;
                    *adapter_clone.battery_type_cache.lock() = battery_type;
                    *adapter_clone.battery_auth_status.lock() = battery_auth_status;
                    *adapter_clone.battery_balance_info.lock() = balance_info;
                    *adapter_clone.fast_charge_snapshot.lock() = fast_snapshot;
                    adapter_clone
                        .battery_capacity_cache
                        .store(capacity_cache, Ordering::Relaxed);
                    adapter_clone
                        .decimal_soc_seed
                        .store(decimal_seed, Ordering::Relaxed);
                    adapter_clone
                        .decimal_soc_rate
                        .store(decimal_rate, Ordering::Relaxed);
                    if adapter_clone.screen_wake_pending.load(Ordering::Acquire) {
                        adapter_clone.request_refresh();
                        continue;
                    }
                    adapter_clone.enforce_charge_control();
                    last_full_refresh = Instant::now();
                    if let Some(initial_ready_tx) = initial_ready_tx.take() {
                        let _ = initial_ready_tx.try_send(());
                    }
                }
            })
        {
            tracing::warn!("failed to start charger poll thread: {}", err);
        }

        #[cfg(target_os = "android")]
        {
            let adapter_weak = Arc::downgrade(&adapter);
            if let Err(error) = thread::Builder::new()
                .name("charger-hal-uevent".into())
                .spawn(move || {
                    if let Err(error) = monitor_power_supply_uevents(adapter_weak) {
                        tracing::warn!("power-supply uevent monitor stopped: {}", error);
                    }
                })
            {
                tracing::warn!("failed to start power-supply uevent monitor: {}", error);
            }
        }

        adapter.request_refresh();
        match initial_ready_rx.recv_timeout(INITIAL_REFRESH_WAIT) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!("initial charger snapshot did not finish within 250 ms")
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!("charger poll thread stopped before the initial snapshot")
            }
        }

        adapter
    }

    fn poll_once<F>(info: &mut ChargerInfo, should_cancel: F) -> bool
    where
        F: Fn() -> bool,
    {
        if should_cancel() {
            return false;
        }
        let b = PSY_BATTERY;
        update_int_from_paths(&mut info.usb_online, &[&format!("{}/online", PSY_USB)]);
        update_non_empty_string_from_paths(&mut info.usb_type, &[&format!("{}/type", PSY_USB)]);
        update_int_from_paths(&mut info.ac_online, &[&format!("{}/online", PSY_AC)]);
        update_int_from_paths(
            &mut info.wireless_online,
            &[
                &format!("{}/online", PSY_WIRELESS),
                &format!("{}/online", PSY_DC),
            ],
        );
        update_non_empty_string_from_paths(&mut info.wireless_type, &[WIRELESS_TYPE]);
        if should_cancel() {
            return false;
        }

        update_int_from_paths(
            &mut info.usb_voltage_now,
            &[&format!("{}/voltage_now", PSY_USB)],
        );
        update_int_from_paths(&mut info.cp_bus_voltage, CP_BUS_VOLTAGE_PATHS);
        update_int_from_paths(&mut info.cp_bus_current, CP_BUS_CURRENT_PATHS);
        if info.usb_voltage_now == 0 {
            info.usb_voltage_now = info.cp_bus_voltage;
        }
        update_int_from_paths(&mut info.usb_current_now, USB_CURRENT_NOW_PATHS);
        if info.usb_current_now == 0 {
            info.usb_current_now = info.cp_bus_current;
        }

        update_int_from_paths(&mut info.usb_temp, USB_CONNECTOR_TEMP_PATHS);
        update_int_from_paths(&mut info.connector_temp, USB_CONNECTOR_TEMP_PATHS);
        info.usb_real_type = read_string_any(QCOM_REAL_TYPE_PATHS);
        if !info.usb_real_type.is_empty() {
            info.usb_type = info.usb_real_type.clone();
        }
        update_non_empty_string_from_paths(&mut info.typec_mode, &[TYPEC_MODE]);
        update_int_from_paths(&mut info.cc_orientation, &[CC_ORIENTATION]);
        if should_cancel() {
            return false;
        }

        update_non_empty_string_from_paths(&mut info.battery_status, &[&format!("{}/status", b)]);
        update_non_empty_string_from_paths(&mut info.battery_health, &[&format!("{}/health", b)]);
        update_int_from_paths(&mut info.battery_temp, &[&format!("{}/temp", b)]);
        update_int_from_paths(
            &mut info.battery_current_now,
            &[&format!("{}/current_now", b)],
        );
        update_int_from_paths(
            &mut info.battery_voltage_now,
            &[&format!("{}/voltage_now", b)],
        );
        update_non_empty_string_from_paths(
            &mut info.battery_charge_type,
            &[&format!("{}/charge_type", b)],
        );
        update_non_empty_string_from_paths(
            &mut info.battery_technology,
            &[&format!("{}/technology", b)],
        );
        if let Some(capacity) = try_read_int_any(BATTERY_CAPACITY_PATHS) {
            info.battery_capacity = normalize_battery_capacity(capacity);
        }
        if should_cancel() {
            return false;
        }

        info.fg_fcc = read_int(FG_FCC_PATHS[0]);
        info.charge_full = read_int_any(FG_FCC_PATHS);
        info.fg_rm = read_int(FG_RM_PATHS[0]);
        info.fg_rsoc = read_int(FG_RSOC);
        info.fg_cycle = read_int(FG_CYCLE_PATHS[0]);
        info.fg_soh = read_int_any(FG_SOH_PATHS);
        info.fg_qmax = read_int_any(FG_QMAX_PATHS);
        info.fg_ai = read_int(FG_AI);
        info.fg_avg_current = read_int(FG_AVG_CURRENT);
        info.fg_vendor = read_string(FG_VENDOR);
        info.battery_type = read_non_empty_string_any(BATTERY_TYPE_PATHS);
        info.gauge_type = info.fg_vendor.clone();
        info.gauge_info.clear();
        info.charge_full_design = read_int_any(CHARGE_FULL_DESIGN_PATHS);
        if info.charge_full_design == 0 {
            info.charge_full_design = info.charge_full;
        }
        info.charge_counter = read_int_any(CHARGE_COUNTER_PATHS);
        if info.charge_counter == 0 {
            info.charge_counter = info.fg_rm;
        }
        info.cycle_count = read_int_any(FG_CYCLE_PATHS);
        if should_cancel() {
            return false;
        }

        info.cell1_vol = read_int(FG_CELL1_VOL);
        info.cell2_vol = read_int(FG_CELL2_VOL);
        info.cell1_rascale = read_int(FG_CELL1_RASCALE);

        info.input_current_max = read_int_any(INPUT_CURRENT_MAX_PATHS);
        info.input_voltage_max = read_int(&format!("{}/voltage_max", PSY_USB));
        info.input_current_settled = read_int_any(INPUT_CURRENT_SETTLED_PATHS);
        info.input_voltage_settled = read_int_any(INPUT_VOLTAGE_SETTLED_PATHS);
        info.fastchg_mode = read_int_any(FASTCHG_MODE_PATHS);
        info.current_state = read_string(CURRENT_STATE);
        info.sport_mode = read_int(SPORT_MODE);
        info.quick_charge_type = read_string_any(QUICK_CHG_TYPE_PATHS);
        info.pd_verified = read_int_any(PD_VERIFIED_PATHS);
        if should_cancel() {
            return false;
        }

        info.cp_online = read_int_any(CP_ONLINE_PATHS);
        info.cp_status = read_string(&format!("{}/status", PSY_CP));
        info.cp_master_iin = read_int(CP_MASTER_IIN);
        info.cp_slave_iin = read_int(CP_SLAVE_IIN);

        info.remaining_time = read_int_any(REMAINING_TIME_PATHS);
        info.restrict_chg = read_int(RESTRICT_CHG);
        info.input_suspend = read_int_any(INPUT_SUSPEND_PATHS);
        info.smart_chg = read_int(SMART_CHG);
        info.night_charging = read_int(NIGHT_CHARGING);
        info.smart_batt = read_int(SMART_BATT);
        if should_cancel() {
            return false;
        }

        info.die_temperature = read_int(DIE_TEMPERATURE);
        info.slave_die_temperature = read_int(SLAVE_DIE_TEMPERATURE);
        info.thermal_board_temp = read_int(THERMAL_BOARD_TEMP);

        info.batt_sn = read_string_any(BATT_SN_PATHS);
        info.authentic = 1;
        info.batt_cont_online = read_int(BATT_CONT_ONLINE);
        info.max_life_temp = read_int(MAX_LIFE_TEMP);
        info.max_life_vol = read_int(MAX_LIFE_VOL);
        info.over_vol_duration = read_int(OVER_VOL_DURATION);
        info.moisture_detected = read_int(MOISTURE_STATUS) != 0;
        if should_cancel() {
            return false;
        }

        info.flash_active = read_int(FLASH_ACTIVE) != 0;
        info.hifi_connect = read_int(HIFI_CONNECT) != 0;
        info.vbus_disable = read_int(VBUS_DISABLE) != 0;
        info.otg_ui_support = read_int(OTG_UI_SUPPORT);

        info.fake_soc = read_int(FAKE_SOC);
        info.fake_soh = read_int(FAKE_SOH);
        info.fake_cycle = read_int(FAKE_CYCLE);
        info.fake_temp = read_int(FAKE_TEMP);

        info.pc_port_online = read_int_any(PC_PORT_ONLINE_PATHS);
        if should_cancel() {
            return false;
        }

        let charger_online =
            info.usb_online != 0 || info.ac_online != 0 || info.wireless_online != 0;
        let data_port = info.usb_online != 0
            && (info.pc_port_online != 0
                || is_data_port_usb_type(&info.usb_real_type)
                || (info.usb_real_type.is_empty() && is_data_port_usb_type(&info.usb_type)));
        if !charger_online {
            Self::clear_fast_charge_session(info);
        } else if data_port {
            info.quick_charge_type = "0".into();
            info.pd_verified = 0;
            info.cp_online = 0;
            info.cp_status.clear();
            info.cp_master_iin = 0;
            info.cp_slave_iin = 0;
            info.fastchg_mode = 0;
            info.sport_mode = 0;
            info.adapter_power_w = 0;
        } else {
            let adapter_power_w = Adapter::estimate_power(info);
            if should_cancel() {
                return false;
            }
            info.adapter_power_w = Adapter::stable_adapter_power_w_with(
                adapter_power_w,
                &info.quick_charge_type,
                Adapter::current_quick_charge_type,
                Adapter::read_adapter_power_direct_w,
                || thread::sleep(Duration::from_millis(POWER_RECHECK_DELAY_MS)),
            );
            if should_cancel() {
                return false;
            }
        }
        info.fast_charge_type = Adapter::classify_fast_charge(info);
        info.charge_technology = Adapter::classify_charge_technology(info);
        info.charge_state = Adapter::classify_charge_state(info);
        info.remaining_time = Adapter::estimate_remaining_time_seconds(info);
        true
    }

    fn clear_fast_charge_session(info: &mut ChargerInfo) {
        info.usb_type.clear();
        info.usb_real_type.clear();
        info.quick_charge_type.clear();
        info.pd_verified = 0;
        info.cp_online = 0;
        info.cp_status.clear();
        info.cp_bus_voltage = 0;
        info.cp_bus_current = 0;
        info.cp_master_iin = 0;
        info.cp_slave_iin = 0;
        info.fastchg_mode = 0;
        info.sport_mode = 0;
        info.adapter_power_w = 0;
        info.fast_charge_type = "0".into();
        info.charge_technology = "0".into();
    }

    // ── Classifiers ──

    /// Xiaomi quick_charge_type → OPlus fast_charge_type (0-3)
    fn classify_fast_charge_values(inputs: FastChargeInputs<'_>) -> i32 {
        let FastChargeInputs {
            quick_charge_type,
            fastchg_mode,
            sport_mode,
            pd_verified,
            cp_online,
            usb_type,
            adapter_power_w,
            online,
            usb_online,
            pc_port_online,
        } = inputs;
        if !online {
            return 0;
        }
        if usb_online != 0 && (pc_port_online != 0 || is_data_port_usb_type(usb_type)) {
            return 0;
        }
        let qct_num: i32 = quick_charge_type.trim().parse().unwrap_or(-1);
        match qct_num {
            1 => return 1,
            2 => return 2,
            3 | 4 => return 3,
            _ => {}
        }
        let qct = quick_charge_type;
        if qct.contains("Super")
            || qct.contains("SUPER")
            || qct.contains("Turbo")
            || qct.contains("TURBO")
        {
            return 3;
        }
        if qct.contains("Flash") || qct.contains("FLASH") {
            return 2;
        }
        if qct.contains("Fast") || qct.contains("FAST") {
            return 1;
        }
        if qct.contains("Normal") || qct.contains("NORMAL") {
            return 0;
        }
        if fastchg_mode != 0 || sport_mode != 0 || pd_verified != 0 || cp_online != 0 {
            return 3;
        }
        let usb_type = usb_type.to_lowercase();
        if usb_type.contains("pd") || usb_type.contains("pps") {
            return 3;
        }
        if usb_type.contains("qc") || usb_type.contains("hvdcp") || usb_type.contains("quick") {
            return 2;
        }
        if usb_type.contains("dcp") {
            return 1;
        }
        if usb_type.contains("sdp") || usb_type.contains("cdp") {
            return 0;
        }
        if adapter_power_w > 20 {
            3
        } else if adapter_power_w > 10 {
            2
        } else if adapter_power_w > 3 || online {
            1
        } else {
            0
        }
    }

    fn classify_fast_charge(info: &ChargerInfo) -> String {
        let ut = if !info.usb_real_type.is_empty() {
            &info.usb_real_type
        } else {
            &info.usb_type
        };
        Self::classify_fast_charge_values(FastChargeInputs {
            quick_charge_type: &info.quick_charge_type,
            fastchg_mode: info.fastchg_mode,
            sport_mode: info.sport_mode,
            pd_verified: info.pd_verified,
            cp_online: info.cp_online,
            usb_type: ut,
            adapter_power_w: info.adapter_power_w,
            online: info.usb_online != 0 || info.ac_online != 0 || info.wireless_online != 0,
            usb_online: info.usb_online,
            pc_port_online: info.pc_port_online,
        })
        .to_string()
    }

    /// Xiaomi quick_charge_type → OPlus charge_technology (0=normal,1=QC,2=HVDCP,3=PD_PPS)
    fn classify_charge_technology_values(quick_charge_type: &str, fast_type: i32) -> i32 {
        let qct_num: i32 = quick_charge_type.trim().parse().unwrap_or(-1);
        match qct_num {
            1 => return 1,
            2 => return 2,
            3 | 4 => return 3,
            _ => {}
        }
        if quick_charge_type.contains("Super")
            || quick_charge_type.contains("SUPER")
            || quick_charge_type.contains("Turbo")
            || quick_charge_type.contains("TURBO")
        {
            return 3;
        }
        if quick_charge_type.contains("Flash") || quick_charge_type.contains("FLASH") {
            return 2;
        }
        if quick_charge_type.contains("Fast") || quick_charge_type.contains("FAST") {
            return 1;
        }
        fast_type
    }

    fn classify_charge_technology(info: &ChargerInfo) -> String {
        Self::classify_charge_technology_values(
            &info.quick_charge_type,
            info.fast_charge_type.trim().parse().unwrap_or(0),
        )
        .to_string()
    }

    fn classify_charge_state(info: &ChargerInfo) -> ChargeState {
        if info.usb_online == 0 && info.ac_online == 0 && info.wireless_online == 0 {
            return ChargeState::Disconnected;
        }
        let pw = info.adapter_power_w;
        if pw > 30 {
            ChargeState::SuperCharging
        } else if pw > 20 {
            ChargeState::FlashCharging
        } else if pw > 10 {
            ChargeState::FastCharging
        } else if pw > 3 {
            ChargeState::NormalCharging
        } else {
            ChargeState::SlowCharging
        }
    }

    fn estimate_remaining_time_seconds(info: &ChargerInfo) -> i32 {
        if info.remaining_time > 0 {
            return if info.remaining_time > 86_400 {
                info.remaining_time / 1000
            } else {
                info.remaining_time
            };
        }
        if info.battery_status.eq_ignore_ascii_case("Full") || info.battery_capacity >= 100 {
            return 0;
        }
        if info.usb_online == 0 && info.ac_online == 0 && info.wireless_online == 0 {
            return 0;
        }

        let remaining_mah = if info.fg_fcc > 0 && info.fg_rm > 0 && info.fg_fcc > info.fg_rm {
            normalize_capacity_mah(info.fg_fcc - info.fg_rm)
        } else {
            let fcc_mah = Self::best_fcc_mah(info);
            if fcc_mah <= 0 {
                return 0;
            }
            clamp_i64_to_i32((fcc_mah as i64) * ((100 - info.battery_capacity).max(0) as i64) / 100)
        };
        if remaining_mah <= 0 {
            return 0;
        }

        let current_abs = abs_i32_to_i64(info.battery_current_now);
        let current_ma = if current_abs > 100_000 {
            current_abs / 1000
        } else {
            current_abs
        };
        if current_ma <= 0 {
            return 0;
        }
        clamp_i64_to_i32((remaining_mah as i64) * 3600 / current_ma)
    }

    /// Power in watts. Auto-detects W / mW / µW units.
    fn estimate_power(info: &ChargerInfo) -> i32 {
        let direct_pw = read_positive_int_any(ADAPTER_POWER_PATHS);
        if direct_pw > 0 {
            return Self::normalize_power_value(direct_pw);
        }
        let const_pw = read_int(&format!("{}/constant_power", QCOM_BATT));
        if const_pw > 0 {
            return Self::normalize_power_value(const_pw);
        }
        if info.cp_bus_voltage > 0 && info.cp_bus_current > 0 {
            return Self::power_watts(info.cp_bus_voltage, info.cp_bus_current);
        }
        if info.cp_master_iin > 0 || info.cp_slave_iin > 0 {
            let v = if info.usb_voltage_now > 0 {
                info.usb_voltage_now
            } else {
                info.battery_voltage_now
            };
            let total_i = info.cp_master_iin.saturating_add(info.cp_slave_iin);
            if v > 0 && total_i > 0 {
                return Self::power_watts(v, total_i);
            }
        }
        // Most accurate source on qpnp-smb5 platforms (Redmi 9T and similar):
        // "settled" current and voltage are what the PMIC has actually
        // negotiated with the adapter right now. Both are 0 when idle,
        // so using them is safe and won't cause false positives.
        if info.input_current_settled > 0 {
            let v = if info.input_voltage_settled > 0 {
                info.input_voltage_settled
            } else if info.usb_voltage_now > 0 {
                info.usb_voltage_now
            } else {
                info.battery_voltage_now
            };
            if v > 0 {
                return Self::power_watts(v, info.input_current_settled);
            }
        }
        let v = if info.usb_voltage_now > 0 {
            info.usb_voltage_now
        } else {
            info.battery_voltage_now
        };
        let c = if info.input_current_max > 0 {
            info.input_current_max
        } else if info.usb_current_now > 0 {
            info.usb_current_now
        } else {
            clamp_i64_to_i32(abs_i32_to_i64(info.battery_current_now))
        };
        Self::power_watts(v, c)
    }

    fn power_watts(voltage: i32, current: i32) -> i32 {
        if voltage <= 0 || current <= 0 {
            return 0;
        }
        let voltage = voltage as u64;
        let current = current as u64;
        let divisor = match (voltage > 100_000, current > 100_000) {
            (true, true) => 1_000_000_000_000,
            (true, false) | (false, true) => 1_000_000_000,
            (false, false) => 1_000,
        };
        clamp_u64_to_i32(voltage.saturating_mul(current) / divisor)
    }

    fn normalize_power_value(power: i32) -> i32 {
        if power > 100_000 {
            power / 1_000_000
        } else if power > 500 {
            power / 1_000
        } else {
            power
        }
    }

    fn read_adapter_power_direct_w() -> i32 {
        let direct_pw = read_positive_int_any(ADAPTER_POWER_PATHS);
        if direct_pw > 0 {
            Self::normalize_power_value(direct_pw)
        } else {
            0
        }
    }

    fn current_quick_charge_type() -> String {
        read_string_any(QUICK_CHG_TYPE_PATHS)
    }

    fn is_quick_charge_type_4(value: &str) -> bool {
        value.trim() == "4" || value.to_ascii_lowercase().contains("super")
    }

    fn fast_charge_snapshot_from_info(info: &ChargerInfo) -> FastChargeSnapshot {
        let usb_type = if !info.usb_real_type.is_empty() {
            info.usb_real_type.clone()
        } else {
            info.usb_type.clone()
        };
        FastChargeSnapshot {
            online: info.usb_online != 0 || info.ac_online != 0 || info.wireless_online != 0,
            usb_online: info.usb_online,
            ac_online: info.ac_online,
            wireless_online: info.wireless_online,
            fast_type: info.fast_charge_type.trim().parse().unwrap_or(0),
            charge_tech: info.charge_technology.trim().parse().unwrap_or(0),
            quick_charge_type: info.quick_charge_type.clone(),
            pd_verified: info.pd_verified,
            cp_online: info.cp_online,
            fastchg_mode: info.fastchg_mode,
            sport_mode: info.sport_mode,
            adapter_power_w: info.adapter_power_w,
            usb_type,
            pc_port_online: info.pc_port_online,
        }
    }

    fn bool_like(value: &str) -> bool {
        matches!(
            value.trim(),
            "1" | "true" | "TRUE" | "True" | "enable" | "enabled" | "on"
        )
    }

    fn parse_bypass_switch(value: &str) -> bool {
        parse_keyed_int(value, "switch")
            .map(|v| v != 0)
            .unwrap_or_else(|| Self::bool_like(value))
    }

    fn parse_charge_limit_state_payload(value: &str) -> (i32, Option<i32>) {
        let ints = parse_plus_ints(value);
        if ints.len() >= 3 && ints[0] == 2 {
            (ints[1], Some(ints[2]))
        } else if ints.len() >= 2 {
            (ints[0], Some(ints[1]))
        } else {
            (parse_first_int(value).unwrap_or(0), None)
        }
    }

    fn parse_charge_limit_control_payload(value: &str) -> (i32, Option<i32>, i32, Option<i32>) {
        let ints = parse_plus_ints(value);
        if ints.len() >= 5 && ints[0] == 4 {
            (ints[1], Some(ints[2]), ints[3], Some(ints[4]))
        } else if ints.len() >= 4 {
            (ints[0], Some(ints[1]), ints[2], Some(ints[3]))
        } else {
            (parse_first_int(value).unwrap_or(0), None, 0, None)
        }
    }

    fn bypass_should_restrict(&self) -> bool {
        Self::parse_bypass_switch(&self.bypass_charge_status.lock())
    }

    fn charge_limit_switch_enabled(&self) -> bool {
        self.chg_up_limit_state.lock().trim() == "1"
    }

    fn charge_limit_target(&self) -> Option<i32> {
        self.chg_up_limit_value.lock().trim().parse::<i32>().ok()
    }

    fn charge_limit_latch_state(
        currently_active: bool,
        switch_enabled: bool,
        capacity: i32,
        limit: Option<i32>,
    ) -> bool {
        if !switch_enabled {
            return false;
        }
        let Some(limit) = limit else {
            return currently_active;
        };
        if currently_active {
            capacity > limit.saturating_sub(CHARGE_LIMIT_HYSTERESIS_PERCENT)
        } else {
            capacity >= limit
        }
    }

    fn update_charge_limit_latch(&self) -> bool {
        let currently_active = *self.charge_limit_active.lock();
        let next_active = Self::charge_limit_latch_state(
            currently_active,
            self.charge_limit_switch_enabled(),
            self.info.lock().battery_capacity,
            self.charge_limit_target(),
        );
        *self.charge_limit_active.lock() = next_active;
        next_active
    }

    fn desired_charge_control_active(&self) -> bool {
        self.bypass_should_restrict() || *self.charge_limit_active.lock()
    }

    fn enforce_charge_control(&self) {
        let _update_guard = self.charge_control_update_lock.lock();
        self.update_charge_limit_latch();
        self.set_charge_control_active(self.desired_charge_control_active());
    }

    fn next_synthetic_decimal_random(state: &mut DecimalSocState) -> u32 {
        if state.random == 0 {
            state.random = 0x6d2b_79f5;
        }
        state.random = state
            .random
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        state.random
    }

    fn normalize_decimal_seed_offset(raw: i32, capacity: i32) -> Option<i32> {
        if raw <= 0 {
            return None;
        }

        let capacity = capacity.clamp(0, 100);
        let base = capacity * 100;
        let normalized = if raw <= 100 {
            raw * 100
        } else if raw <= 10000 {
            raw
        } else {
            raw / 100
        };
        let offset = normalized - base;
        if (SYNTHETIC_DECIMAL_MIN_CENTI..=SYNTHETIC_DECIMAL_MAX_CENTI).contains(&offset) {
            Some(offset)
        } else {
            None
        }
    }

    fn next_synthetic_decimal_seed(
        state: &mut DecimalSocState,
        capacity: i32,
        cached_seed: i32,
    ) -> i32 {
        Self::normalize_decimal_seed_offset(cached_seed, capacity).unwrap_or_else(|| {
            SYNTHETIC_DECIMAL_MIN_CENTI
                + (Self::next_synthetic_decimal_random(state)
                    % ((SYNTHETIC_DECIMAL_SEED_MAX_CENTI - SYNTHETIC_DECIMAL_MIN_CENTI + 1) as u32))
                    as i32
        })
    }

    fn normalize_decimal_rate(rate: i32) -> i32 {
        if rate <= 0 {
            SYNTHETIC_DECIMAL_STEP_CENTI
        } else if rate > 10000 {
            ((rate / 100) / 4).clamp(
                SYNTHETIC_DECIMAL_STEP_CENTI,
                SYNTHETIC_DECIMAL_MAX_STEP_CENTI,
            )
        } else {
            (rate / 4).clamp(
                SYNTHETIC_DECIMAL_STEP_CENTI,
                SYNTHETIC_DECIMAL_MAX_STEP_CENTI,
            )
        }
    }

    fn synthetic_decimal_soc_pair(
        state: &mut DecimalSocState,
        capacity: i32,
        cached_seed: i32,
        cached_rate: i32,
    ) -> (i32, i32) {
        let capacity = capacity.clamp(0, 100);
        if capacity >= 100 {
            state.capacity = capacity;
            state.target = 10000;
            return (10000, 10000);
        }

        let base = capacity * 100;
        let min = base + SYNTHETIC_DECIMAL_MIN_CENTI;
        let max = base + SYNTHETIC_DECIMAL_MAX_CENTI;
        let step = Self::normalize_decimal_rate(cached_rate).min(max - min);

        if state.capacity != capacity {
            state.capacity = capacity;
            state.target = base + Self::next_synthetic_decimal_seed(state, capacity, cached_seed);
        }

        let start = state.target.clamp(min, max);
        let seed = Self::normalize_decimal_seed_offset(cached_seed, capacity)
            .map(|offset| base + offset)
            .unwrap_or(start);
        let end = (start + step).max(seed).clamp(start, max);
        state.target = end;
        (start, end)
    }

    fn decimal_soc_pair(&self, capacity: i32) -> (i32, i32) {
        let cached_seed = self.decimal_soc_seed.load(Ordering::Relaxed);
        let cached_rate = self.decimal_soc_rate.load(Ordering::Relaxed);
        let mut state = self.decimal_soc.lock();
        Self::synthetic_decimal_soc_pair(&mut state, capacity, cached_seed, cached_rate)
    }

    fn should_recheck_power(power_w: i32) -> bool {
        (POWER_RECHECK_MIN_W..=POWER_RECHECK_MAX_W).contains(&power_w)
    }

    fn stable_adapter_power_w_with<F, G, H>(
        current_power_w: i32,
        quick_charge_type: &str,
        mut read_quick_charge_type: F,
        mut read_power_w: G,
        mut wait: H,
    ) -> i32
    where
        F: FnMut() -> String,
        G: FnMut() -> i32,
        H: FnMut(),
    {
        if !Self::should_recheck_power(current_power_w) {
            return current_power_w;
        }
        let quick_charge_type = if quick_charge_type.trim().is_empty() {
            read_quick_charge_type()
        } else {
            quick_charge_type.to_string()
        };
        if !Self::is_quick_charge_type_4(&quick_charge_type) {
            return current_power_w;
        }
        let mut best_power_w = current_power_w;
        for _ in 0..3 {
            wait();
            best_power_w = best_power_w.max(read_power_w());
        }
        best_power_w
    }

    pub fn get_stable_adapter_power_w(&self) -> i32 {
        let snapshot = self.get_fast_charge_snapshot();
        if !snapshot.online {
            return 0;
        }
        if snapshot.adapter_power_w > 0 {
            snapshot.adapter_power_w
        } else {
            self.info.lock().adapter_power_w
        }
    }

    fn poll_interval(screen_on: bool, online: bool) -> Duration {
        if screen_on {
            SCREEN_ON_POLL_INTERVAL
        } else if online {
            SCREEN_OFF_CHARGING_POLL_INTERVAL
        } else {
            SCREEN_OFF_IDLE_POLL_INTERVAL
        }
    }

    fn power_source_probe_values_changed(
        snapshot: &FastChargeSnapshot,
        usb_online: Option<i32>,
        ac_online: Option<i32>,
        wireless_online: Option<i32>,
        pc_port_online: Option<i32>,
        quick_charge_type: Option<&str>,
        usb_type: Option<&str>,
    ) -> bool {
        usb_online.is_some_and(|value| value != snapshot.usb_online)
            || ac_online.is_some_and(|value| value != snapshot.ac_online)
            || wireless_online.is_some_and(|value| value != snapshot.wireless_online)
            || pc_port_online.is_some_and(|value| value != snapshot.pc_port_online)
            || quick_charge_type.is_some_and(|value| value != snapshot.quick_charge_type)
            || usb_type.is_some_and(|value| value != snapshot.usb_type)
    }

    fn power_source_probe_changed(snapshot: &FastChargeSnapshot) -> bool {
        let usb_online_path = format!("{}/online", PSY_USB);
        let ac_online_path = format!("{}/online", PSY_AC);
        let wireless_online_path = format!("{}/online", PSY_WIRELESS);
        let dc_online_path = format!("{}/online", PSY_DC);
        let quick_charge_type = read_string_any(QUICK_CHG_TYPE_PATHS);
        let usb_type = read_string_any(QCOM_REAL_TYPE_PATHS);
        Self::power_source_probe_values_changed(
            snapshot,
            try_read_int(&usb_online_path),
            try_read_int(&ac_online_path),
            try_read_int_any(&[&wireless_online_path, &dc_online_path]),
            try_read_int_any(PC_PORT_ONLINE_PATHS),
            (!quick_charge_type.is_empty()).then_some(quick_charge_type.as_str()),
            (!usb_type.is_empty()).then_some(usb_type.as_str()),
        )
    }

    fn request_refresh(&self) {
        self.refresh_pending.store(true, Ordering::Relaxed);
        self.wake_poll_worker();
    }

    #[cfg(target_os = "android")]
    fn request_uevent_probe(&self) {
        self.uevent_probe_pending.store(true, Ordering::Relaxed);
        self.wake_poll_worker();
    }

    fn wake_poll_worker(&self) {
        let _ = self.poll_wake_tx.try_send(());
    }

    pub fn get_fast_charge_snapshot(&self) -> FastChargeSnapshot {
        self.fast_charge_snapshot.lock().clone()
    }

    pub fn notify_screen_status(&self, status: i32) {
        let screen_on = status != 0;
        let previous = self.screen_on.swap(screen_on, Ordering::Relaxed);
        if previous != screen_on {
            self.screen_wake_pending.store(true, Ordering::Release);
        }
    }

    // ── Best-value helpers ──

    fn best_soh(info: &ChargerInfo) -> i32 {
        if info.fg_soh > 0 && info.fg_soh <= 100 {
            info.fg_soh
        } else if info.charge_full > 0 && info.charge_full_design > 0 {
            ((info.charge_full as f64 / info.charge_full_design as f64) * 100.0).clamp(0.0, 100.0)
                as i32
        } else {
            0
        }
    }
    fn best_fcc(info: &ChargerInfo) -> i32 {
        if info.fg_fcc > 0 {
            info.fg_fcc
        } else {
            info.charge_full
        }
    }
    fn best_fcc_mah(info: &ChargerInfo) -> i32 {
        normalize_capacity_mah(Self::best_fcc(info))
    }
    fn best_design_capacity_mah(info: &ChargerInfo) -> i32 {
        normalize_capacity_mah(info.charge_full_design)
    }
    fn best_qmax_mah(info: &ChargerInfo) -> i32 {
        normalize_capacity_mah(info.fg_qmax)
    }
    fn best_rm(info: &ChargerInfo) -> i32 {
        if info.fg_rm > 0 {
            info.fg_rm
        } else if info.fg_fcc > 0 && info.fg_rsoc > 0 {
            let denominator = if info.fg_rsoc > 100 { 10000 } else { 100 };
            clamp_i64_to_i32((info.fg_fcc as i64) * (info.fg_rsoc as i64) / denominator)
        } else {
            clamp_i64_to_i32((info.charge_full as i64) * (info.battery_capacity as i64) / 100)
        }
    }
    fn best_rm_mah(info: &ChargerInfo) -> i32 {
        normalize_capacity_mah(Self::best_rm(info))
    }
    fn best_charge_counter_mah(info: &ChargerInfo) -> i32 {
        normalize_capacity_mah(info.charge_counter)
    }
    fn best_cycle(info: &ChargerInfo) -> i32 {
        if info.fg_cycle > 0 {
            info.fg_cycle
        } else {
            info.cycle_count
        }
    }
    fn best_battery_type(info: &ChargerInfo) -> String {
        if !info.battery_type.is_empty() {
            info.battery_type.clone()
        } else if !info.battery_technology.is_empty() {
            info.battery_technology.clone()
        } else {
            "unknown".into()
        }
    }
    fn is_silicon_battery(info: &ChargerInfo) -> bool {
        let battery_type = Self::best_battery_type(info).to_lowercase();
        battery_type.contains("silicon")
            || battery_type.contains("si-c")
            || battery_type.contains("sic")
    }

    // ── String builders (ColorOS format) ──

    fn build_charger_info_json(info: &ChargerInfo) -> String {
        let temp_c = info.battery_temp as f64 / 10.0;
        let voltage_v = info.battery_voltage_now as f64 / 1_000_000.0;
        let current_ma = info.battery_current_now as f64 / 1000.0;
        let soh = Self::best_soh(info);
        let fcc_mah = Self::best_fcc_mah(info);
        let rm_mah = Self::best_rm_mah(info);
        let design_mah = Self::best_design_capacity_mah(info);
        let qmax_mah = Self::best_qmax_mah(info);
        let battery_type = Self::best_battery_type(info);
        format!(
            "usb_online={};usb_type={};usb_real_type={};ac_online={};pc_port={};batt_status={};batt_health={};capacity={};temp={:.1};current={:.1};voltage={:.3};charge_type={};technology={};battery_type={};soh={};fast_chg_type={};charge_tech={};wireless={};wireless_type={};input_current={};charge_full={};fcc={};rm={};design_capacity={};qmax={};adapter_power={};cp_online={};cp_iin_m={};cp_iin_s={};pd_verified={};quick_charge_type={};die_temp={};typec_mode={};cc_orientation={};remaining_time={};smart_chg={};night_chg={};sport={};input_suspend={};moisture={};cell1_v={};cell2_v={};fg_qmax={};fg_ai={};fg_avg_cur={};fg_vendor={};authentic={};max_life_t={};max_life_v={};board_temp={};flash={};hifi={};fake_soc={};fake_soh={}",
            info.usb_online, info.usb_type, info.usb_real_type, info.ac_online, info.pc_port_online,
            info.battery_status, info.battery_health, info.battery_capacity,
            temp_c, current_ma, voltage_v, info.battery_charge_type,
            info.battery_technology, battery_type, soh, info.fast_charge_type, info.charge_technology,
            info.wireless_online, info.wireless_type,
            info.input_current_max, fcc_mah, fcc_mah, rm_mah, design_mah, qmax_mah,
            info.adapter_power_w, info.cp_online, info.cp_master_iin, info.cp_slave_iin,
            info.pd_verified, info.quick_charge_type,
            info.die_temperature, info.typec_mode, info.cc_orientation,
            info.remaining_time, info.smart_chg, info.night_charging, info.sport_mode,
            info.input_suspend, info.moisture_detected as i32,
            info.cell1_vol, info.cell2_vol,
            info.fg_qmax, info.fg_ai, info.fg_avg_current, info.fg_vendor,
            info.authentic, info.max_life_temp, info.max_life_vol,
            info.thermal_board_temp,
            info.flash_active as i32, info.hifi_connect as i32,
            info.fake_soc, info.fake_soh
        )
    }

    fn build_soh_debug_info(info: &ChargerInfo) -> String {
        let soh = Self::best_soh(info);
        let cycle = Self::best_cycle(info);
        let fcc_mah = Self::best_fcc_mah(info);
        let rm_mah = Self::best_rm_mah(info);
        let design_mah = Self::best_design_capacity_mah(info);
        let qmax_mah = Self::best_qmax_mah(info);
        let battery_type = Self::best_battery_type(info);
        let gauge_type = if info.gauge_type.is_empty() {
            "unknown"
        } else {
            &info.gauge_type
        };
        format!(
            "soh={};charge_full={};charge_full_design={};charge_counter={};fcc={};rm={};design_capacity={};qmax={};cycle={};battery_type={};gauge_type={};capacity={};temp={};status={};fg_soh={};fg_fcc={};fg_rm={};fg_rsoc={};fg_cycle={};fg_ai={};fg_qmax={};fg_vendor={};gauge_info={};die_temp={};remaining_time={};max_life_temp={};max_life_vol={};over_vol_dur={}",
            soh, fcc_mah, design_mah, Self::best_charge_counter_mah(info), fcc_mah, rm_mah, design_mah, qmax_mah,
            cycle, battery_type, gauge_type, info.battery_capacity, info.battery_temp, info.battery_status,
            info.fg_soh, info.fg_fcc, info.fg_rm, info.fg_rsoc, info.fg_cycle,
            info.fg_ai, info.fg_qmax, info.fg_vendor, info.gauge_info,
            info.die_temperature, info.remaining_time,
            info.max_life_temp, info.max_life_vol, info.over_vol_duration
        )
    }

    fn build_balance_info(info: &ChargerInfo) -> String {
        let diff = clamp_i64_to_i32(((info.cell1_vol as i64) - (info.cell2_vol as i64)).abs());
        format!(
            "{},{},{},{},{},{},{},{}",
            info.cell1_vol,
            info.cell2_vol,
            diff,
            info.cell1_rascale,
            info.fg_avg_current,
            info.fg_qmax,
            info.fg_ai,
            if info.fg_vendor.is_empty() { "0" } else { "1" }
        )
    }

    // ── Public accessors ──

    pub fn get_usb_online(&self) -> i32 {
        self.fast_charge_snapshot.lock().usb_online
    }
    pub fn get_usb_status(&self) -> String {
        let info = self.info.lock();
        if info.usb_online == 0 && info.ac_online == 0 && info.wireless_online == 0 {
            return "0,0,0,0,0,0".into();
        }
        let usb_ty = if !info.usb_real_type.is_empty() {
            &info.usb_real_type
        } else {
            &info.usb_type
        };
        let type_code = match usb_ty.as_str() {
            "USB" | "SDP" => "1",
            "USB_CDP" | "CDP" => "2",
            "USB_DCP" | "DCP" => "3",
            "USB_HVDCP" | "HVDCP" => "4",
            "USB_PD" | "PD" | "PD_ACTIVE" => "5",
            "USB_HVDCP_3" | "HVDCP_3" => "6",
            _ => {
                let tl = usb_ty.to_lowercase();
                if tl.contains("pd") {
                    "5"
                } else if tl.contains("hvdcp") {
                    "4"
                } else if tl.contains("dcp") {
                    "3"
                } else if tl.contains("cdp") {
                    "2"
                } else {
                    "0"
                }
            }
        };
        format!(
            "{},{},{},{},{},{}",
            info.usb_online,
            type_code,
            info.input_current_max,
            info.battery_voltage_now,
            info.battery_current_now,
            info.battery_temp
        )
    }
    pub fn get_ac_online(&self) -> i32 {
        self.fast_charge_snapshot.lock().ac_online
    }
    pub fn get_wireless_online(&self) -> i32 {
        self.fast_charge_snapshot.lock().wireless_online
    }
    pub fn get_pc_port_online(&self) -> i32 {
        self.info.lock().pc_port_online
    }
    pub fn get_battery_temp(&self) -> i32 {
        self.info.lock().battery_temp
    }
    pub fn get_battery_current_now(&self) -> i32 {
        self.info.lock().battery_current_now
    }
    pub fn get_average_current(&self) -> i32 {
        let info = self.info.lock();
        if info.fg_avg_current != 0 {
            clamp_i64_to_i32(abs_i32_to_i64(info.fg_avg_current))
        } else {
            clamp_i64_to_i32(abs_i32_to_i64(info.battery_current_now))
        }
    }
    pub fn get_battery_status(&self) -> String {
        let info = self.info.lock();
        let snapshot = self.get_fast_charge_snapshot();
        let fcc = Self::best_fcc_mah(&info);
        let design_capacity = Self::best_design_capacity_mah(&info);
        let charge_counter = Self::best_charge_counter_mah(&info);
        let rm = Self::best_rm_mah(&info);
        let qmax = Self::best_qmax_mah(&info);
        let battery_type = Self::best_battery_type(&info);
        let status = if *self.charge_control_active.lock()
            && (snapshot.usb_online != 0 || snapshot.ac_online != 0)
        {
            "Charging"
        } else if !snapshot.online && info.battery_status.eq_ignore_ascii_case("Charging") {
            "Discharging"
        } else {
            &info.battery_status
        };
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            status,
            info.battery_capacity,
            info.battery_voltage_now,
            info.battery_current_now,
            info.battery_temp,
            info.battery_health,
            info.battery_technology,
            battery_type,
            info.battery_charge_type,
            fcc,
            design_capacity,
            charge_counter,
            rm,
            qmax,
            info.fast_charge_type,
            info.input_current_max,
            info.authentic
        )
    }
    pub fn get_battery_rm(&self) -> i32 {
        Self::best_rm_mah(&self.info.lock())
    }
    pub fn get_battery_fcc(&self) -> i32 {
        Self::best_fcc_mah(&self.info.lock())
    }
    pub fn get_battery_cc(&self) -> i32 {
        Self::best_charge_counter_mah(&self.info.lock())
    }
    pub fn get_battery_design_capacity(&self) -> i32 {
        Self::best_design_capacity_mah(&self.info.lock())
    }
    pub fn get_battery_qmax(&self) -> i32 {
        Self::best_qmax_mah(&self.info.lock())
    }
    pub fn get_battery_type(&self) -> String {
        Self::best_battery_type(&self.info.lock())
    }
    pub fn get_battery_gauge_type(&self) -> String {
        let info = self.info.lock();
        if !info.gauge_type.is_empty() {
            info.gauge_type.clone()
        } else {
            Self::best_battery_type(&info)
        }
    }
    pub fn is_silicon_battery_now(&self) -> bool {
        Self::is_silicon_battery(&self.info.lock())
    }
    pub fn get_battery_soh(&self) -> i32 {
        Self::best_soh(&self.info.lock())
    }
    pub fn get_battery_cycle_count(&self) -> i32 {
        Self::best_cycle(&self.info.lock())
    }
    pub fn get_decimal_soc(&self) -> String {
        if !self.get_fast_charge_snapshot().online {
            return "0,0".into();
        }
        let capacity = self
            .battery_capacity_cache
            .load(Ordering::Relaxed)
            .clamp(0, 100);
        let (start, end) = self.decimal_soc_pair(capacity);
        format!("{},{}", start, end)
    }
    pub fn get_adapter_power_w(&self) -> i32 {
        self.get_stable_adapter_power_w()
    }
    pub fn get_charge_limit_value(&self) -> String {
        self.chg_up_limit_value.lock().clone()
    }
    pub fn set_charge_limit_value(&self, value: &str) {
        let _update_guard = self.charge_control_update_lock.lock();
        *self.charge_control_applied.lock() = None;
        let (stop_charging, limit, _force, _recharge) =
            Self::parse_charge_limit_control_payload(value);
        if let Some(limit) = limit {
            *self.chg_up_limit_value.lock() = limit.to_string();
        } else if let Some(limit) = parse_first_int(value) {
            *self.chg_up_limit_value.lock() = limit.to_string();
        } else {
            *self.chg_up_limit_value.lock() = value.to_string();
        }
        if stop_charging != 0 {
            *self.charge_limit_active.lock() = true;
        } else if !self.charge_limit_switch_enabled()
            || !Self::charge_limit_latch_state(
                true,
                true,
                self.info.lock().battery_capacity,
                self.charge_limit_target(),
            )
        {
            *self.charge_limit_active.lock() = false;
        }
        self.set_charge_control_active(self.desired_charge_control_active());
    }
    pub fn get_charge_limit_state(&self) -> String {
        self.chg_up_limit_state.lock().clone()
    }
    pub fn set_charge_limit_state(&self, value: &str) {
        let _update_guard = self.charge_control_update_lock.lock();
        *self.charge_control_applied.lock() = None;
        let (enabled, limit) = Self::parse_charge_limit_state_payload(value);
        *self.chg_up_limit_state.lock() = enabled.to_string();
        if let Some(limit) = limit {
            *self.chg_up_limit_value.lock() = limit.to_string();
            write_string_any(CHARGE_STOP_THRESHOLD_PATHS, &limit.to_string());
        }
        if enabled == 0 {
            *self.charge_limit_active.lock() = false;
        } else {
            self.update_charge_limit_latch();
        }
        write_string_any(CHARGE_LIMIT_STATE_PATHS, &enabled.to_string());
        self.set_charge_control_active(self.desired_charge_control_active());
    }
    pub fn get_bypass_charge_status(&self) -> String {
        self.bypass_charge_status.lock().clone()
    }
    pub fn set_bypass_charge_status(&self, value: &str) {
        let _update_guard = self.charge_control_update_lock.lock();
        *self.charge_control_applied.lock() = None;
        let enabled = Self::parse_bypass_switch(value);
        let normalized = if enabled { "1" } else { "0" };
        *self.bypass_charge_status.lock() = normalized.into();
        self.set_charge_control_active(self.desired_charge_control_active());
        write_string_any(BYPASS_STATUS_PATHS, normalized);
    }
    fn set_charge_control_active(&self, restrict: bool) {
        let mut applied = self.charge_control_applied.lock();
        *self.charge_control_active.lock() = restrict;
        if *applied != Some(restrict) {
            self.apply_charge_control_limit(restrict);
            *applied = Some(restrict);
        }
    }
    fn apply_charge_control_limit(&self, restrict: bool) {
        let restricted_value;
        let value = if restrict {
            restricted_value =
                restricted_charge_control_value(read_int_any(CHARGE_CONTROL_LIMIT_MAX_PATHS));
            restricted_value.as_str()
        } else {
            CHARGE_CONTROL_LIMIT_RELEASED
        };
        write_string_any(CHARGE_CONTROL_LIMIT_PATHS, value);
        write_string_any(COOL_MODE_PATHS, if restrict { "1" } else { "0" });
        write_string_any(COOL_DOWN_PATHS, if restrict { "1" } else { "0" });
    }
    pub fn get_battery_authenticate(&self) -> i32 {
        1
    }
    pub fn get_battery_short_status(&self) -> i32 {
        SHORT_CIRCUIT_HEALTHY
    }
    pub fn get_battery_short_ic_otp_status(&self) -> i32 {
        SHORT_CIRCUIT_HEALTHY
    }
    pub fn get_battery_short_feature(&self) -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{Adapter, ChargerInfo, FastChargeInputs};
    use std::sync::{mpsc::sync_channel, Arc};
    use std::time::Duration;

    fn fast_inputs<'a>(
        quick_charge_type: &'a str,
        pd_verified: i32,
        cp_online: i32,
        adapter_power_w: i32,
        online: bool,
    ) -> FastChargeInputs<'a> {
        FastChargeInputs {
            quick_charge_type,
            fastchg_mode: 0,
            sport_mode: 0,
            pd_verified,
            cp_online,
            usb_type: "",
            adapter_power_w,
            online,
            usb_online: online as i32,
            pc_port_online: 0,
        }
    }

    #[test]
    fn screen_notification_never_waits_for_poll_or_control_locks() {
        let adapter = Adapter::new();
        let _info_guard = adapter.info.lock();
        let _charger_info_guard = adapter.charger_info_json.lock();
        let _fast_snapshot_guard = adapter.fast_charge_snapshot.lock();
        let _decimal_soc_guard = adapter.decimal_soc.lock();
        let _charge_control_guard = adapter.charge_control_update_lock.lock();
        let (done_tx, done_rx) = sync_channel(1);
        let notify_adapter = Arc::clone(&adapter);

        std::thread::spawn(move || {
            notify_adapter.notify_screen_status(1);
            let _ = done_tx.try_send(());
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(250)).is_ok());
    }

    #[test]
    fn qct4_rechecks_33w_and_returns_highest_power() {
        let mut powers = [33, 33, 67, 65, 67].into_iter();
        let power = Adapter::stable_adapter_power_w_with(
            33,
            "4",
            || "4".into(),
            || powers.next().unwrap_or(33),
            || {},
        );
        assert_eq!(power, 67);
    }

    #[test]
    fn non_qct4_keeps_33w_without_power_reads() {
        let mut reads = 0;
        let power = Adapter::stable_adapter_power_w_with(
            33,
            "3",
            || "3".into(),
            || {
                reads += 1;
                67
            },
            || {},
        );
        assert_eq!(power, 33);
        assert_eq!(reads, 0);
    }

    #[test]
    fn non_33w_power_does_not_recheck() {
        let mut reads = 0;
        let power = Adapter::stable_adapter_power_w_with(
            67,
            "4",
            || "4".into(),
            || {
                reads += 1;
                33
            },
            || {},
        );
        assert_eq!(power, 67);
        assert_eq!(reads, 0);
    }

    #[test]
    fn direct_fast_classifier_detects_qct4_immediately() {
        assert_eq!(
            Adapter::classify_fast_charge_values(fast_inputs("4", 0, 0, 0, true)),
            3
        );
        assert_eq!(Adapter::classify_charge_technology_values("4", 3), 3);
        assert_eq!(
            Adapter::classify_fast_charge_values(fast_inputs("4", 1, 1, 67, false)),
            0
        );
    }

    #[test]
    fn direct_fast_classifier_uses_pd_cp_and_power_fallbacks() {
        assert_eq!(
            Adapter::classify_fast_charge_values(fast_inputs("", 1, 0, 0, true)),
            3
        );
        assert_eq!(
            Adapter::classify_fast_charge_values(fast_inputs("", 0, 1, 0, true)),
            3
        );
        assert_eq!(
            Adapter::classify_fast_charge_values(fast_inputs("", 0, 0, 21, true)),
            3
        );
        assert_eq!(
            Adapter::classify_fast_charge_values(fast_inputs("0", 1, 0, 0, true)),
            3
        );
        assert_eq!(Adapter::classify_charge_technology_values("0", 3), 3);
    }

    #[test]
    fn fast_snapshot_offline_suppresses_stale_fast_fields() {
        let snapshot = super::FastChargeSnapshot {
            online: false,
            usb_online: 0,
            ac_online: 0,
            wireless_online: 0,
            fast_type: 3,
            charge_tech: 3,
            quick_charge_type: "4".into(),
            pd_verified: 1,
            cp_online: 1,
            fastchg_mode: 1,
            sport_mode: 1,
            adapter_power_w: 67,
            usb_type: "USB_PD".into(),
            pc_port_online: 0,
        };

        assert!(!snapshot.is_fast_charge());
        assert!(!snapshot.is_svooc_active());
        assert!(!snapshot.is_pps_active());
    }

    #[test]
    fn computer_usb_overrides_all_stale_fast_charge_evidence() {
        let inputs = FastChargeInputs {
            quick_charge_type: "4",
            fastchg_mode: 1,
            sport_mode: 1,
            pd_verified: 1,
            cp_online: 1,
            usb_type: "USB_SDP",
            adapter_power_w: 67,
            online: true,
            usb_online: 1,
            pc_port_online: 1,
        };
        assert_eq!(Adapter::classify_fast_charge_values(inputs), 0);

        let snapshot = super::FastChargeSnapshot {
            online: true,
            usb_online: 1,
            ac_online: 0,
            wireless_online: 0,
            fast_type: 3,
            charge_tech: 3,
            quick_charge_type: "4".into(),
            pd_verified: 1,
            cp_online: 1,
            fastchg_mode: 1,
            sport_mode: 1,
            adapter_power_w: 67,
            usb_type: "USB_SDP".into(),
            pc_port_online: 1,
        };
        assert!(!snapshot.is_fast_charge());
        assert!(!snapshot.is_svooc_active());
        assert!(!snapshot.is_pps_active());
        assert!(!snapshot.should_show_power());
    }

    #[test]
    fn disconnect_clears_the_entire_fast_charge_session() {
        let mut info = ChargerInfo {
            usb_type: "USB_PD".into(),
            usb_real_type: "USB_PD".into(),
            quick_charge_type: "4".into(),
            pd_verified: 1,
            cp_online: 1,
            cp_status: "Charging".into(),
            cp_bus_voltage: 20_000_000,
            cp_bus_current: 3_000_000,
            cp_master_iin: 1_500_000,
            cp_slave_iin: 1_500_000,
            fastchg_mode: 1,
            sport_mode: 1,
            adapter_power_w: 67,
            fast_charge_type: "3".into(),
            charge_technology: "3".into(),
            ..Default::default()
        };
        Adapter::clear_fast_charge_session(&mut info);
        assert!(info.usb_type.is_empty());
        assert!(info.usb_real_type.is_empty());
        assert!(info.quick_charge_type.is_empty());
        assert_eq!(info.pd_verified, 0);
        assert_eq!(info.cp_online, 0);
        assert_eq!(info.adapter_power_w, 0);
        assert_eq!(info.fast_charge_type, "0");
        assert_eq!(info.charge_technology, "0");
    }

    #[test]
    fn estimates_remaining_time_from_capacity_and_current() {
        let info = super::ChargerInfo {
            usb_online: 1,
            battery_status: "Charging".into(),
            battery_capacity: 50,
            fg_fcc: 4000,
            fg_rm: 2000,
            battery_current_now: -2_000_000,
            ..Default::default()
        };

        assert_eq!(Adapter::estimate_remaining_time_seconds(&info), 3600);
    }

    #[test]
    fn normalizes_remaining_time_node_units() {
        let info = super::ChargerInfo {
            usb_online: 1,
            remaining_time: 3_600_000,
            ..Default::default()
        };

        assert_eq!(Adapter::estimate_remaining_time_seconds(&info), 3600);
    }

    #[test]
    fn bypass_bool_parser_accepts_common_enabled_values() {
        assert!(Adapter::bool_like("1"));
        assert!(Adapter::bool_like("enable"));
        assert!(Adapter::bool_like("on"));
        assert!(!Adapter::bool_like("0"));
        assert!(!Adapter::bool_like("disable"));
    }

    #[test]
    fn parses_coloros_bypass_switch_payload() {
        assert!(Adapter::parse_bypass_switch("1+switch=1"));
        assert!(!Adapter::parse_bypass_switch("1+switch=0"));
    }

    #[test]
    fn parses_coloros_charge_limit_state_payload() {
        assert_eq!(
            Adapter::parse_charge_limit_state_payload("2++1+80"),
            (1, Some(80))
        );
        assert_eq!(
            Adapter::parse_charge_limit_state_payload("2++0+90"),
            (0, Some(90))
        );
    }

    #[test]
    fn parses_coloros_charge_limit_control_payload() {
        assert_eq!(
            Adapter::parse_charge_limit_control_payload("4++1+80+1+80"),
            (1, Some(80), 1, Some(80))
        );
        assert_eq!(
            Adapter::parse_charge_limit_control_payload("4++0+80+0+80"),
            (0, Some(80), 0, Some(80))
        );
    }

    #[test]
    fn charge_limit_latch_holds_at_boundary() {
        assert!(Adapter::charge_limit_latch_state(false, true, 91, Some(90)));
        assert!(Adapter::charge_limit_latch_state(false, true, 95, Some(95)));
        assert!(Adapter::charge_limit_latch_state(true, true, 95, Some(95)));
        assert!(Adapter::charge_limit_latch_state(true, true, 90, Some(90)));
        assert!(!Adapter::charge_limit_latch_state(true, true, 89, Some(90)));
        assert!(!Adapter::charge_limit_latch_state(
            true,
            false,
            95,
            Some(95)
        ));
    }

    #[test]
    fn normalizes_capacity_without_decimal_pollution() {
        assert_eq!(super::normalize_battery_capacity(89), 89);
        assert_eq!(super::normalize_battery_capacity(8999), 89);
        assert_eq!(super::normalize_battery_capacity(10000), 100);
    }

    #[test]
    fn integer_fallback_skips_unreadable_or_invalid_nodes() {
        let directory =
            std::env::temp_dir().join(format!("chargerhal-read-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let invalid = directory.join("invalid");
        let valid = directory.join("valid");
        std::fs::write(&invalid, "not-a-number\n").unwrap();
        std::fs::write(&valid, "67\n").unwrap();

        assert_eq!(
            super::read_int_any(&[invalid.to_str().unwrap(), valid.to_str().unwrap()]),
            67
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transient_sysfs_failures_preserve_cached_values() {
        let directory =
            std::env::temp_dir().join(format!("chargerhal-cache-preserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let missing = directory.join("missing");
        let value_path = directory.join("value");

        let mut integer = 42;
        super::update_int_from_paths(&mut integer, &[missing.to_str().unwrap()]);
        assert_eq!(integer, 42);

        std::fs::write(&value_path, "0\n").unwrap();
        super::update_int_from_paths(&mut integer, &[value_path.to_str().unwrap()]);
        assert_eq!(integer, 0);

        let mut text = "cached".to_string();
        super::update_non_empty_string_from_paths(&mut text, &[missing.to_str().unwrap()]);
        assert_eq!(text, "cached");

        std::fs::write(&value_path, "Charging\n").unwrap();
        super::update_non_empty_string_from_paths(&mut text, &[value_path.to_str().unwrap()]);
        assert_eq!(text, "Charging");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zero_percent_decimal_soc_stays_near_zero() {
        let mut state = super::DecimalSocState::default();
        let (start, end) = Adapter::synthetic_decimal_soc_pair(&mut state, 0, 0, 1);

        assert!((5..=50).contains(&start));
        assert!((5..=85).contains(&end));
    }

    #[test]
    fn polling_slows_down_without_screen_or_charger() {
        assert_eq!(
            Adapter::poll_interval(true, false),
            super::SCREEN_ON_POLL_INTERVAL
        );
        assert_eq!(
            Adapter::poll_interval(false, true),
            super::SCREEN_OFF_CHARGING_POLL_INTERVAL
        );
        assert_eq!(
            Adapter::poll_interval(false, false),
            super::SCREEN_OFF_IDLE_POLL_INTERVAL
        );
    }

    #[test]
    fn lightweight_probe_only_escalates_on_power_source_changes() {
        let snapshot = super::FastChargeSnapshot {
            usb_online: 1,
            ac_online: 0,
            wireless_online: 0,
            pc_port_online: 0,
            quick_charge_type: "4".into(),
            usb_type: "USB_PD".into(),
            ..Default::default()
        };

        assert!(!Adapter::power_source_probe_values_changed(
            &snapshot,
            Some(1),
            Some(0),
            Some(0),
            Some(0),
            Some("4"),
            Some("USB_PD"),
        ));
        assert!(Adapter::power_source_probe_values_changed(
            &snapshot,
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some("4"),
            Some("USB_PD"),
        ));
        assert!(Adapter::power_source_probe_values_changed(
            &snapshot,
            None,
            None,
            None,
            None,
            Some("0"),
            None,
        ));
    }

    #[test]
    fn full_scan_honors_screen_transition_cancellation() {
        let checks = std::cell::Cell::new(0);
        let mut info = ChargerInfo::default();
        let completed = Adapter::poll_once(&mut info, || {
            let next = checks.get() + 1;
            checks.set(next);
            next >= 2
        });

        assert!(!completed);
        assert!(checks.get() >= 2);
    }

    #[test]
    fn unknown_xiaomi_metrics_are_not_fabricated() {
        let info = ChargerInfo::default();
        assert_eq!(info.battery_capacity, 0);
        assert_eq!(super::SHORT_CIRCUIT_HEALTHY, 1);
        assert_eq!(Adapter::best_soh(&info), 0);
        assert_eq!(Adapter::best_fcc_mah(&info), 0);
        assert_eq!(info.authentic, 1);
    }

    #[test]
    fn detects_only_power_supply_uevents() {
        assert!(super::is_power_supply_uevent(
            b"change@/devices/virtual/power_supply/battery\0ACTION=change\0SUBSYSTEM=power_supply\0"
        ));
        assert!(!super::is_power_supply_uevent(
            b"change@/devices/platform/display\0ACTION=change\0SUBSYSTEM=graphics\0"
        ));
        assert!(!super::is_power_supply_uevent(
            b"SUBSYSTEM=power_supply_extra\0"
        ));
    }

    #[test]
    fn synthetic_decimal_pair_progresses_without_decimal_node() {
        let mut state = super::DecimalSocState::default();
        let (first_start, first_end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 0, 1);
        let (second_start, second_end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 0, 1);

        assert!((8905..=8950).contains(&first_start));
        assert!((8905..=8985).contains(&first_end));
        assert_ne!(first_end, first_start);
        assert_eq!(second_start, first_end);
        assert!((8905..=8985).contains(&second_end));
        assert_ne!(second_end, second_start);
    }

    #[test]
    fn synthetic_decimal_pair_uses_node_seed_on_capacity_reset() {
        let mut state = super::DecimalSocState::default();
        let (first_start, first_end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8942, 1);
        let (second_start, second_end) =
            Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8901, 1);

        assert_eq!(first_start, 8942);
        assert!((8905..=8985).contains(&first_end));
        assert_ne!(first_end, first_start);
        assert_eq!(second_start, first_end);
        assert!((8905..=8985).contains(&second_end));
        assert_ne!(second_end, second_start);
    }

    #[test]
    fn synthetic_decimal_pair_does_not_regress_after_reaching_cap() {
        let mut state = super::DecimalSocState::default();
        let mut previous_end = 0;

        for _ in 0..40 {
            let pair = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8942, 1);
            assert!((8905..=8985).contains(&pair.0));
            assert!((8905..=8985).contains(&pair.1));
            assert_ne!(pair.1, pair.0);
            assert!(pair.1 >= previous_end);
            previous_end = pair.1;
        }
    }

    #[test]
    fn synthetic_decimal_pair_does_not_regress_when_rate_increases() {
        let mut state = super::DecimalSocState::default();
        let (_, first_end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8980, 1);
        let (_, second_end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8980, 20);

        assert!(second_end >= first_end);
    }

    #[test]
    fn synthetic_decimal_pair_continues_past_eighty_five_centi() {
        let mut state = super::DecimalSocState::default();
        let (_, end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8984, 20);

        assert!(end > 8985);
        assert!(end <= 8999);
    }

    #[test]
    fn synthetic_decimal_pair_uses_rate_as_step() {
        let mut state = super::DecimalSocState::default();
        let (start, end) = Adapter::synthetic_decimal_soc_pair(&mut state, 89, 8942, 20);
        assert_eq!(start, 8942);
        assert_eq!(end, 8945);
    }

    #[test]
    fn restricted_charge_control_uses_max_minus_one() {
        assert_eq!(super::restricted_charge_control_value(16), "15");
        assert_eq!(super::restricted_charge_control_value(2), "1");
        assert_eq!(super::restricted_charge_control_value(0), "15");
    }
}
