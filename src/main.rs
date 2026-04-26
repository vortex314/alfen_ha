#![recursion_limit = "512"]

use anyhow::{Context, Result};
use byteorder::{BigEndian, ByteOrder};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time;
use tokio_modbus::prelude::*;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    alfen: AlfenConfig,
    mqtt: MqttConfig,
    charger: ChargerConfig,
}

#[derive(Debug, Deserialize)]
struct AlfenConfig {
    host: String,
    port: u16,
    unit_id: u8,
    #[serde(default)]
    socket_unit_id: Option<u8>,
    #[serde(default)]
    station_unit_id: Option<u8>,
    #[serde(default)]
    control_unit_id: Option<u8>,
    #[serde(default)]
    takeover_sequence: Option<bool>,
    #[serde(default)]
    takeover_safe_current_amps: Option<f32>,
    #[serde(default)]
    takeover_safe_current_register: Option<u16>,
    #[serde(default)]
    takeover_validity_secs: Option<u16>,
    #[serde(default)]
    takeover_validity_register: Option<u16>,
    poll_interval_secs: u64,
    #[serde(default)]
    setpoint_heartbeat_secs: Option<u64>,
}

impl AlfenConfig {
    fn socket_slave(&self) -> u8 {
        self.socket_unit_id.unwrap_or(self.unit_id)
    }

    fn station_slave(&self) -> u8 {
        self.station_unit_id.unwrap_or_else(|| self.socket_slave())
    }

    fn control_slave(&self) -> u8 {
        // Control/status registers in the 1200-range are on the socket slave on this setup.
        self.control_unit_id.unwrap_or_else(|| self.socket_slave())
    }

    fn takeover_enabled(&self) -> bool {
        self.takeover_sequence.unwrap_or(true)
    }

    fn takeover_safe_current_amps(&self) -> f32 {
        self.takeover_safe_current_amps.unwrap_or(6.0)
    }

    fn takeover_safe_current_register(&self) -> Option<u16> {
        self.takeover_safe_current_register
    }

    fn takeover_validity_secs(&self) -> u16 {
        self.takeover_validity_secs.unwrap_or(120)
    }

    fn takeover_validity_register(&self) -> Option<u16> {
        self.takeover_validity_register
    }
}

#[derive(Debug, Deserialize)]
struct MqttConfig {
    host: String,
    port: u16,
    client_id: String,
    username: Option<String>,
    password: Option<String>,
    topic_prefix: String,
    ha_discovery_prefix: String,
}

#[derive(Debug, Deserialize)]
struct ChargerConfig {
    name: String,
    unique_id: String,
}

fn load_config(path: &str) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;
    toml::from_str(&content).context("Failed to parse config.toml")
}

// ---------------------------------------------------------------------------
// Alfen Eve Modbus register map
//
// All multi-byte values are big-endian.  Float32 = 2 registers, Float64 = 4.
// Register addresses are 0-based (as sent on the wire).
// ---------------------------------------------------------------------------

/// Alfen Eve register addresses (0-based)
mod reg {
    // -- Station info (holding registers, ASCII encoded) --
    pub const STATION_NAME: u16 = 100;   // 25 × u16 (50 bytes ASCII)
    pub const SERIAL_NR: u16 = 125;      // 11 × u16 (22 bytes ASCII)
    pub const FIRMWARE_VER: u16 = 136;   // 5  × u16 (10 bytes ASCII)
    pub const ACTIVE_MAX_CURRENT: u16 = 1100; // float32, station active max current

    // -- Meter / measurements --
    pub const METER_STATE: u16 = 300;      // UNSIGNED16 state bitmask
    pub const METER_LAST_VALUE: u16 = 301; // UNSIGNED64 ms since last received measurement
    pub const METER_TYPE: u16 = 305;       // UNSIGNED16 meter transport type

    // Voltages (FLOAT32, 2 regs each)
    pub const VOLTAGE_L1: u16 = 306;       // Vrms line-to-neutral
    pub const VOLTAGE_L2: u16 = 308;
    pub const VOLTAGE_L3: u16 = 310;
    pub const VOLTAGE_L1_L2: u16 = 312;    // Vrms line-to-line
    pub const VOLTAGE_L2_L3: u16 = 314;
    pub const VOLTAGE_L3_L1: u16 = 316;

    // Currents (FLOAT32, 2 regs each)
    pub const CURRENT_N: u16 = 318;
    pub const CURRENT_L1: u16 = 320;
    pub const CURRENT_L2: u16 = 322;
    pub const CURRENT_L3: u16 = 324;
    pub const CURRENT_SUM: u16 = 326;

    // Power factors and frequency (FLOAT32)
    pub const POWER_FACTOR_L1: u16 = 328;
    pub const POWER_FACTOR_L2: u16 = 330;
    pub const POWER_FACTOR_L3: u16 = 332;
    pub const POWER_FACTOR: u16 = 334;     // Sum
    pub const FREQUENCY: u16 = 336;        // Hz

    // Real / apparent / reactive power (FLOAT32)
    pub const POWER_L1: u16 = 338;         // W active
    pub const POWER_L2: u16 = 340;
    pub const POWER_L3: u16 = 342;
    pub const POWER_TOTAL: u16 = 344;
    pub const APPARENT_POWER_L1: u16 = 346; // VA
    pub const APPARENT_POWER_L2: u16 = 348;
    pub const APPARENT_POWER_L3: u16 = 350;
    pub const APPARENT_POWER_SUM: u16 = 352;
    pub const REACTIVE_POWER_L1: u16 = 354; // VAr
    pub const REACTIVE_POWER_L2: u16 = 356;
    pub const REACTIVE_POWER_L3: u16 = 358;
    pub const REACTIVE_POWER_SUM: u16 = 360;

    // Energies (FLOAT64, 4 regs each)
    pub const ENERGY_DELIVERED_L1: u16 = 362; // Wh
    pub const ENERGY_DELIVERED_L2: u16 = 366;
    pub const ENERGY_DELIVERED_L3: u16 = 370;
    pub const ENERGY_TOTAL: u16 = 374;        // Wh delivered sum
    pub const ENERGY_CONSUMED_L1: u16 = 378;
    pub const ENERGY_CONSUMED_L2: u16 = 382;
    pub const ENERGY_CONSUMED_L3: u16 = 386;
    pub const ENERGY_CONSUMED_SUM: u16 = 390;
    pub const APPARENT_ENERGY_L1: u16 = 394;  // VAh
    pub const APPARENT_ENERGY_L2: u16 = 398;
    pub const APPARENT_ENERGY_L3: u16 = 402;
    pub const APPARENT_ENERGY_SUM: u16 = 406;
    pub const REACTIVE_ENERGY_L1: u16 = 410;  // VArh
    pub const REACTIVE_ENERGY_L2: u16 = 414;
    pub const REACTIVE_ENERGY_L3: u16 = 418;
    pub const REACTIVE_ENERGY_SUM: u16 = 422;

    // -- Status --
    pub const AVAILABILITY: u16 = 1200;         // UNSIGNED16, 1=operative 0=inoperative
    pub const MODE3_STATE: u16 = 1201;           // STRING 5 regs (10 bytes): IEC 61851 state text ("A","B1","C2"…)
    pub const APPLIED_MAX_CURRENT: u16 = 1206;   // FLOAT32, actual applied overall max current for socket
    pub const MAX_CURRENT_VALID_TIME: u16 = 1208; // UNSIGNED32, remaining seconds before fallback to safe current
    pub const ACTIVE_LOAD_BALANCING: u16 = 1212; // FLOAT32, active load balancing safe current

    // -- Control (writeable) --
    pub const MAX_CURRENT: u16 = 1210;           // FLOAT32, Modbus slave max current setpoint (R/W)
    pub const SETPOINT_ACCOUNTED: u16 = 1214;    // UNSIGNED16, 1=setpoint accepted, 0=overridden
    pub const PHASES: u16 = 1215;                // UNSIGNED16, 1=single-phase, 3=three-phase (R/W)
}

/// IEC 61851 state string (from register 1201, 5 regs) → friendly description.
fn mode3_state_description(raw: &str) -> &'static str {
    match raw.trim_end_matches('\0').trim() {
        "A"  => "A (not connected)",
        "B1" => "B1 (connected, no power)",
        "B2" => "B2 (connected, ventilation ok)",
        "C1" => "C1 (charging, no ventilation)",
        "C2" => "C2 (charging)",
        "D1" => "D1 (charging with ventilation)",
        "D2" => "D2 (charging with ventilation)",
        "E"  => "E (short circuit)",
        "F"  => "F (error)",
        _    => "unknown",
    }
}

/// Map IEC 61851 state string → legacy numeric code for MQTT backward-compatibility.
fn mode3_string_to_u16(raw: &str) -> u16 {
    match raw.trim_end_matches('\0').trim() {
        "A"  => 0,
        "B1" => 1,
        "B2" => 2,
        "C1" => 3,
        "C2" => 4,
        "D1" => 5,
        "D2" => 6,
        "E"  => 7,
        "F"  => 8,
        _    => 255,
    }
}

// ---------------------------------------------------------------------------
// Modbus helpers
// ---------------------------------------------------------------------------

fn regs_to_f32(regs: &[u16]) -> f32 {
    let mut buf = [0u8; 4];
    BigEndian::write_u16(&mut buf[0..2], regs[0]);
    BigEndian::write_u16(&mut buf[2..4], regs[1]);
    f32::from_be_bytes(buf)
}

