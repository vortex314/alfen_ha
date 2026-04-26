use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct Config {
    mqtt: MqttConfig,
    charger: ChargerConfig,
    #[serde(default)]
    solar_control: SolarControlConfig,
}

#[derive(Debug, Deserialize)]
struct MqttConfig {
    host: String,
    port: u16,
    client_id: String,
    username: Option<String>,
    password: Option<String>,
    topic_prefix: String,
    #[serde(default = "default_ha_discovery_prefix")]
    ha_discovery_prefix: String,
}

#[derive(Debug, Deserialize)]
struct ChargerConfig {
    #[serde(default = "default_charger_name")]
    name: String,
    unique_id: String,
}

#[derive(Debug, Deserialize)]
struct SolarControlConfig {
    #[serde(default = "default_power_topic")]
    power_topic: String,
    #[serde(default)]
    external_mode_topic: Option<String>,
    #[serde(default)]
    mode_command_topic: Option<String>,
    #[serde(default)]
    mode_state_topic: Option<String>,
    #[serde(default)]
    debug_state_topic: Option<String>,
    #[serde(default = "default_threshold_kw")]
    power_threshold_kw: f64,
    #[serde(default = "default_switch_high_after_secs")]
    switch_high_after_secs: u64,
    #[serde(default = "default_switch_low_after_secs")]
    switch_low_after_secs: u64,
    #[serde(default = "default_high_current_amps")]
    solar_high_current_amps: f32,
    #[serde(default = "default_low_current_amps")]
    solar_low_current_amps: f32,
    #[serde(default = "default_fixed_current_amps")]
    fixed_current_amps: f32,
    #[serde(default = "default_mode_on_payload")]
    mode_on_payload: String,
    #[serde(default = "default_mode_off_payload")]
    mode_off_payload: String,
    #[serde(default = "default_publish_on_change")]
    publish_only_on_change: bool,
    #[serde(default = "default_publish_ha_discovery")]
    publish_ha_discovery: bool,
}

impl Default for SolarControlConfig {
    fn default() -> Self {
        Self {
            power_topic: default_power_topic(),
            external_mode_topic: None,
            mode_command_topic: None,
            mode_state_topic: None,
            debug_state_topic: None,
            power_threshold_kw: default_threshold_kw(),
            switch_high_after_secs: default_switch_high_after_secs(),
            switch_low_after_secs: default_switch_low_after_secs(),
            solar_high_current_amps: default_high_current_amps(),
            solar_low_current_amps: default_low_current_amps(),
            fixed_current_amps: default_fixed_current_amps(),
            mode_on_payload: default_mode_on_payload(),
            mode_off_payload: default_mode_off_payload(),
            publish_only_on_change: default_publish_on_change(),
            publish_ha_discovery: default_publish_ha_discovery(),
        }
    }
}

fn default_ha_discovery_prefix() -> String {
    "homeassistant".to_string()
}

fn default_charger_name() -> String {
    "Alfen Eve".to_string()
}

fn default_power_topic() -> String {
    "homeassistant/sensor/esp_p1/dsmr_reader_power_returned/state".to_string()
}

fn default_threshold_kw() -> f64 {
    5.0
}

fn default_switch_high_after_secs() -> u64 {
    300
}

fn default_switch_low_after_secs() -> u64 {
    300
}

fn default_high_current_amps() -> f32 {
    6.0
}

fn default_low_current_amps() -> f32 {
    4.0
}

fn default_fixed_current_amps() -> f32 {
    6.0
}

fn default_mode_on_payload() -> String {
    "ON".to_string()
}

fn default_mode_off_payload() -> String {
    "OFF".to_string()
}

fn default_publish_on_change() -> bool {
    true
}

fn default_publish_ha_discovery() -> bool {
    true
}

fn load_config(path: &str) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;
    toml::from_str(&content).context("Failed to parse config.toml")
}

fn parse_power_text(raw: &str) -> Option<f64> {
    let trimmed = raw.trim().trim_matches('"');

    if let Ok(v) = trimmed.parse::<f64>() {
        return Some(v);
    }

    let lower = trimmed.to_ascii_lowercase();

    if let Some(value) = lower.strip_suffix("kw") {
        return value.trim().parse::<f64>().ok();
    }

    if let Some(value) = lower.strip_suffix('w') {
        return value.trim().parse::<f64>().ok().map(|v| v / 1000.0);
    }

    None
}