fn regs_to_f64(regs: &[u16]) -> f64 {
    let mut buf = [0u8; 8];
    BigEndian::write_u16(&mut buf[0..2], regs[0]);
    BigEndian::write_u16(&mut buf[2..4], regs[1]);
    BigEndian::write_u16(&mut buf[4..6], regs[2]);
    BigEndian::write_u16(&mut buf[6..8], regs[3]);
    f64::from_be_bytes(buf)
}

fn regs_to_u32(regs: &[u16]) -> u32 {
    ((regs[0] as u32) << 16) | (regs[1] as u32)
}

fn regs_to_u64(regs: &[u16]) -> u64 {
    ((regs[0] as u64) << 48)
        | ((regs[1] as u64) << 32)
        | ((regs[2] as u64) << 16)
        | (regs[3] as u64)
}

fn regs_to_string(regs: &[u16]) -> String {
    let bytes: Vec<u8> = regs
        .iter()
        .flat_map(|r| [(r >> 8) as u8, (r & 0xFF) as u8])
        .collect();
    String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

fn f32_to_regs(v: f32) -> [u16; 2] {
    let bytes = v.to_be_bytes();
    [
        BigEndian::read_u16(&bytes[0..2]),
        BigEndian::read_u16(&bytes[2..4]),
    ]
}

fn finite_f32_or(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

fn finite_f64_or(v: f64, default: f64) -> f64 {
    if v.is_finite() { v } else { default }
}

fn decode_max_current(regs: &[u16]) -> (f32, &'static str) {
    // Preferred format from Alfen docs: float32 spread over two registers.
    let as_f32 = finite_f32_or(regs_to_f32(regs), 0.0);
    if (0.0..=64.0).contains(&as_f32) && (as_f32 > 0.05 || (regs[0] == 0 && regs[1] == 0)) {
        return (as_f32, "float32");
    }

    // Some firmware variants expose this as a plain integer in the first register.
    if regs[1] == 0 && regs[0] <= 64 {
        return (regs[0] as f32, "u16_amps");
    }

    // Common scaled integer variant: tenths of amps in first register.
    if regs[1] == 0 && regs[0] <= 640 {
        return (regs[0] as f32 / 10.0, "u16_tenths_amps");
    }

    (as_f32, "float32_fallback")
}

fn effective_mqtt_client_id(base: &str) -> String {
    // Build a stable-enough runtime suffix so concurrent instances don't collide.
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "host".to_string());
    let pid = std::process::id();
    let suffix = format!("{host}-{pid}");

    // Keep the full ID within a practical size for brokers with stricter limits.
    let max_len = 64usize;
    if base.len() + 1 + suffix.len() <= max_len {
        return format!("{base}-{suffix}");
    }

    let keep_base = max_len.saturating_sub(1 + suffix.len());
    if keep_base == 0 {
        return suffix.chars().take(max_len).collect();
    }

    let base_truncated: String = base.chars().take(keep_base).collect();
    format!("{base_truncated}-{suffix}")
}

fn is_illegal_data_address<E: std::fmt::Display>(e: &E) -> bool {
    e.to_string().contains("Illegal data address")
}

async fn write_f32_with_fallback(
    ctx: &mut tokio_modbus::client::Context,
    start: u16,
    value: f32,
    label: &str,
) -> Result<()> {
    let regs = f32_to_regs(value);
    info!(
        "Writing {label}: value={value:.2} as float32 regs={:?} to addr={}..{} using Write Multiple Registers",
        regs,
        start,
        start.saturating_add(1)
    );

    match ctx.write_multiple_registers(start, &regs).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Some chargers/maps are documented 1-based while Modbus wire is 0-based.
            // Retry once at +1 when address is rejected.
            if is_illegal_data_address(&e) && start < u16::MAX {
                let fallback_start = start + 1;
                ctx.write_multiple_registers(fallback_start, &regs)
                    .await
                    .with_context(|| {
                        format!(
                            "Write failed ({label}, addr={start}) and fallback failed (addr={fallback_start})"
                        )
                    })?;
                warn!(
                    "Write fallback applied ({label}): addr={start} rejected, using addr={fallback_start}..{}",
                    fallback_start.saturating_add(1)
                );
                return Ok(());
            }

            anyhow::bail!("Write failed ({label}, addr={start}): {e}");
        }
    }
}

async fn write_u16_with_fallback(
    ctx: &mut tokio_modbus::client::Context,
    start: u16,
    value: u16,
    label: &str,
) -> Result<()> {
    info!(
        "Writing {label}: value={} to addr={} using Write Single Register",
        value,
        start
    );

    match ctx.write_single_register(start, value).await {
        Ok(()) => Ok(()),
        Err(e) => {
            if is_illegal_data_address(&e) && start < u16::MAX {
                let fallback_start = start + 1;
                ctx.write_single_register(fallback_start, value)
                    .await
                    .with_context(|| {
                        format!(
                            "Write failed ({label}, addr={start}) and fallback failed (addr={fallback_start})"
                        )
                    })?;
                warn!(
                    "Write fallback applied ({label}): addr={start} rejected, using addr={fallback_start}"
                );
                return Ok(());
            }

            anyhow::bail!("Write failed ({label}, addr={start}): {e}");
        }
    }
}

#[derive(Copy, Clone)]
struct TakeoverSettings {
    enabled: bool,
    safe_current_amps: f32,
    /// If `Some`, write this safe-current register before MAX_CURRENT.
    /// Leave `None` for standard table-based mappings.
    safe_current_register: Option<u16>,
    validity_secs: u16,
    /// If `Some`, a u16 validity-seconds value is written to this register before MAX_CURRENT.
    /// There is no such register in the standard Alfen Modbus map; leave `None` unless your
    /// firmware variant specifically supports it.
    validity_register: Option<u16>,
}

async fn write_setpoint_with_takeover(
    ctx: &mut tokio_modbus::client::Context,
    control_slave: u8,
    target_amps: f32,
    settings: TakeoverSettings,
    context_label: &str,
) -> Result<()> {
    ctx.set_slave(Slave(control_slave));

    if settings.enabled {
        if let Some(safe_current_reg) = settings.safe_current_register {
            if let Err(e) = write_f32_with_fallback(ctx, safe_current_reg, settings.safe_current_amps, "safe_current").await {
                warn!(
                    "Safe-current prewrite failed on reg{}: {}. Continuing with validity + max-current sequence.",
                    safe_current_reg,
                    e
                );
            }
        }
        if let Some(validity_reg) = settings.validity_register {
            write_u16_with_fallback(
                ctx,
                validity_reg,
                settings.validity_secs,
                "setpoint_validity_secs",
            )
            .await?;
        }
    }

    write_f32_with_fallback(ctx, reg::MAX_CURRENT, target_amps, context_label).await
}

async fn read_optional_u16_with_fallback(
    ctx: &mut tokio_modbus::client::Context,
    start: u16,
    label: &str,
) -> Option<u16> {
    match ctx.read_holding_registers(start, 1).await {
        Ok(v) => v.first().copied(),
        Err(e) => {
            if is_illegal_data_address(&e) && start < u16::MAX {
                let fallback_start = start + 1;
                match ctx.read_holding_registers(fallback_start, 1).await {
                    Ok(v) => {
                        warn!(
                            "Read fallback applied ({label}): addr={start} rejected, using addr={fallback_start}, qty=1"
                        );
                        v.first().copied()
                    }
                    Err(e2) => {
                        warn!(
                            "Optional read failed ({label}, addr={start}) and fallback (addr={fallback_start}) failed: {e2}"
                        );
                        None
                    }
                }
            } else {
                warn!("Optional read failed ({label}, addr={start}): {e}");
                None
            }
        }
    }
}

async fn read_setpoint_accounted(ctx: &mut tokio_modbus::client::Context) -> Option<u16> {
    read_optional_u16_with_fallback(ctx, reg::SETPOINT_ACCOUNTED, "setpoint_accounted").await
}

async fn read_optional_f32_with_fallback(
    ctx: &mut tokio_modbus::client::Context,
    start: u16,
    label: &str,
) -> Option<f32> {
    let read = match ctx.read_holding_registers(start, 2).await {
        Ok(v) => Some(v),
        Err(e) => {
            if is_illegal_data_address(&e) && start < u16::MAX {
                let fallback_start = start + 1;
                match ctx.read_holding_registers(fallback_start, 2).await {
                    Ok(v) => {
                        warn!(
                            "Read fallback applied ({label}): addr={start} rejected, using addr={fallback_start}, qty=2"
                        );
                        Some(v)
                    }
                    Err(e2) => {
                        warn!(
                            "Optional read failed ({label}, addr={start}) and fallback (addr={fallback_start}) failed: {e2}"
                        );
                        None
                    }
                }
            } else {
                warn!("Optional read failed ({label}, addr={start}): {e}");
                None
            }
        }
    };

    read.map(|regs| finite_f32_or(regs_to_f32(&regs), 0.0))
}

async fn read_optional_u32_with_fallback(
    ctx: &mut tokio_modbus::client::Context,
    start: u16,
    label: &str,
) -> Option<u32> {
    let read = match ctx.read_holding_registers(start, 2).await {
        Ok(v) => Some(v),
        Err(e) => {
            if is_illegal_data_address(&e) && start < u16::MAX {
                let fallback_start = start + 1;
                match ctx.read_holding_registers(fallback_start, 2).await {
                    Ok(v) => {
                        warn!(
                            "Read fallback applied ({label}): addr={start} rejected, using addr={fallback_start}, qty=2"
                        );
                        Some(v)
                    }
                    Err(e2) => {
                        warn!(
                            "Optional read failed ({label}, addr={start}) and fallback (addr={fallback_start}) failed: {e2}"
                        );
                        None
                    }
                }
            } else {
                warn!("Optional read failed ({label}, addr={start}): {e}");
                None
            }
        }
    };

    read.map(|regs| regs_to_u32(&regs))
}

async fn read_applied_max_current(ctx: &mut tokio_modbus::client::Context) -> Option<f32> {
    read_optional_f32_with_fallback(ctx, reg::APPLIED_MAX_CURRENT, "applied_max_current").await
}

fn log_applied_limit_hint(requested: f32, applied: Option<f32>, context_label: &str) {
    match applied {
        Some(applied_amps) if applied_amps + 0.2 < requested => {
            warn!(
                "Applied limit hint ({context_label}): requested={requested:.1}A but reg{} reports applied={applied_amps:.1}A; likely station-level cap/override",
                reg::APPLIED_MAX_CURRENT
            );
        }
        Some(applied_amps) => {
            info!(
                "Applied limit hint ({context_label}): reg{} applied={applied_amps:.1}A",
                reg::APPLIED_MAX_CURRENT
            );
        }
        None => {
            warn!(
                "Applied limit hint ({context_label}): reg{} unavailable",
                reg::APPLIED_MAX_CURRENT
            );
        }
    }
}

fn log_setpoint_accounted_proof(raw: Option<u16>, context_label: &str) {
    match raw {
        Some(1) => info!(
            "Setpoint proof ({context_label}) reg{}=1: charger is listening to Modbus setpoint",
            reg::SETPOINT_ACCOUNTED
        ),
        Some(0) => warn!(
            "Setpoint proof ({context_label}) reg{}=0: charger received setpoint but is ignoring it due to higher-priority control",
            reg::SETPOINT_ACCOUNTED
        ),
        Some(v) => warn!(
            "Setpoint proof ({context_label}) reg{} has unexpected value {}",
            reg::SETPOINT_ACCOUNTED,
            v
        ),
        None => warn!(
            "Setpoint proof ({context_label}) reg{} unavailable",
            reg::SETPOINT_ACCOUNTED
        ),
    }
}

fn log_remaining_valid_time(raw: Option<u32>, context_label: &str) {
    match raw {
        Some(secs) => info!(
            "Remaining valid time ({context_label}) reg{}={}s",
            reg::MAX_CURRENT_VALID_TIME,
            secs
        ),
        None => warn!(
            "Remaining valid time ({context_label}) reg{} unavailable",
            reg::MAX_CURRENT_VALID_TIME
        ),
    }
}