fn parse_power_json(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => parse_power_text(s),
        serde_json::Value::Object(map) => {
            for key in ["state", "value", "power_kw", "power", "returned", "surplus_kw"] {
                if let Some(parsed) = map.get(key).and_then(parse_power_json) {
                    return Some(parsed);
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_power_kw(payload: &[u8]) -> Option<f64> {
    let raw = std::str::from_utf8(payload).ok()?.trim();

    if let Some(v) = parse_power_text(raw) {
        return Some(v);
    }

    let json: serde_json::Value = serde_json::from_str(raw).ok()?;
    parse_power_json(&json)
}

fn parse_mode(payload: &[u8], mode_on_payload: &str) -> Option<bool> {
    let s = std::str::from_utf8(payload).ok()?.trim();
    if s.eq_ignore_ascii_case(mode_on_payload) {
        return Some(true);
    }

    if s.eq_ignore_ascii_case("ON")
        || s.eq_ignore_ascii_case("TRUE")
        || s.eq_ignore_ascii_case("1")
        || s.eq_ignore_ascii_case("SOLAR")
    {
        return Some(true);
    }

    if s.eq_ignore_ascii_case("OFF")
        || s.eq_ignore_ascii_case("FALSE")
        || s.eq_ignore_ascii_case("0")
        || s.eq_ignore_ascii_case("FIXED")
    {
        return Some(false);
    }

    None
}

async fn publish_ha_discovery(
    client: &AsyncClient,
    cfg: &Config,
    mode_command_topic: &str,
    mode_state_topic: &str,
    debug_state_topic: &str,
) -> Result<()> {
    let uid = &cfg.charger.unique_id;
    let switch_discovery_topic = format!(
        "{}/switch/{}_solar_charging_mode/config",
        cfg.mqtt.ha_discovery_prefix, uid
    );

    let payload = json!({
        "name": "Solar Charging Mode",
        "unique_id": format!("{}_solar_charging_mode", uid),
        "object_id": format!("{}_solar_charging_mode", uid),
        "state_topic": mode_state_topic,
        "command_topic": mode_command_topic,
        "payload_on": cfg.solar_control.mode_on_payload,
        "payload_off": cfg.solar_control.mode_off_payload,
        "state_on": cfg.solar_control.mode_on_payload,
        "state_off": cfg.solar_control.mode_off_payload,
        "icon": "mdi:solar-power",
        "device": {
            "identifiers": [uid],
            "name": cfg.charger.name,
            "manufacturer": "Alfen",
            "model": "Eve",
        }
    })
    .to_string();

    client
        .publish(switch_discovery_topic, QoS::AtLeastOnce, true, payload)
        .await
        .context("failed to publish HA discovery for solar mode switch")?;

    let debug_discovery_topic = format!(
        "{}/sensor/{}_solar_debug/config",
        cfg.mqtt.ha_discovery_prefix, uid
    );

    let debug_payload = json!({
        "name": "Solar Charging Debug",
        "unique_id": format!("{}_solar_debug", uid),
        "object_id": format!("{}_solar_debug", uid),
        "state_topic": debug_state_topic,
        "json_attributes_topic": debug_state_topic,
        "value_template": "{{ value_json.summary }}",
        "icon": "mdi:chart-timeline-variant",
        "entity_category": "diagnostic",
        "device": {
            "identifiers": [uid],
            "name": cfg.charger.name,
            "manufacturer": "Alfen",
            "model": "Eve",
        }
    })
    .to_string();

    client
        .publish(debug_discovery_topic, QoS::AtLeastOnce, true, debug_payload)
        .await
        .context("failed to publish HA discovery for solar debug sensor")?;

    Ok(())
}

async fn publish_mode_state(client: &AsyncClient, topic: &str, solar_mode: bool, cfg: &Config) -> Result<()> {
    let payload = if solar_mode {
        cfg.solar_control.mode_on_payload.clone()
    } else {
        cfg.solar_control.mode_off_payload.clone()
    };

    client
        .publish(topic, QoS::AtLeastOnce, true, payload)
        .await
        .context("failed to publish solar mode state")?;

    Ok(())
}

async fn publish_debug_state(
    client: &AsyncClient,
    topic: &str,
    cfg: &Config,
    solar_mode: bool,
    power_kw: Option<f64>,
    high_since: Option<Instant>,
    low_since: Option<Instant>,
    target_current: f32,
    last_published: Option<f32>,
) -> Result<()> {
    let now = Instant::now();

    let high_elapsed_s = high_since.map(|t| now.duration_since(t).as_secs());
    let low_elapsed_s = low_since.map(|t| now.duration_since(t).as_secs());
    let high_remaining_s = if solar_mode {
        Some(cfg.solar_control.switch_high_after_secs.saturating_sub(high_elapsed_s.unwrap_or(0)))
    } else {
        None
    };
    let low_remaining_s = if solar_mode {
        Some(cfg.solar_control.switch_low_after_secs.saturating_sub(low_elapsed_s.unwrap_or(0)))
    } else {
        None
    };

    let mode_text = if solar_mode { "solar" } else { "fixed" };
    let power_text = power_kw
        .map(|v| format!("{v:.3}kW"))
        .unwrap_or_else(|| "n/a".to_string());
    let summary = format!(
        "mode={}, power={}, target={:.1}A, hi_rem={}s, lo_rem={}s",
        mode_text,
        power_text,
        target_current,
        high_remaining_s.unwrap_or(0),
        low_remaining_s.unwrap_or(0)
    );

    let payload = json!({
        "summary": summary,
        "mode": mode_text,
        "power_kw": power_kw,
        "threshold_kw": cfg.solar_control.power_threshold_kw,
        "target_current_a": target_current,
        "last_published_current_a": last_published,
        "switch_high_after_secs": cfg.solar_control.switch_high_after_secs,
        "switch_low_after_secs": cfg.solar_control.switch_low_after_secs,
        "high_elapsed_secs": high_elapsed_s,
        "low_elapsed_secs": low_elapsed_s,
        "high_remaining_secs": high_remaining_s,
        "low_remaining_secs": low_remaining_s,
    })
    .to_string();

    client
        .publish(topic, QoS::AtLeastOnce, true, payload)
        .await
        .context("failed to publish solar debug state")?;

    Ok(())
}

fn target_current(
    cfg: &SolarControlConfig,
    solar_mode: bool,
    power_kw: Option<f64>,
    high_since: &mut Option<Instant>,
    low_since: &mut Option<Instant>,
    previous_target: Option<f32>,
) -> f32 {
    if !solar_mode {
        *high_since = None;
        *low_since = None;
        return cfg.fixed_current_amps;
    }

    let now = Instant::now();
    let current_target = previous_target.unwrap_or(cfg.solar_low_current_amps);

    match power_kw {
        Some(v) if v > cfg.power_threshold_kw => {
            if high_since.is_none() {
                *high_since = Some(now);
            }
            *low_since = None;

            let held_long_enough = high_since
                .map(|t| now.duration_since(t) >= Duration::from_secs(cfg.switch_high_after_secs))
                .unwrap_or(false);

            if held_long_enough {
                cfg.solar_high_current_amps
            } else {
                current_target
            }
        }
        Some(_) => {
            if low_since.is_none() {
                *low_since = Some(now);
            }
            *high_since = None;

            let held_long_enough = low_since
                .map(|t| now.duration_since(t) >= Duration::from_secs(cfg.switch_low_after_secs))
                .unwrap_or(false);

            if held_long_enough {
                cfg.solar_low_current_amps
            } else {
                current_target
            }
        }
        None => {
            *high_since = None;
            *low_since = None;
            current_target
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "alfen_solar=info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = load_config(&config_path)?;

    let set_topic = format!(
        "{}/{}/set/max_current",
        cfg.mqtt.topic_prefix, cfg.charger.unique_id
    );
    let mode_command_topic = cfg
        .solar_control
        .mode_command_topic
        .clone()
        .unwrap_or_else(|| format!("{}/{}/set/solar_mode", cfg.mqtt.topic_prefix, cfg.charger.unique_id));
    let mode_state_topic = cfg
        .solar_control
        .mode_state_topic
        .clone()
        .unwrap_or_else(|| format!("{}/{}/state/solar_mode", cfg.mqtt.topic_prefix, cfg.charger.unique_id));
    let debug_state_topic = cfg
        .solar_control
        .debug_state_topic
        .clone()
        .unwrap_or_else(|| format!("{}/{}/state/solar_debug", cfg.mqtt.topic_prefix, cfg.charger.unique_id));

    let mut mqttoptions = MqttOptions::new(
        format!("{}-alfen_solar", cfg.mqtt.client_id),
        cfg.mqtt.host.clone(),
        cfg.mqtt.port,
    );

    if let Some(username) = &cfg.mqtt.username {
        if !username.is_empty() {
            mqttoptions.set_credentials(username.clone(), cfg.mqtt.password.clone().unwrap_or_default());
        }
    }
    mqttoptions.set_keep_alive(Duration::from_secs(30));

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    client
        .subscribe(cfg.solar_control.power_topic.clone(), QoS::AtLeastOnce)
        .await
        .context("subscribe power topic failed")?;
    client
        .subscribe(mode_command_topic.clone(), QoS::AtLeastOnce)
        .await
        .context("subscribe mode command topic failed")?;

    if let Some(topic) = &cfg.solar_control.external_mode_topic {
        client
            .subscribe(topic.clone(), QoS::AtLeastOnce)
            .await
            .context("subscribe external mode topic failed")?;
    }

    if cfg.solar_control.publish_ha_discovery {
        publish_ha_discovery(
            &client,
            &cfg,
            &mode_command_topic,
            &mode_state_topic,
            &debug_state_topic,
        )
        .await?;
    }

    info!(
        "alfen_solar started, power='{}', mode_command='{}', mode_state='{}', debug_state='{}', external_mode={:?}",
        cfg.solar_control.power_topic,
        mode_command_topic,
        mode_state_topic,
        debug_state_topic,
        cfg.solar_control.external_mode_topic
    );

    let mut solar_mode = false;
    let mut power_kw: Option<f64> = None;
    let mut last_published: Option<f32> = None;
    let mut high_since: Option<Instant> = None;
    let mut low_since: Option<Instant> = None;

    publish_mode_state(&client, &mode_state_topic, solar_mode, &cfg).await?;
    publish_debug_state(
        &client,
        &debug_state_topic,
        &cfg,
        solar_mode,
        power_kw,
        high_since,
        low_since,
        cfg.solar_control.fixed_current_amps,
        last_published,
    )
    .await?;

    loop {
        let event = eventloop.poll().await?;
        let packet = match event {
            Event::Incoming(Incoming::Publish(p)) => p,
            Event::Incoming(_) | Event::Outgoing(_) => continue,
        };

        if packet.topic == cfg.solar_control.power_topic {
            if let Some(v) = parse_power_kw(&packet.payload) {
                power_kw = Some(v);
                info!("Power update: {v:.3} kW");
            } else {
                warn!("Ignoring unparsable power payload: {:?}", packet.payload);
                continue;
            }
        } else if packet.topic == mode_command_topic
            || cfg
                .solar_control
                .external_mode_topic
                .as_ref()
                .map(|t| *t == packet.topic)
                .unwrap_or(false)
        {
            if let Some(mode) = parse_mode(&packet.payload, &cfg.solar_control.mode_on_payload) {
                if solar_mode != mode {
                    solar_mode = mode;
                    publish_mode_state(&client, &mode_state_topic, solar_mode, &cfg).await?;
                }
                info!("Mode update: solar_mode={solar_mode}");
            } else {
                warn!("Ignoring unparsable mode payload: {:?}", packet.payload);
                continue;
            }
        } else {
            continue;
        }

        let target = target_current(
            &cfg.solar_control,
            solar_mode,
            power_kw,
            &mut high_since,
            &mut low_since,
            last_published,
        );

        publish_debug_state(
            &client,
            &debug_state_topic,
            &cfg,
            solar_mode,
            power_kw,
            high_since,
            low_since,
            target,
            last_published,
        )
        .await?;

        if cfg.solar_control.publish_only_on_change
            && last_published
                .map(|prev| (prev - target).abs() < 0.01)
                .unwrap_or(false)
        {
            continue;
        }

        let payload = format!("{target:.1}");
        client
            .publish(set_topic.clone(), QoS::AtLeastOnce, false, payload.clone())
            .await
            .with_context(|| format!("failed to publish max_current to topic '{set_topic}'"))?;

        last_published = Some(target);
        info!(
            "Published max_current={}A (solar_mode={}, power_kw={:?})",
            payload, solar_mode, power_kw
        );
    }
}