async fn log_direct_max_current_readback(
    ctx: &mut tokio_modbus::client::Context,
    context_label: &str,
) -> Option<f32> {
    match ctx.read_holding_registers(reg::MAX_CURRENT, 2).await {
        Ok(regs) => {
            let (decoded, encoding) = decode_max_current(&regs);
            info!(
                "Direct readback ({context_label}) addr={} raw={:?} decoded={:.2}A ({encoding})",
                reg::MAX_CURRENT,
                regs,
                decoded
            );
            Some(decoded)
        }
        Err(e) => {
            warn!(
                "Direct readback failed ({context_label}) addr={}: {e}",
                reg::MAX_CURRENT
            );

            // Diagnostic fallback probe: some maps shift by +1.
            let fallback_addr = reg::MAX_CURRENT.saturating_add(1);
            if fallback_addr != reg::MAX_CURRENT {
                match ctx.read_holding_registers(fallback_addr, 2).await {
                    Ok(regs) => {
                        let (decoded, encoding) = decode_max_current(&regs);
                        info!(
                            "Direct readback fallback ({context_label}) addr={} raw={:?} decoded={:.2}A ({encoding})",
                            fallback_addr,
                            regs,
                            decoded
                        );
                        Some(decoded)
                    }
                    Err(e2) => {
                        warn!(
                            "Direct readback fallback failed ({context_label}) addr={}: {e2}",
                            fallback_addr
                        );
                        None
                    }
                }
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ChargerState {
    // Identification
    station_name: String,
    serial_nr: String,
    firmware_ver: String,

    // Measurements
    meter_state: u16,
    meter_last_value_ms: u64,
    meter_type: u16,
    voltage_l1: f32,
    voltage_l2: f32,
    voltage_l3: f32,
    voltage_l1_l2: f32,
    voltage_l2_l3: f32,
    voltage_l3_l1: f32,
    current_n: f32,
    current_l1: f32,
    current_l2: f32,
    current_l3: f32,
    current_sum: f32,
    power_factor_l1: f32,
    power_factor_l2: f32,
    power_factor_l3: f32,
    power_l1: f32,
    power_l2: f32,
    power_l3: f32,
    power_total: f32,
    power_factor: f32,
    frequency_hz: f32,
    apparent_power_l1: f32,
    apparent_power_l2: f32,
    apparent_power_l3: f32,
    apparent_power_total: f32,
    reactive_power_l1: f32,
    reactive_power_l2: f32,
    reactive_power_l3: f32,
    reactive_power_total: f32,
    energy_delivered_l1_kwh: f64,
    energy_delivered_l2_kwh: f64,
    energy_delivered_l3_kwh: f64,
    energy_total_kwh: f64,
    energy_consumed_l1_kwh: f64,
    energy_consumed_l2_kwh: f64,
    energy_consumed_l3_kwh: f64,
    energy_consumed_total_kwh: f64,
    apparent_energy_l1_kvah: f64,
    apparent_energy_l2_kvah: f64,
    apparent_energy_l3_kvah: f64,
    apparent_energy_total_kvah: f64,
    reactive_energy_l1_kvarh: f64,
    reactive_energy_l2_kvarh: f64,
    reactive_energy_l3_kvarh: f64,
    reactive_energy_total_kvarh: f64,

    // Status
    available: bool,
    mode3_state: u16,
    mode3_state_name: String,
    max_current: f32,
    active_max_current: Option<f32>,
    applied_max_current: Option<f32>,
    active_load_balancing: Option<f32>,
    phases_raw: Option<u16>,
    setpoint_accounted_raw: Option<u16>,
    setpoint_accounted: Option<bool>,
}

impl Default for ChargerState {
    fn default() -> Self {
        Self {
            station_name: "unknown".to_string(),
            serial_nr: "unknown".to_string(),
            firmware_ver: "unknown".to_string(),
            meter_state: 0,
            meter_last_value_ms: 0,
            meter_type: 0,
            voltage_l1: 0.0,
            voltage_l2: 0.0,
            voltage_l3: 0.0,
            voltage_l1_l2: 0.0,
            voltage_l2_l3: 0.0,
            voltage_l3_l1: 0.0,
            current_n: 0.0,
            current_l1: 0.0,
            current_l2: 0.0,
            current_l3: 0.0,
            current_sum: 0.0,
            power_factor_l1: 0.0,
            power_factor_l2: 0.0,
            power_factor_l3: 0.0,
            power_l1: 0.0,
            power_l2: 0.0,
            power_l3: 0.0,
            power_total: 0.0,
            power_factor: 0.0,
            frequency_hz: 0.0,
            apparent_power_l1: 0.0,
            apparent_power_l2: 0.0,
            apparent_power_l3: 0.0,
            apparent_power_total: 0.0,
            reactive_power_l1: 0.0,
            reactive_power_l2: 0.0,
            reactive_power_l3: 0.0,
            reactive_power_total: 0.0,
            energy_delivered_l1_kwh: 0.0,
            energy_delivered_l2_kwh: 0.0,
            energy_delivered_l3_kwh: 0.0,
            energy_total_kwh: 0.0,
            energy_consumed_l1_kwh: 0.0,
            energy_consumed_l2_kwh: 0.0,
            energy_consumed_l3_kwh: 0.0,
            energy_consumed_total_kwh: 0.0,
            apparent_energy_l1_kvah: 0.0,
            apparent_energy_l2_kvah: 0.0,
            apparent_energy_l3_kvah: 0.0,
            apparent_energy_total_kvah: 0.0,
            reactive_energy_l1_kvarh: 0.0,
            reactive_energy_l2_kvarh: 0.0,
            reactive_energy_l3_kvarh: 0.0,
            reactive_energy_total_kvarh: 0.0,
            available: false,
            mode3_state: 0,
            mode3_state_name: "unknown".to_string(),
            max_current: 0.0,
            active_max_current: None,
            applied_max_current: None,
            active_load_balancing: None,
            phases_raw: None,
            setpoint_accounted_raw: None,
            setpoint_accounted: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Modbus poller
// ---------------------------------------------------------------------------

async fn read_charger(
    ctx: &mut tokio_modbus::client::Context,
    socket_slave: u8,
    station_slave: u8,
) -> Result<ChargerState> {
    static READ_OPTIONAL_IDENTITY_ONCE: AtomicBool = AtomicBool::new(true);
    static LOGGED_ALT_MAX_CURRENT_ENCODING: AtomicBool = AtomicBool::new(false);

    async fn read_with_fallback(
        ctx: &mut tokio_modbus::client::Context,
        start: u16,
        qty: u16,
        label: &str,
        required: bool,
    ) -> Result<Vec<u16>> {
        match ctx.read_holding_registers(start, qty).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // Some chargers/maps are documented 1-based while Modbus wire is 0-based.
                // Retry once at +1 when address is rejected.
                if is_illegal_data_address(&e) && start < u16::MAX {
                    let fallback_start = start + 1;
                    match ctx.read_holding_registers(fallback_start, qty).await {
                        Ok(v) => {
                            warn!(
                                "Read fallback applied ({label}): addr={start} rejected, using addr={fallback_start}, qty={qty}"
                            );
                            return Ok(v);
                        }
                        Err(e2) => {
                            if required && is_illegal_data_address(&e2) {
                                warn!(
                                    "Required read unsupported ({label}, addr={start}, qty={qty}) and fallback (addr={fallback_start}) also unsupported; using defaults for this block."
                                );
                                return Ok(vec![0u16; qty as usize]);
                            }
                            if required {
                                anyhow::bail!(
                                    "Required read failed ({label}, addr={start}, qty={qty}) and fallback failed (addr={fallback_start}): {e2}"
                                );
                            }
                            warn!(
                                "Optional read failed ({label}, addr={start}, qty={qty}) and fallback (addr={fallback_start}) failed: {e2}. Using defaults."
                            );
                            return Ok(vec![0u16; qty as usize]);
                        }
                    }
                }

                if required && is_illegal_data_address(&e) {
                    warn!(
                        "Required read unsupported ({label}, addr={start}, qty={qty}); using defaults for this block."
                    );
                    return Ok(vec![0u16; qty as usize]);
                }

                if required {
                    anyhow::bail!("Required read failed ({label}, addr={start}, qty={qty}): {e}");
                }

                warn!(
                    "Optional read failed ({label}, addr={start}, qty={qty}): {e}. Using defaults."
                );
                Ok(vec![0u16; qty as usize])
            }
        }
    }

    // Measurements + control telemetry are usually on the socket slave (often 1).
    ctx.set_slave(Slave(socket_slave));
    // Measurements — read contiguous blocks for efficiency.
    let meter_regs = read_with_fallback(ctx, reg::METER_STATE, 6, "meter", false).await?;
    let volt_regs = read_with_fallback(ctx, reg::VOLTAGE_L1, 6, "voltages_ln", false).await?;
    let volt_ll_regs = read_with_fallback(ctx, reg::VOLTAGE_L1_L2, 6, "voltages_ll", false).await?;
    let curr_regs = read_with_fallback(ctx, reg::CURRENT_N, 10, "currents", false).await?;
    let pf_freq_regs = read_with_fallback(ctx, reg::POWER_FACTOR_L1, 10, "power_factor_frequency", false).await?;
    let pow_regs = read_with_fallback(ctx, reg::POWER_L1, 8, "powers", false).await?;
    let app_reactive_power_regs = read_with_fallback(ctx, reg::APPARENT_POWER_L1, 16, "apparent_reactive_power", false).await?;
    let energy_regs = read_with_fallback(ctx, reg::ENERGY_DELIVERED_L1, 64, "energies", false).await?;
    // Status + max_current
    let avail_regs = read_with_fallback(ctx, reg::AVAILABILITY, 1, "availability", true).await?;
    // Mode 3 state: 5 holding registers (10 bytes) containing the IEC 61851 state as an ASCII string.
    let mode3_regs = read_with_fallback(ctx, reg::MODE3_STATE, 5, "mode3_state", true).await?;
    let max_curr_regs = read_with_fallback(ctx, reg::MAX_CURRENT, 2, "max_current", true).await?;
    let applied_max_current = read_applied_max_current(ctx).await;
    let active_load_balancing =
        read_optional_f32_with_fallback(ctx, reg::ACTIVE_LOAD_BALANCING, "active_load_balancing").await;
    let phases_raw = read_optional_u16_with_fallback(ctx, reg::PHASES, "phases").await;
    let setpoint_accounted_raw = read_setpoint_accounted(ctx).await;
    let (decoded_max_current, max_current_encoding) = decode_max_current(&max_curr_regs);
    if max_current_encoding != "float32"
        && !LOGGED_ALT_MAX_CURRENT_ENCODING.swap(true, Ordering::Relaxed)
    {
        warn!(
            "Using alternate max_current decoding mode '{}' for regs={:?}",
            max_current_encoding,
            max_curr_regs
        );
    }

    let mode3_raw = regs_to_string(&mode3_regs);
    let mode3_state = mode3_string_to_u16(&mode3_raw);

    // Station-level active max current (slave 200 on many Pro-line firmwares).
    ctx.set_slave(Slave(station_slave));
    let active_max_current =
        read_optional_f32_with_fallback(ctx, reg::ACTIVE_MAX_CURRENT, "active_max_current").await;

    // Station info (optional on some firmware/models); probe once to avoid log spam.
    // Execute this after required socket reads so unsupported station probes cannot block startup.
    let read_optional_identity = READ_OPTIONAL_IDENTITY_ONCE.swap(false, Ordering::Relaxed);
    let (name_regs, serial_regs, fw_regs) = if read_optional_identity {
        ctx.set_slave(Slave(station_slave));
        (
            read_with_fallback(ctx, reg::STATION_NAME, 25, "station_name", false).await?,
            read_with_fallback(ctx, reg::SERIAL_NR, 11, "serial_nr", false).await?,
            read_with_fallback(ctx, reg::FIRMWARE_VER, 5, "firmware_ver", false).await?,
        )
    } else {
        (vec![0u16; 25], vec![0u16; 11], vec![0u16; 5])
    };

    Ok(ChargerState {
        station_name: regs_to_string(&name_regs),
        serial_nr: regs_to_string(&serial_regs),
        firmware_ver: regs_to_string(&fw_regs),

        meter_state: meter_regs[0],
        meter_last_value_ms: regs_to_u64(&meter_regs[1..5]),
        meter_type: meter_regs[5],

        voltage_l1: finite_f32_or(regs_to_f32(&volt_regs[0..2]), 0.0),
        voltage_l2: finite_f32_or(regs_to_f32(&volt_regs[2..4]), 0.0),
        voltage_l3: finite_f32_or(regs_to_f32(&volt_regs[4..6]), 0.0),
        voltage_l1_l2: finite_f32_or(regs_to_f32(&volt_ll_regs[0..2]), 0.0),
        voltage_l2_l3: finite_f32_or(regs_to_f32(&volt_ll_regs[2..4]), 0.0),
        voltage_l3_l1: finite_f32_or(regs_to_f32(&volt_ll_regs[4..6]), 0.0),

        current_n: finite_f32_or(regs_to_f32(&curr_regs[0..2]), 0.0),
        current_l1: finite_f32_or(regs_to_f32(&curr_regs[2..4]), 0.0),
        current_l2: finite_f32_or(regs_to_f32(&curr_regs[4..6]), 0.0),
        current_l3: finite_f32_or(regs_to_f32(&curr_regs[6..8]), 0.0),
        current_sum: finite_f32_or(regs_to_f32(&curr_regs[8..10]), 0.0),

        power_factor_l1: finite_f32_or(regs_to_f32(&pf_freq_regs[0..2]), 0.0),
        power_factor_l2: finite_f32_or(regs_to_f32(&pf_freq_regs[2..4]), 0.0),
        power_factor_l3: finite_f32_or(regs_to_f32(&pf_freq_regs[4..6]), 0.0),
        power_factor: finite_f32_or(regs_to_f32(&pf_freq_regs[6..8]), 0.0),
        frequency_hz: finite_f32_or(regs_to_f32(&pf_freq_regs[8..10]), 0.0),

        power_l1: finite_f32_or(regs_to_f32(&pow_regs[0..2]), 0.0),
        power_l2: finite_f32_or(regs_to_f32(&pow_regs[2..4]), 0.0),
        power_l3: finite_f32_or(regs_to_f32(&pow_regs[4..6]), 0.0),
        power_total: finite_f32_or(regs_to_f32(&pow_regs[6..8]), 0.0),
        apparent_power_l1: finite_f32_or(regs_to_f32(&app_reactive_power_regs[0..2]), 0.0),
        apparent_power_l2: finite_f32_or(regs_to_f32(&app_reactive_power_regs[2..4]), 0.0),
        apparent_power_l3: finite_f32_or(regs_to_f32(&app_reactive_power_regs[4..6]), 0.0),
        apparent_power_total: finite_f32_or(regs_to_f32(&app_reactive_power_regs[6..8]), 0.0),
        reactive_power_l1: finite_f32_or(regs_to_f32(&app_reactive_power_regs[8..10]), 0.0),
        reactive_power_l2: finite_f32_or(regs_to_f32(&app_reactive_power_regs[10..12]), 0.0),
        reactive_power_l3: finite_f32_or(regs_to_f32(&app_reactive_power_regs[12..14]), 0.0),
        reactive_power_total: finite_f32_or(regs_to_f32(&app_reactive_power_regs[14..16]), 0.0),

        energy_delivered_l1_kwh: finite_f64_or(regs_to_f64(&energy_regs[0..4]) / 1000.0, 0.0),
        energy_delivered_l2_kwh: finite_f64_or(regs_to_f64(&energy_regs[4..8]) / 1000.0, 0.0),
        energy_delivered_l3_kwh: finite_f64_or(regs_to_f64(&energy_regs[8..12]) / 1000.0, 0.0),
        energy_total_kwh: finite_f64_or(regs_to_f64(&energy_regs[12..16]) / 1000.0, 0.0),
        energy_consumed_l1_kwh: finite_f64_or(regs_to_f64(&energy_regs[16..20]) / 1000.0, 0.0),
        energy_consumed_l2_kwh: finite_f64_or(regs_to_f64(&energy_regs[20..24]) / 1000.0, 0.0),
        energy_consumed_l3_kwh: finite_f64_or(regs_to_f64(&energy_regs[24..28]) / 1000.0, 0.0),
        energy_consumed_total_kwh: finite_f64_or(regs_to_f64(&energy_regs[28..32]) / 1000.0, 0.0),
        apparent_energy_l1_kvah: finite_f64_or(regs_to_f64(&energy_regs[32..36]) / 1000.0, 0.0),
        apparent_energy_l2_kvah: finite_f64_or(regs_to_f64(&energy_regs[36..40]) / 1000.0, 0.0),
        apparent_energy_l3_kvah: finite_f64_or(regs_to_f64(&energy_regs[40..44]) / 1000.0, 0.0),
        apparent_energy_total_kvah: finite_f64_or(regs_to_f64(&energy_regs[44..48]) / 1000.0, 0.0),
        reactive_energy_l1_kvarh: finite_f64_or(regs_to_f64(&energy_regs[48..52]) / 1000.0, 0.0),
        reactive_energy_l2_kvarh: finite_f64_or(regs_to_f64(&energy_regs[52..56]) / 1000.0, 0.0),
        reactive_energy_l3_kvarh: finite_f64_or(regs_to_f64(&energy_regs[56..60]) / 1000.0, 0.0),
        reactive_energy_total_kvarh: finite_f64_or(regs_to_f64(&energy_regs[60..64]) / 1000.0, 0.0),

        available: avail_regs[0] == 1,
        mode3_state,
        mode3_state_name: mode3_state_description(&mode3_raw).to_string(),
        max_current: decoded_max_current,
        active_max_current,
        applied_max_current,
        active_load_balancing,
        phases_raw,
        setpoint_accounted_raw,
        setpoint_accounted: setpoint_accounted_raw.map(|v| v == 1),
    })
}

// ---------------------------------------------------------------------------
// Home Assistant MQTT discovery helpers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HaDiscoveryDevice<'a> {
    identifiers: Vec<&'a str>,
    name: &'a str,
    model: &'static str,
    manufacturer: &'static str,
    sw_version: &'a str,
}

#[derive(Serialize)]
struct HaSensorConfig<'a> {
    name: &'a str,
    unique_id: String,
    state_topic: &'a str,
    value_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_of_measurement: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_class: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_class: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_category: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<&'a str>,
    device: HaDiscoveryDevice<'a>,
    availability_topic: &'a str,
    payload_available: &'static str,
    payload_not_available: &'static str,
}

#[derive(Serialize)]
struct HaBinarySensorConfig<'a> {
    name: &'a str,
    unique_id: String,
    state_topic: &'a str,
    value_template: String,
    payload_on: &'a str,
    payload_off: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_class: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_category: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<&'a str>,
    device: HaDiscoveryDevice<'a>,
    availability_topic: &'a str,
    payload_available: &'static str,
    payload_not_available: &'static str,
}

#[derive(Serialize)]
struct HaNumberConfig<'a> {
    name: &'a str,
    unique_id: String,
    state_topic: &'a str,
    value_template: String,
    command_topic: &'a str,
    min: f32,
    max: f32,
    step: f32,
    unit_of_measurement: &'a str,
    icon: &'a str,
    device: HaDiscoveryDevice<'a>,
    availability_topic: &'a str,
    payload_available: &'static str,
    payload_not_available: &'static str,
}

#[derive(Serialize)]
struct HaSwitchConfig<'a> {
    name: &'a str,
    unique_id: String,
    state_topic: &'a str,
    value_template: String,
    command_topic: &'a str,
    payload_on: &'a str,
    payload_off: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<&'a str>,
    device: HaDiscoveryDevice<'a>,
    availability_topic: &'a str,
    payload_available: &'static str,
    payload_not_available: &'static str,
}

async fn publish_ha_discovery(
    client: &AsyncClient,
    cfg: &Config,
    fw_version: &str,
) -> Result<()> {
    let uid = &cfg.charger.unique_id;
    let name = &cfg.charger.name;
    let prefix = &cfg.mqtt.topic_prefix;
    let ha = &cfg.mqtt.ha_discovery_prefix;
    let state_topic = format!("{prefix}/{uid}/state");
    let avail_topic = format!("{prefix}/{uid}/availability");

    macro_rules! device {
        () => {
            HaDiscoveryDevice {
                identifiers: vec![uid.as_str()],
                name: name.as_str(),
                model: "Eve Single Pro-line",
                manufacturer: "Alfen",
                sw_version: fw_version,
            }
        };
    }

    // Helper to publish a sensor
    macro_rules! publish_sensor {
        ($sensor_name:expr, $id_suffix:expr, $tpl:expr, $unit:expr, $dc:expr, $sc:expr, $entity_cat:expr, $icon:expr) => {{
            let cfg_payload = HaSensorConfig {
                name: $sensor_name,
                unique_id: format!("{uid}_{}", $id_suffix),
                state_topic: &state_topic,
                value_template: $tpl.to_string(),
                unit_of_measurement: $unit,
                device_class: $dc,
                state_class: $sc,
                entity_category: $entity_cat,
                icon: $icon,
                device: device!(),
                availability_topic: &avail_topic,
                payload_available: "online",
                payload_not_available: "offline",
            };
            let disc_topic = format!("{ha}/sensor/{uid}_{}/config", $id_suffix);
            client
                .publish(
                    disc_topic,
                    QoS::AtLeastOnce,
                    true,
                    serde_json::to_vec(&cfg_payload)?,
                )
                .await?;
        }};
    }

    macro_rules! publish_binary_sensor {
        ($sensor_name:expr, $id_suffix:expr, $tpl:expr, $payload_on:expr, $payload_off:expr, $dc:expr, $entity_cat:expr, $icon:expr) => {{
            let cfg_payload = HaBinarySensorConfig {
                name: $sensor_name,
                unique_id: format!("{uid}_{}", $id_suffix),
                state_topic: &state_topic,
                value_template: $tpl.to_string(),
                payload_on: $payload_on,
                payload_off: $payload_off,
                device_class: $dc,
                entity_category: $entity_cat,
                icon: $icon,
                device: device!(),
                availability_topic: &avail_topic,
                payload_available: "online",
                payload_not_available: "offline",
            };
            let disc_topic = format!("{ha}/binary_sensor/{uid}_{}/config", $id_suffix);
            client
                .publish(
                    disc_topic,
                    QoS::AtLeastOnce,
                    true,
                    serde_json::to_vec(&cfg_payload)?,
                )
                .await?;
        }};
    }

    // Sensors
    publish_sensor!("Power Total",    "power_total",     "{{ value_json.power_total | round(1) }}",      Some("W"),   Some("power"),        Some("measurement"), None,                  None);
    publish_sensor!("Power L1",       "power_l1",        "{{ value_json.power_l1 | round(1) }}",         Some("W"),   Some("power"),        Some("measurement"), None,                  None);
    publish_sensor!("Power L2",       "power_l2",        "{{ value_json.power_l2 | round(1) }}",         Some("W"),   Some("power"),        Some("measurement"), None,                  None);
    publish_sensor!("Power L3",       "power_l3",        "{{ value_json.power_l3 | round(1) }}",         Some("W"),   Some("power"),        Some("measurement"), None,                  None);
    publish_sensor!("Voltage L1",     "voltage_l1",      "{{ value_json.voltage_l1 | round(1) }}",       Some("V"),   Some("voltage"),      Some("measurement"), None,                  None);
    publish_sensor!("Voltage L2",     "voltage_l2",      "{{ value_json.voltage_l2 | round(1) }}",       Some("V"),   Some("voltage"),      Some("measurement"), None,                  None);
    publish_sensor!("Voltage L3",     "voltage_l3",      "{{ value_json.voltage_l3 | round(1) }}",       Some("V"),   Some("voltage"),      Some("measurement"), None,                  None);
    publish_sensor!("Current L1",     "current_l1",      "{{ value_json.current_l1 | round(2) }}",       Some("A"),   Some("current"),      Some("measurement"), None,                  None);
    publish_sensor!("Current L2",     "current_l2",      "{{ value_json.current_l2 | round(2) }}",       Some("A"),   Some("current"),      Some("measurement"), None,                  None);
    publish_sensor!("Current L3",     "current_l3",      "{{ value_json.current_l3 | round(2) }}",       Some("A"),   Some("current"),      Some("measurement"), None,                  None);
    publish_sensor!("Voltage L1-L2",  "voltage_l1_l2",   "{{ value_json.voltage_l1_l2 | round(1) }}",    Some("V"),   Some("voltage"),      Some("measurement"), None,                  None);
    publish_sensor!("Voltage L2-L3",  "voltage_l2_l3",   "{{ value_json.voltage_l2_l3 | round(1) }}",    Some("V"),   Some("voltage"),      Some("measurement"), None,                  None);
    publish_sensor!("Voltage L3-L1",  "voltage_l3_l1",   "{{ value_json.voltage_l3_l1 | round(1) }}",    Some("V"),   Some("voltage"),      Some("measurement"), None,                  None);
    publish_sensor!("Current N",      "current_n",       "{{ value_json.current_n | round(2) }}",         Some("A"),   Some("current"),      Some("measurement"), None,                  None);
    publish_sensor!("Current Sum",    "current_sum",     "{{ value_json.current_sum | round(2) }}",       Some("A"),   Some("current"),      Some("measurement"), None,                  None);
    publish_sensor!("Power Factor",   "power_factor",    "{{ value_json.power_factor | round(2) }}",     None,        Some("power_factor"), Some("measurement"), None,                  None);
    publish_sensor!("Power Factor L1", "power_factor_l1", "{{ value_json.power_factor_l1 | round(2) }}",  None,        Some("power_factor"), Some("measurement"), None,                  None);
    publish_sensor!("Power Factor L2", "power_factor_l2", "{{ value_json.power_factor_l2 | round(2) }}",  None,        Some("power_factor"), Some("measurement"), None,                  None);
    publish_sensor!("Power Factor L3", "power_factor_l3", "{{ value_json.power_factor_l3 | round(2) }}",  None,        Some("power_factor"), Some("measurement"), None,                  None);
    publish_sensor!("Frequency",      "frequency_hz",    "{{ value_json.frequency_hz | round(2) }}",      Some("Hz"),  Some("frequency"),    Some("measurement"), None,                  None);
    publish_sensor!("Energy Total",   "energy_total",    "{{ value_json.energy_total_kwh | round(3) }}", Some("kWh"), Some("energy"),       Some("total_increasing"), None,                  None);
    publish_sensor!("Delivered Energy L1", "energy_delivered_l1", "{{ value_json.energy_delivered_l1_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Delivered Energy L2", "energy_delivered_l2", "{{ value_json.energy_delivered_l2_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Delivered Energy L3", "energy_delivered_l3", "{{ value_json.energy_delivered_l3_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Consumed Energy L1", "energy_consumed_l1", "{{ value_json.energy_consumed_l1_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Consumed Energy L2", "energy_consumed_l2", "{{ value_json.energy_consumed_l2_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Consumed Energy L3", "energy_consumed_l3", "{{ value_json.energy_consumed_l3_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Consumed Energy Total", "energy_consumed_total", "{{ value_json.energy_consumed_total_kwh | round(3) }}", Some("kWh"), Some("energy"), Some("total_increasing"), None, None);
    publish_sensor!("Apparent Power L1", "apparent_power_l1", "{{ value_json.apparent_power_l1 | round(1) }}", Some("VA"), None, Some("measurement"), None, None);
    publish_sensor!("Apparent Power L2", "apparent_power_l2", "{{ value_json.apparent_power_l2 | round(1) }}", Some("VA"), None, Some("measurement"), None, None);
    publish_sensor!("Apparent Power L3", "apparent_power_l3", "{{ value_json.apparent_power_l3 | round(1) }}", Some("VA"), None, Some("measurement"), None, None);
    publish_sensor!("Apparent Power Total", "apparent_power_total", "{{ value_json.apparent_power_total | round(1) }}", Some("VA"), None, Some("measurement"), None, None);
    publish_sensor!("Reactive Power L1", "reactive_power_l1", "{{ value_json.reactive_power_l1 | round(1) }}", Some("VAr"), None, Some("measurement"), None, None);
    publish_sensor!("Reactive Power L2", "reactive_power_l2", "{{ value_json.reactive_power_l2 | round(1) }}", Some("VAr"), None, Some("measurement"), None, None);
    publish_sensor!("Reactive Power L3", "reactive_power_l3", "{{ value_json.reactive_power_l3 | round(1) }}", Some("VAr"), None, Some("measurement"), None, None);
    publish_sensor!("Reactive Power Total", "reactive_power_total", "{{ value_json.reactive_power_total | round(1) }}", Some("VAr"), None, Some("measurement"), None, None);
    publish_sensor!("Apparent Energy L1", "apparent_energy_l1", "{{ value_json.apparent_energy_l1_kvah | round(3) }}", Some("kVAh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Apparent Energy L2", "apparent_energy_l2", "{{ value_json.apparent_energy_l2_kvah | round(3) }}", Some("kVAh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Apparent Energy L3", "apparent_energy_l3", "{{ value_json.apparent_energy_l3_kvah | round(3) }}", Some("kVAh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Apparent Energy Total", "apparent_energy_total", "{{ value_json.apparent_energy_total_kvah | round(3) }}", Some("kVAh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Reactive Energy L1", "reactive_energy_l1", "{{ value_json.reactive_energy_l1_kvarh | round(3) }}", Some("kVArh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Reactive Energy L2", "reactive_energy_l2", "{{ value_json.reactive_energy_l2_kvarh | round(3) }}", Some("kVArh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Reactive Energy L3", "reactive_energy_l3", "{{ value_json.reactive_energy_l3_kvarh | round(3) }}", Some("kVArh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Reactive Energy Total", "reactive_energy_total", "{{ value_json.reactive_energy_total_kvarh | round(3) }}", Some("kVArh"), None, Some("total_increasing"), None, None);
    publish_sensor!("Status",         "mode3_state",     "{{ value_json.mode3_state_name }}",             None,        None,                 None,                None,                  Some("mdi:ev-station"));
    publish_sensor!("Max Current",    "max_current_ro",  "{{ value_json.max_current | round(1) }}",       Some("A"),   Some("current"),      Some("measurement"), None,                  Some("mdi:current-ac"));
    publish_sensor!("Active Max Current", "active_max_current", "{{ value_json.active_max_current | round(1) }}", Some("A"), Some("current"), Some("measurement"), Some("diagnostic"), Some("mdi:current-ac"));
    publish_sensor!("Applied Max Current", "applied_max_current", "{{ value_json.applied_max_current | round(1) }}", Some("A"), Some("current"), Some("measurement"), Some("diagnostic"), Some("mdi:current-ac"));
    publish_sensor!("Active Load Balancing", "active_load_balancing", "{{ value_json.active_load_balancing | round(1) }}", Some("A"), Some("current"), Some("measurement"), Some("diagnostic"), Some("mdi:current-ac"));
    publish_sensor!("Charge Phases",  "phases_raw",      "{{ value_json.phases_raw }}",                   None,        None,                 None,                Some("diagnostic"),    Some("mdi:power-plug"));
    publish_sensor!("Meter State",    "meter_state",     "{{ value_json.meter_state }}",                  None,        None,                 None,                Some("diagnostic"),    Some("mdi:gauge"));
    publish_sensor!("Meter Type",     "meter_type",      "{{ value_json.meter_type }}",                   None,        None,                 None,                Some("diagnostic"),    Some("mdi:lan"));
    publish_sensor!("Meter Last Update", "meter_last_value_ms", "{{ value_json.meter_last_value_ms }}",   Some("ms"),  None,                 None,                Some("diagnostic"),    Some("mdi:timer-sand"));
    publish_sensor!("Last Error",     "last_error",      "{{ value_json.last_error | default('none', true) }}", None, None, None, Some("diagnostic"), Some("mdi:alert-circle-outline"));

    publish_binary_sensor!(
        "Connectivity",
        "connectivity",
        "{% if value_json.comm_ok %}ON{% else %}OFF{% endif %}",
        "ON",
        "OFF",
        Some("connectivity"),
        Some("diagnostic"),
        Some("mdi:lan-connect")
    );

    publish_binary_sensor!(
        "Setpoint Accounted",
        "setpoint_accounted",
        "{% if value_json.setpoint_accounted %}ON{% else %}OFF{% endif %}",
        "ON",
        "OFF",
        None,
        Some("diagnostic"),
        Some("mdi:check-decagram")
    );

    // Number entity (writable max current)
    let cmd_topic = format!("{prefix}/{uid}/set/max_current");
    let number_cfg = HaNumberConfig {
        name: "Max Charge Current",
        unique_id: format!("{uid}_max_current_set"),
        state_topic: &state_topic,
        value_template: "{{ value_json.max_current | round(1) }}".to_string(),
        command_topic: &cmd_topic,
        min: 0.0,
        max: 32.0,
        step: 1.0,
        unit_of_measurement: "A",
        icon: "mdi:current-ac",
        device: device!(),
        availability_topic: &avail_topic,
        payload_available: "online",
        payload_not_available: "offline",
    };
    let number_disc_topic = format!("{ha}/number/{uid}_max_current_set/config");
    client
        .publish(
            number_disc_topic,
            QoS::AtLeastOnce,
            true,
            serde_json::to_vec(&number_cfg)?,
        )
        .await?;

    // Friendly charging switch entity (ON/OFF mapped to current setpoint)
    let charging_cmd_topic = format!("{prefix}/{uid}/set/charging");
    let switch_cfg = HaSwitchConfig {
        name: "Charging",
        unique_id: format!("{uid}_charging"),
        state_topic: &state_topic,
        value_template: "{% if value_json.max_current | float(0) > 0 %}ON{% else %}OFF{% endif %}".to_string(),
        command_topic: &charging_cmd_topic,
        payload_on: "ON",
        payload_off: "OFF",
        icon: Some("mdi:ev-station"),
        device: device!(),
        availability_topic: &avail_topic,
        payload_available: "online",
        payload_not_available: "offline",
    };
    let switch_disc_topic = format!("{ha}/switch/{uid}_charging/config");
    client
        .publish(
            switch_disc_topic,
            QoS::AtLeastOnce,
            true,
            serde_json::to_vec(&switch_cfg)?,
        )
        .await?;

    info!("Published HA discovery config for {} entities", 58);
    Ok(())
}

// ---------------------------------------------------------------------------
// MQTT publishing
// ---------------------------------------------------------------------------

async fn publish_state(
    client: &AsyncClient,
    state: &ChargerState,
    comm_ok: bool,
    last_error: Option<&str>,
    cfg: &Config,
) -> Result<()> {
    let uid = &cfg.charger.unique_id;
    let prefix = &cfg.mqtt.topic_prefix;

    let state_topic = format!("{prefix}/{uid}/state");
    let payload = serde_json::json!({
        "station_name": state.station_name,
        "serial_nr": state.serial_nr,
        "firmware_ver": state.firmware_ver,
        "meter_state": state.meter_state,
        "meter_last_value_ms": state.meter_last_value_ms,
        "meter_type": state.meter_type,
        "voltage_l1": state.voltage_l1,
        "voltage_l2": state.voltage_l2,
        "voltage_l3": state.voltage_l3,
        "voltage_l1_l2": state.voltage_l1_l2,
        "voltage_l2_l3": state.voltage_l2_l3,
        "voltage_l3_l1": state.voltage_l3_l1,
        "current_n": state.current_n,
        "current_l1": state.current_l1,
        "current_l2": state.current_l2,
        "current_l3": state.current_l3,
        "current_sum": state.current_sum,
        "power_factor_l1": state.power_factor_l1,
        "power_factor_l2": state.power_factor_l2,
        "power_factor_l3": state.power_factor_l3,
        "power_l1": state.power_l1,
        "power_l2": state.power_l2,
        "power_l3": state.power_l3,
        "power_total": state.power_total,
        "power_factor": state.power_factor,
        "frequency_hz": state.frequency_hz,
        "apparent_power_l1": state.apparent_power_l1,
        "apparent_power_l2": state.apparent_power_l2,
        "apparent_power_l3": state.apparent_power_l3,
        "apparent_power_total": state.apparent_power_total,
        "reactive_power_l1": state.reactive_power_l1,
        "reactive_power_l2": state.reactive_power_l2,
        "reactive_power_l3": state.reactive_power_l3,
        "reactive_power_total": state.reactive_power_total,
        "energy_delivered_l1_kwh": state.energy_delivered_l1_kwh,
        "energy_delivered_l2_kwh": state.energy_delivered_l2_kwh,
        "energy_delivered_l3_kwh": state.energy_delivered_l3_kwh,
        "energy_total_kwh": state.energy_total_kwh,
        "energy_consumed_l1_kwh": state.energy_consumed_l1_kwh,
        "energy_consumed_l2_kwh": state.energy_consumed_l2_kwh,
        "energy_consumed_l3_kwh": state.energy_consumed_l3_kwh,
        "energy_consumed_total_kwh": state.energy_consumed_total_kwh,
        "apparent_energy_l1_kvah": state.apparent_energy_l1_kvah,
        "apparent_energy_l2_kvah": state.apparent_energy_l2_kvah,
        "apparent_energy_l3_kvah": state.apparent_energy_l3_kvah,
        "apparent_energy_total_kvah": state.apparent_energy_total_kvah,
        "reactive_energy_l1_kvarh": state.reactive_energy_l1_kvarh,
        "reactive_energy_l2_kvarh": state.reactive_energy_l2_kvarh,
        "reactive_energy_l3_kvarh": state.reactive_energy_l3_kvarh,
        "reactive_energy_total_kvarh": state.reactive_energy_total_kvarh,
        "available": state.available,
        "mode3_state": state.mode3_state,
        "mode3_state_name": state.mode3_state_name,
        "max_current": state.max_current,
        "active_max_current": state.active_max_current,
        "applied_max_current": state.applied_max_current,
        "active_load_balancing": state.active_load_balancing,
        "phases_raw": state.phases_raw,
        "setpoint_accounted_raw": state.setpoint_accounted_raw,
        "setpoint_accounted": state.setpoint_accounted,
        "comm_ok": comm_ok,
        "last_error": last_error,
        "last_update_epoch_s": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    })
    .to_string();

    client
        .publish(state_topic, QoS::AtLeastOnce, true, payload)
        .await?;

    Ok(())
}

async fn publish_error_state(
    client: &AsyncClient,
    error: &str,
    cfg: &Config,
) -> Result<()> {
    let uid = &cfg.charger.unique_id;
    let prefix = &cfg.mqtt.topic_prefix;
    let state_topic = format!("{prefix}/{uid}/state");

    let payload = serde_json::json!({
        "comm_ok": false,
        "last_error": error,
        "last_update_epoch_s": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    })
    .to_string();

    client
        .publish(state_topic, QoS::AtLeastOnce, true, payload)
        .await?;

    Ok(())
}

async fn publish_availability(
    client: &AsyncClient,
    online: bool,
    cfg: &Config,
) -> Result<()> {
    let uid = &cfg.charger.unique_id;
    let prefix = &cfg.mqtt.topic_prefix;
    let avail_topic = format!("{prefix}/{uid}/availability");
    let payload = if online { "online" } else { "offline" };
    client
        .publish(avail_topic, QoS::AtLeastOnce, true, payload)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Command handler (set max current via MQTT)
// ---------------------------------------------------------------------------

async fn handle_set_max_current(
    ctx: &mut tokio_modbus::client::Context,
    value_str: &str,
    control_slave: u8,
    takeover: TakeoverSettings,
) -> Result<f32> {
    fn parse_max_current_amps(value_str: &str) -> Result<f32> {
        let trimmed = value_str.trim();

        // Most HA MQTT payloads are plain text numbers ("10" or "10.0").
        if let Ok(v) = trimmed.parse::<f32>() {
            return Ok(v);
        }

        // Accept quoted numeric strings (e.g. "\"10.0\"").
        if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
            let inner = &trimmed[1..trimmed.len() - 1];
            if let Ok(v) = inner.trim().parse::<f32>() {
                return Ok(v);
            }
        }

        // Accept JSON payloads where value may be number or string.
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
            match json {
                serde_json::Value::Number(n) => {
                    if let Some(v) = n.as_f64() {
                        return Ok(v as f32);
                    }
                }
                serde_json::Value::String(s) => {
                    if let Ok(v) = s.trim().parse::<f32>() {
                        return Ok(v);
                    }
                }
                _ => {}
            }
        }

        anyhow::bail!("Invalid ampere value: {value_str}")
    }

    let amps = parse_max_current_amps(value_str)?;

    if !(0.0..=32.0).contains(&amps) {
        anyhow::bail!("Max current {amps}A out of range [0, 32]");
    }

    write_setpoint_with_takeover(ctx, control_slave, amps, takeover, "set_max_current").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let readback = log_direct_max_current_readback(ctx, "after_set_max_current").await;
    let remaining_valid_time = read_optional_u32_with_fallback(
        ctx,
        reg::MAX_CURRENT_VALID_TIME,
        "max_current_remaining_valid_time",
    )
    .await;
    let accounted_raw = read_setpoint_accounted(ctx).await;
    let applied_limit = read_applied_max_current(ctx).await;
    log_remaining_valid_time(remaining_valid_time, "after_set_max_current");
    log_setpoint_accounted_proof(accounted_raw, "after_set_max_current");
    log_applied_limit_hint(amps, applied_limit, "after_set_max_current");
    match readback {
        Some(applied) if (applied - amps).abs() <= 0.2 => {
            info!("Set max current requested={amps:.1}A applied={applied:.1}A");
        }
        Some(applied) => {
            warn!(
                "Set max current mismatch: requested={amps:.1}A applied={applied:.1}A (charger overruled or different register semantics)"
            );
        }
        None => {
            warn!("Set max current requested={amps:.1}A, but direct readback was unavailable");
        }
    }
    Ok(amps)
}

async fn handle_set_charging(
    ctx: &mut tokio_modbus::client::Context,
    value_str: &str,
    control_slave: u8,
    takeover: TakeoverSettings,
) -> Result<f32> {
    // Friendly switch mapping: OFF -> 0A, ON -> 6A (minimum practical charging current).
    let normalized = value_str.trim().to_ascii_uppercase();
    let amps = match normalized.as_str() {
        "ON" | "1" | "TRUE" => 6.0,
        "OFF" | "0" | "FALSE" => 0.0,
        other => anyhow::bail!("Invalid charging switch payload: {other} (expected ON/OFF)"),
    };

    write_setpoint_with_takeover(ctx, control_slave, amps, takeover, "set_charging").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let readback = log_direct_max_current_readback(ctx, "after_set_charging").await;
    let remaining_valid_time = read_optional_u32_with_fallback(
        ctx,
        reg::MAX_CURRENT_VALID_TIME,
        "max_current_remaining_valid_time",
    )
    .await;
    let accounted_raw = read_setpoint_accounted(ctx).await;
    let applied_limit = read_applied_max_current(ctx).await;
    log_remaining_valid_time(remaining_valid_time, "after_set_charging");
    log_setpoint_accounted_proof(accounted_raw, "after_set_charging");
    log_applied_limit_hint(amps, applied_limit, "after_set_charging");
    match readback {
        Some(applied) if (applied - amps).abs() <= 0.2 => {
            info!("Set charging={} requested={amps:.1}A applied={applied:.1}A", normalized);
        }
        Some(applied) => {
            warn!(
                "Set charging={} mismatch: requested={amps:.1}A applied={applied:.1}A (charger overruled or different register semantics)",
                normalized
            );
        }
        None => {
            warn!(
                "Set charging={} requested={amps:.1}A, but direct readback was unavailable",
                normalized
            );
        }
    }
    Ok(amps)
}

enum CommandMessage {
    SetMaxCurrent(String),
    SetCharging(String),
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing setup
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "alfen_mqtt=info".parse().unwrap()),
        )
        .init();

    // Config
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    info!("Loading config from {config_path}");
    let cfg = load_config(&config_path)?;
    let socket_slave = cfg.alfen.socket_slave();
    let station_slave = cfg.alfen.station_slave();
    let control_slave = cfg.alfen.control_slave();
    let takeover = TakeoverSettings {
        enabled: cfg.alfen.takeover_enabled(),
        safe_current_amps: cfg.alfen.takeover_safe_current_amps(),
        safe_current_register: cfg.alfen.takeover_safe_current_register(),
        validity_secs: cfg.alfen.takeover_validity_secs(),
        validity_register: cfg.alfen.takeover_validity_register(), // None unless explicitly set in config
    };

    // ----- MQTT setup -----
    let mqtt_client_id = effective_mqtt_client_id(&cfg.mqtt.client_id);
    info!(
        "MQTT client id: '{}' (base='{}')",
        mqtt_client_id,
        cfg.mqtt.client_id
    );
    let mut mqtt_opts = MqttOptions::new(
        &mqtt_client_id,
        &cfg.mqtt.host,
        cfg.mqtt.port,
    );
    mqtt_opts.set_keep_alive(Duration::from_secs(30));
    mqtt_opts.set_clean_session(true);

    if let (Some(user), Some(pass)) = (&cfg.mqtt.username, &cfg.mqtt.password) {
        if !user.is_empty() {
            mqtt_opts.set_credentials(user, pass);
        }
    }

    // LWT so HA marks the device offline if we crash
    let uid = &cfg.charger.unique_id;
    let prefix = &cfg.mqtt.topic_prefix;
    let avail_topic = format!("{prefix}/{uid}/availability");
    mqtt_opts.set_last_will(rumqttc::LastWill::new(
        &avail_topic,
        "offline",
        QoS::AtLeastOnce,
        true,
    ));

    let (mqtt_client, mut event_loop) = AsyncClient::new(mqtt_opts, 64);

    // Subscribe to command topic
    let cmd_topic = format!("{prefix}/{uid}/set/max_current");
    let charging_cmd_topic = format!("{prefix}/{uid}/set/charging");
    mqtt_client
        .subscribe(&cmd_topic, QoS::AtLeastOnce)
        .await?;
    mqtt_client
        .subscribe(&charging_cmd_topic, QoS::AtLeastOnce)
        .await?;

    // ----- Modbus TCP setup -----
    let modbus_addr: std::net::SocketAddr = format!("{}:{}", cfg.alfen.host, cfg.alfen.port)
        .parse()
        .context("Invalid Alfen host/port")?;

    info!(
        "Connecting to Alfen Eve at {} (socket unit {}, station unit {}, control unit {}, takeover={})",
        modbus_addr,
        socket_slave,
        station_slave,
        control_slave,
        takeover.enabled
    );

    let mut modbus_ctx =
        tcp::connect_slave(modbus_addr, Slave(socket_slave)).await
        .context("Failed to connect to Alfen Modbus TCP")?;

    info!("Modbus connected");

    // Initial read to get firmware version for HA discovery.
    // Keep running even when the first poll fails, so reconnect logic can recover.
    let initial_state = match read_charger(&mut modbus_ctx, socket_slave, station_slave).await {
        Ok(state) => {
            info!(
                "Charger: {} / {} / fw {}",
                state.station_name, state.serial_nr, state.firmware_ver
            );
            state
        }
        Err(e) => {
            warn!("Initial Modbus read failed: {e}. Continuing with placeholder state.");
            ChargerState::default()
        }
    };

    // Publish HA discovery config (retained, only needed once but harmless to repeat)
    publish_ha_discovery(&mqtt_client, &cfg, &initial_state.firmware_ver).await?;
    publish_availability(&mqtt_client, true, &cfg).await?;

    // ----- Main loop -----
    let poll_interval = Duration::from_secs(cfg.alfen.poll_interval_secs);
    let mut poll_ticker = time::interval(poll_interval);
    let setpoint_heartbeat_secs = cfg.alfen.setpoint_heartbeat_secs.unwrap_or(30).max(5);
    let mut setpoint_heartbeat_ticker = time::interval(Duration::from_secs(setpoint_heartbeat_secs));
    // Skip the first immediate tick.
    setpoint_heartbeat_ticker.tick().await;
    // Last value requested via MQTT command. Re-sent periodically to satisfy charger watchdog.
    let mut desired_max_current: Option<f32> = None;

    // We drive the MQTT event loop in a background task via a channel.
    // Commands come in through the MQTT eventloop; we forward them via a channel.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<CommandMessage>(16);
    let cmd_topic_clone = cmd_topic.clone();
    let charging_cmd_topic_clone = charging_cmd_topic.clone();

    // MQTT event loop task
    tokio::spawn(async move {
        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    if p.topic == cmd_topic_clone {
                        let payload = String::from_utf8_lossy(&p.payload).to_string();
                        if cmd_tx.send(CommandMessage::SetMaxCurrent(payload)).await.is_err() {
                            break;
                        }
                    } else if p.topic == charging_cmd_topic_clone {
                        let payload = String::from_utf8_lossy(&p.payload).to_string();
                        if cmd_tx.send(CommandMessage::SetCharging(payload)).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("MQTT event loop error: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });

    info!(
        "Bridge running. Polling every {}s, setpoint heartbeat every {}s",
        cfg.alfen.poll_interval_secs,
        setpoint_heartbeat_secs
    );

    loop {
        tokio::select! {
            _ = poll_ticker.tick() => {
                match read_charger(&mut modbus_ctx, socket_slave, station_slave).await {
                    Ok(state) => {
                        if let Err(e) = publish_state(&mqtt_client, &state, true, None, &cfg).await {
                            error!("Failed to publish state: {e}");
                        } else {
                            info!(
                                "Published: {:.0}W total, {:.1}A L1/{:.1}A L2/{:.1}A L3, {:.3}kWh, state={}, max_current={:.1}A, active_max_current={:?}, applied_max_current={:?}, setpoint_accounted={:?}",
                                state.power_total,
                                state.current_l1, state.current_l2, state.current_l3,
                                state.energy_total_kwh,
                                state.mode3_state_name,
                                state.max_current,
                                state.active_max_current,
                                state.applied_max_current,
                                state.setpoint_accounted_raw
                            );
                        }
                    }
                    Err(e) => {
                        error!("Modbus read error: {e}");
                        publish_error_state(&mqtt_client, &format!("modbus_read_error: {e}"), &cfg).await.ok();
                        // Try to reconnect
                        publish_availability(&mqtt_client, false, &cfg).await.ok();
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        match tcp::connect_slave(modbus_addr, Slave(socket_slave)).await {
                            Ok(new_ctx) => {
                                modbus_ctx = new_ctx;
                                publish_availability(&mqtt_client, true, &cfg).await.ok();
                                info!("Modbus reconnected");
                            }
                            Err(e) => error!("Reconnect failed: {e}"),
                        }
                    }
                }
            }

            _ = setpoint_heartbeat_ticker.tick() => {
                if let Some(amps) = desired_max_current {
                    if let Err(e) = write_setpoint_with_takeover(&mut modbus_ctx, control_slave, amps, takeover, "setpoint_heartbeat").await {
                        warn!("Setpoint heartbeat write failed for {:.1}A: {e}", amps);
                        publish_error_state(&mqtt_client, &format!("setpoint_heartbeat_error: {e}"), &cfg).await.ok();
                    } else {
                        info!("Setpoint heartbeat refreshed {:.1}A", amps);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let readback = log_direct_max_current_readback(&mut modbus_ctx, "heartbeat").await;
                        let remaining_valid_time = read_optional_u32_with_fallback(
                            &mut modbus_ctx,
                            reg::MAX_CURRENT_VALID_TIME,
                            "max_current_remaining_valid_time",
                        )
                        .await;
                        let accounted_raw = read_setpoint_accounted(&mut modbus_ctx).await;
                        let applied_limit = read_applied_max_current(&mut modbus_ctx).await;
                        log_remaining_valid_time(remaining_valid_time, "heartbeat");
                        log_setpoint_accounted_proof(accounted_raw, "heartbeat");
                        log_applied_limit_hint(amps, applied_limit, "heartbeat");
                        if let Some(applied) = readback {
                            if (applied - amps).abs() > 0.2 {
                                warn!(
                                    "Setpoint heartbeat mismatch: requested={amps:.1}A applied={applied:.1}A"
                                );
                            }
                        }
                    }
                }
            }

            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    CommandMessage::SetMaxCurrent(cmd_payload) => {
                        info!("Received set_max_current command: '{cmd_payload}'");
                        match handle_set_max_current(&mut modbus_ctx, &cmd_payload, control_slave, takeover).await {
                            Ok(amps) => {
                                desired_max_current = Some(amps);
                                if let Ok(state) = read_charger(&mut modbus_ctx, socket_slave, station_slave).await {
                                    if let Err(e) = publish_state(&mqtt_client, &state, true, None, &cfg).await {
                                        warn!("Set max current succeeded but failed to publish immediate state: {e}");
                                    }
                                }
                            }
                            Err(e) => {
                            error!("Failed to set max current: {e}");
                            publish_error_state(&mqtt_client, &format!("set_max_current_error: {e}"), &cfg).await.ok();
                            }
                        }
                    }
                    CommandMessage::SetCharging(cmd_payload) => {
                        info!("Received set_charging command: '{cmd_payload}'");
                        match handle_set_charging(&mut modbus_ctx, &cmd_payload, control_slave, takeover).await {
                            Ok(amps) => {
                                desired_max_current = Some(amps);
                                if let Ok(state) = read_charger(&mut modbus_ctx, socket_slave, station_slave).await {
                                    if let Err(e) = publish_state(&mqtt_client, &state, true, None, &cfg).await {
                                        warn!("Set charging succeeded but failed to publish immediate state: {e}");
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to set charging switch: {e}");
                                publish_error_state(&mqtt_client, &format!("set_charging_error: {e}"), &cfg).await.ok();
                            }
                        }
                    }
                }
            }
        }
    }
}