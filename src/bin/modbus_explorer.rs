use anyhow::{anyhow, Context, Result};
use eframe::{egui, App, NativeOptions};
use egui_extras::{Column, TableBuilder};
use serde::Deserialize;
use std::fs;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use tokio_modbus::client::{self, Context as ModbusContext};
use tokio_modbus::prelude::*;

#[derive(Debug, Clone, Deserialize)]
struct ExplorerConfig {
    tcp: TcpConfig,
    registers: Vec<RegisterConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct TcpConfig {
    host: String,
    port: u16,
    unit_id: u8,
    #[serde(default = "default_poll_interval_ms")]
    poll_interval_ms: u64,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct RegisterConfig {
    name: String,
    address: u16,
    #[serde(default)]
    unit_id: Option<u8>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    writable: bool,
    #[serde(default)]
    scale: Option<f64>,
    #[serde(default)]
    length: Option<u16>,
    #[serde(default)]
    register_type: RegisterType,
    data_type: DataType,
    #[serde(default)]
    endian: Endian,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DataType {
    U16,
    I16,
    U32,
    I32,
    U64,
    F32,
    F64,
    Bool,
    String,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Endian {
    #[default]
    Big,
    Little,
    WordSwap,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum RegisterType {
    #[default]
    Holding,
    Input,
}

#[derive(Debug, Clone)]
struct RowState {
    value: String,
    raw: Vec<u16>,
    error: Option<String>,
    last_updated: Option<Instant>,
}

impl Default for RowState {
    fn default() -> Self {
        Self {
            value: "-".to_string(),
            raw: Vec::new(),
            error: None,
            last_updated: None,
        }
    }
}

#[derive(Debug)]
enum WorkerCommand {
    Write { index: usize, value_text: String },
}

#[derive(Debug)]
enum WorkerEvent {
    Snapshot {
        rows: Vec<RowState>,
        connection_error: Option<String>,
    },
    WriteResult {
        register_name: String,
        result: Result<(), String>,
    },
}

fn default_poll_interval_ms() -> u64 {
    1000
}

fn default_timeout_ms() -> u64 {
    2500
}

fn parse_config(path: &str) -> Result<ExplorerConfig> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;
    let cfg: ExplorerConfig =
        serde_yaml::from_str(&body).with_context(|| format!("Failed to parse YAML: {path}"))?;
    if cfg.registers.is_empty() {
        return Err(anyhow!("Config must contain at least one register"));
    }
    Ok(cfg)
}

fn register_word_count(reg: &RegisterConfig) -> u16 {
    match reg.data_type {
        DataType::U16 | DataType::I16 | DataType::Bool => 1,
        DataType::U32 | DataType::I32 | DataType::F32 => 2,
        DataType::U64 | DataType::F64 => 4,
        DataType::String => reg.length.unwrap_or(1).max(1),
    }
}

fn effective_unit_id(reg: &RegisterConfig, default_unit_id: u8) -> u8 {
    reg.unit_id.unwrap_or(default_unit_id)
}

async fn connect_context(cfg: &TcpConfig) -> Result<ModbusContext> {
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .context("Invalid host/port in config")?;
    let mut ctx = tokio::time::timeout(Duration::from_millis(cfg.timeout_ms), client::tcp::connect(addr))
        .await
        .context("Connection timeout")?
        .context("Modbus connect failed")?;
    ctx.set_slave(Slave(cfg.unit_id));
    Ok(ctx)
}

async fn read_single_register(
    ctx: &mut ModbusContext,
    reg: &RegisterConfig,
    default_unit_id: u8,
) -> Result<Vec<u16>> {
    let unit_id = effective_unit_id(reg, default_unit_id);
    ctx.set_slave(Slave(unit_id));
    let qty = register_word_count(reg);
    let primary = match reg.register_type {
        RegisterType::Holding => ctx.read_holding_registers(reg.address, qty).await,
        RegisterType::Input => ctx.read_input_registers(reg.address, qty).await,
    };

    match primary {
        Ok(words) => Ok(words),
        Err(e) => {
            let msg = e.to_string();
            let illegal_data_addr = msg.to_ascii_lowercase().contains("illegal data address");
            if illegal_data_addr {
                let fallback_addr = reg.address.saturating_add(1);
                let retry = match reg.register_type {
                    RegisterType::Holding => ctx.read_holding_registers(fallback_addr, qty).await,
                    RegisterType::Input => ctx.read_input_registers(fallback_addr, qty).await,
                };
                return retry.with_context(|| {
                    format!(
                        "Read failed for '{}' at {} and fallback {} (slave {}): {}",
                        reg.name, reg.address, fallback_addr, unit_id, msg
                    )
                });
            }

            Err(e).with_context(|| {
                format!(
                    "Read failed for '{}' at {} (slave {})",
                    reg.name, reg.address, unit_id
                )
            })
        }
    }
}

fn is_transport_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("broken pipe")
        || m.contains("connection reset")
        || m.contains("connection refused")
        || m.contains("connection aborted")
        || m.contains("unexpected eof")
        || m.contains("timed out")
        || m.contains("not connected")
}

fn words_to_u32(words: &[u16], endian: Endian) -> Result<u32> {
    if words.len() < 2 {
        return Err(anyhow!("Need 2 words for 32-bit value"));
    }
    let (w0, w1) = (words[0], words[1]);
    let value = match endian {
        Endian::Big => ((w0 as u32) << 16) | w1 as u32,
        Endian::WordSwap => ((w1 as u32) << 16) | w0 as u32,
        Endian::Little => {
            let b0 = (w0 & 0x00FF) as u8;
            let b1 = (w0 >> 8) as u8;
            let b2 = (w1 & 0x00FF) as u8;
            let b3 = (w1 >> 8) as u8;
            u32::from_be_bytes([b3, b2, b1, b0])
        }
    };
    Ok(value)
}

fn words_to_u64(words: &[u16], endian: Endian) -> Result<u64> {
    if words.len() < 4 {
        return Err(anyhow!("Need 4 words for 64-bit value"));
    }
    let mut bytes = Vec::with_capacity(8);
    match endian {
        Endian::Big => {
            for w in &words[0..4] {
                bytes.push((w >> 8) as u8);
                bytes.push((w & 0x00FF) as u8);
            }
        }
        Endian::WordSwap => {
            for w in words[0..4].iter().rev() {
                bytes.push((w >> 8) as u8);
                bytes.push((w & 0x00FF) as u8);
            }
        }
        Endian::Little => {
            for w in &words[0..4] {
                bytes.push((w & 0x00FF) as u8);
                bytes.push((w >> 8) as u8);
            }
            bytes.reverse();
        }
    }
    let arr: [u8; 8] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("Failed to build 8-byte value"))?;
    Ok(u64::from_be_bytes(arr))
}

fn decode_value(reg: &RegisterConfig, words: &[u16]) -> Result<String> {
    let scaled = |x: f64| -> String {
        let y = if let Some(scale) = reg.scale { x * scale } else { x };
        if (y.fract().abs()) < 0.000_001 {
            format!("{:.0}", y)
        } else {
            format!("{:.3}", y)
        }
    };

    let out = match reg.data_type {
        DataType::U16 => scaled(words.first().copied().unwrap_or(0) as f64),
        DataType::I16 => scaled((words.first().copied().unwrap_or(0) as i16) as f64),
        DataType::Bool => {
            if words.first().copied().unwrap_or(0) == 0 {
                "false".to_string()
            } else {
                "true".to_string()
            }
        }
        DataType::U32 => {
            let raw = words_to_u32(words, reg.endian)?;
            scaled(raw as f64)
        }
        DataType::I32 => {
            let raw = words_to_u32(words, reg.endian)? as i32;
            scaled(raw as f64)
        }
        DataType::U64 => {
            let raw = words_to_u64(words, reg.endian)?;
            scaled(raw as f64)
        }
        DataType::F32 => {
            let raw = words_to_u32(words, reg.endian)?;
            let f = f32::from_bits(raw) as f64;
            scaled(f)
        }
        DataType::F64 => {
            let raw = words_to_u64(words, reg.endian)?;
            let f = f64::from_bits(raw);
            scaled(f)
        }
        DataType::String => {
            let mut bytes = Vec::with_capacity(words.len() * 2);
            for w in words {
                bytes.push((w >> 8) as u8);
                bytes.push((w & 0x00FF) as u8);
            }
            let s = String::from_utf8_lossy(&bytes).replace('\0', "").trim().to_string();
            if s.is_empty() {
                " ".to_string()
            } else {
                s
            }
        }
    };

    Ok(out)
}

fn encode_value(reg: &RegisterConfig, text: &str) -> Result<Vec<u16>> {
    match reg.data_type {
        DataType::U16 => {
            let v: u16 = text.trim().parse().context("Expected u16")?;
            Ok(vec![v])
        }
        DataType::I16 => {
            let v: i16 = text.trim().parse().context("Expected i16")?;
            Ok(vec![v as u16])
        }
        DataType::Bool => {
            let v = match text.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" => 1,
                "0" | "false" | "off" => 0,
                _ => return Err(anyhow!("Expected bool: true/false/1/0/on/off")),
            };
            Ok(vec![v])
        }
        DataType::U32 => {
            let v: u32 = text.trim().parse().context("Expected u32")?;
            let hi = (v >> 16) as u16;
            let lo = (v & 0xFFFF) as u16;
            let words = match reg.endian {
                Endian::Big => vec![hi, lo],
                Endian::WordSwap => vec![lo, hi],
                Endian::Little => vec![lo.swap_bytes(), hi.swap_bytes()],
            };
            Ok(words)
        }
        DataType::I32 => {
            let v: i32 = text.trim().parse().context("Expected i32")?;
            encode_value(
                &RegisterConfig {
                    data_type: DataType::U32,
                    ..reg.clone()
                },
                &(v as u32).to_string(),
            )
        }
        DataType::U64 => {
            let v: u64 = text.trim().parse().context("Expected u64")?;
            let be = v.to_be_bytes();
            let mut words = vec![
                u16::from_be_bytes([be[0], be[1]]),
                u16::from_be_bytes([be[2], be[3]]),
                u16::from_be_bytes([be[4], be[5]]),
                u16::from_be_bytes([be[6], be[7]]),
            ];
            match reg.endian {
                Endian::Big => {}
                Endian::WordSwap => words.reverse(),
                Endian::Little => {
                    for w in &mut words {
                        *w = w.swap_bytes();
                    }
                    words.reverse();
                }
            }
            Ok(words)
        }
        DataType::F32 => {
            let v: f32 = text.trim().parse().context("Expected f32")?;
            encode_value(
                &RegisterConfig {
                    data_type: DataType::U32,
                    ..reg.clone()
                },
                &v.to_bits().to_string(),
            )
        }
        DataType::F64 => {
            let v: f64 = text.trim().parse().context("Expected f64")?;
            encode_value(
                &RegisterConfig {
                    data_type: DataType::U64,
                    ..reg.clone()
                },
                &v.to_bits().to_string(),
            )
        }
        DataType::String => {
            let len = register_word_count(reg) as usize;
            let mut bytes = text.as_bytes().to_vec();
            bytes.resize(len * 2, 0);
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                let hi = bytes[i * 2] as u16;
                let lo = bytes[i * 2 + 1] as u16;
                out.push((hi << 8) | lo);
            }
            Ok(out)
        }
    }
}

async fn write_register(
    ctx: &mut ModbusContext,
    regs: &[RegisterConfig],
    index: usize,
    value_text: String,
    default_unit_id: u8,
) -> Result<String> {
    let reg = regs
        .get(index)
        .ok_or_else(|| anyhow!("Invalid register index {index}"))?;
    if !reg.writable {
        return Err(anyhow!("Register '{}' is not writable", reg.name));
    }
    if !matches!(reg.register_type, RegisterType::Holding) {
        return Err(anyhow!("Register '{}' is writable only on holding registers", reg.name));
    }

    let words = encode_value(reg, &value_text)
        .with_context(|| format!("Failed to encode '{}'", reg.name))?;

    let unit_id = effective_unit_id(reg, default_unit_id);
    ctx.set_slave(Slave(unit_id));

    if words.len() == 1 {
        ctx.write_single_register(reg.address, words[0])
            .await
            .with_context(|| {
                format!(
                    "Write failed for '{}' at {} (slave {})",
                    reg.name, reg.address, unit_id
                )
            })?;
    } else {
        ctx.write_multiple_registers(reg.address, &words)
            .await
            .with_context(|| {
                format!(
                    "Write failed for '{}' at {} (slave {})",
                    reg.name, reg.address, unit_id
                )
            })?;
    }

    Ok(reg.name.clone())
}

fn run_worker(cfg: ExplorerConfig, command_rx: Receiver<WorkerCommand>, event_tx: Sender<WorkerEvent>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = event_tx.send(WorkerEvent::Snapshot {
                rows: vec![],
                connection_error: Some(format!("Failed to start worker runtime: {e}")),
            });
            return;
        }
    };

    runtime.block_on(async move {
        let mut rows = vec![RowState::default(); cfg.registers.len()];
        let poll_every = Duration::from_millis(cfg.tcp.poll_interval_ms.max(100));
        let mut ctx: Option<ModbusContext> = None;
        let mut connection_error: Option<String> = None;

        loop {
            let timeout = poll_every;
            match command_rx.recv_timeout(timeout) {
                Ok(WorkerCommand::Write { index, value_text }) => {
                    if ctx.is_none() {
                        match connect_context(&cfg.tcp).await {
                            Ok(new_ctx) => {
                                connection_error = None;
                                ctx = Some(new_ctx);
                            }
                            Err(e) => {
                                connection_error = Some(e.to_string());
                            }
                        }
                    }

                    let result = if let Some(context) = ctx.as_mut() {
                        write_register(
                            context,
                            &cfg.registers,
                            index,
                            value_text,
                            cfg.tcp.unit_id,
                        )
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    } else {
                        Err(connection_error
                            .clone()
                            .unwrap_or_else(|| "No Modbus connection".to_string()))
                    };

                    let reg_name = cfg
                        .registers
                        .get(index)
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| format!("#{index}"));

                    let _ = event_tx.send(WorkerEvent::WriteResult {
                        register_name: reg_name,
                        result,
                    });
                }
                Err(RecvTimeoutError::Timeout) => {
                    if ctx.is_none() {
                        match connect_context(&cfg.tcp).await {
                            Ok(new_ctx) => {
                                connection_error = None;
                                ctx = Some(new_ctx);
                            }
                            Err(e) => {
                                connection_error = Some(e.to_string());
                            }
                        }
                    }

                    if let Some(context) = ctx.as_mut() {
                        let mut had_read_error = false;
                        for (i, reg) in cfg.registers.iter().enumerate() {
                            match read_single_register(context, reg, cfg.tcp.unit_id).await {
                                Ok(words) => {
                                    rows[i].raw = words.clone();
                                    rows[i].error = None;
                                    rows[i].last_updated = Some(Instant::now());
                                    match decode_value(reg, &words) {
                                        Ok(v) => rows[i].value = v,
                                        Err(e) => rows[i].error = Some(e.to_string()),
                                    }
                                }
                                Err(e) => {
                                    let msg = e.to_string();
                                    rows[i].error = Some(msg.clone());
                                    if is_transport_error(&msg) {
                                        had_read_error = true;
                                    }
                                }
                            }
                        }

                        if had_read_error {
                            connection_error = Some("Transport read error; reconnecting".to_string());
                            ctx = None;
                        } else {
                            connection_error = None;
                        }
                    }

                    let _ = event_tx.send(WorkerEvent::Snapshot {
                        rows: rows.clone(),
                        connection_error: connection_error.clone(),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

struct ExplorerApp {
    cfg: ExplorerConfig,
    rows: Vec<RowState>,
    write_inputs: Vec<String>,
    command_tx: Sender<WorkerCommand>,
    event_rx: Receiver<WorkerEvent>,
    status_line: String,
    status_error: bool,
}

impl ExplorerApp {
    fn new(
        cfg: ExplorerConfig,
        command_tx: Sender<WorkerCommand>,
        event_rx: Receiver<WorkerEvent>,
    ) -> Self {
        let row_count = cfg.registers.len();
        Self {
            cfg,
            rows: vec![RowState::default(); row_count],
            write_inputs: vec![String::new(); row_count],
            command_tx,
            event_rx,
            status_line: "Connecting...".to_string(),
            status_error: false,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(evt) = self.event_rx.try_recv() {
            match evt {
                WorkerEvent::Snapshot {
                    rows,
                    connection_error,
                } => {
                    if !rows.is_empty() {
                        self.rows = rows;
                    }
                    match connection_error {
                        Some(e) => {
                            self.status_line = format!("Connection/Read issue: {e}");
                            self.status_error = true;
                        }
                        None => {
                            self.status_line = "Connected and polling".to_string();
                            self.status_error = false;
                        }
                    }
                }
                WorkerEvent::WriteResult {
                    register_name,
                    result,
                } => match result {
                    Ok(()) => {
                        self.status_line = format!("Write OK: {register_name}");
                        self.status_error = false;
                    }
                    Err(e) => {
                        self.status_line = format!("Write FAILED: {register_name}: {e}");
                        self.status_error = true;
                    }
                },
            }
        }
    }
}

impl App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("Modbus Explorer");
                ui.separator();
                ui.label(format!(
                    "Target: {}:{} | Slave: {} | Poll: {} ms",
                    self.cfg.tcp.host,
                    self.cfg.tcp.port,
                    self.cfg.tcp.unit_id,
                    self.cfg.tcp.poll_interval_ms
                ));
            });
            let color = if self.status_error {
                egui::Color32::RED
            } else {
                egui::Color32::LIGHT_GREEN
            };
            ui.colored_label(color, &self.status_line);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(170.0))
                .column(Column::initial(65.0))
                .column(Column::initial(70.0))
                .column(Column::initial(90.0))
                .column(Column::initial(55.0))
                .column(Column::initial(70.0))
                .column(Column::initial(120.0))
                .column(Column::remainder().at_least(120.0))
                .column(Column::initial(160.0))
                .header(24.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("Name");
                    });
                    header.col(|ui| {
                        ui.strong("Addr");
                    });
                    header.col(|ui| {
                        ui.strong("Type");
                    });
                    header.col(|ui| {
                        ui.strong("RegType");
                    });
                    header.col(|ui| {
                        ui.strong("Slave");
                    });
                    header.col(|ui| {
                        ui.strong("Unit");
                    });
                    header.col(|ui| {
                        ui.strong("Value");
                    });
                    header.col(|ui| {
                        ui.strong("Raw/Err");
                    });
                    header.col(|ui| {
                        ui.strong("Write");
                    });
                })
                .body(|mut body| {
                    for (i, reg) in self.cfg.registers.iter().enumerate() {
                        let row = self.rows.get(i).cloned().unwrap_or_default();
                        body.row(26.0, |mut table_row| {
                            table_row.col(|ui| {
                                ui.label(&reg.name);
                            });
                            table_row.col(|ui| {
                                ui.label(reg.address.to_string());
                            });
                            table_row.col(|ui| {
                                ui.label(format!("{:?}", reg.data_type));
                            });
                            table_row.col(|ui| {
                                ui.label(format!("{:?}", reg.register_type));
                            });
                            table_row.col(|ui| {
                                ui.label(effective_unit_id(reg, self.cfg.tcp.unit_id).to_string());
                            });
                            table_row.col(|ui| {
                                ui.label(reg.unit.clone().unwrap_or_else(|| "-".to_string()));
                            });
                            table_row.col(|ui| {
                                ui.label(row.value);
                            });
                            table_row.col(|ui| {
                                if let Some(err) = row.error {
                                    ui.colored_label(egui::Color32::RED, err);
                                } else {
                                    ui.monospace(format!("{:?}", row.raw));
                                }
                            });
                            table_row.col(|ui| {
                                if reg.writable {
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut self.write_inputs[i])
                                                .desired_width(80.0),
                                        );
                                        if ui.button("Write").clicked() {
                                            let value_text = self.write_inputs[i].trim().to_string();
                                            if value_text.is_empty() {
                                                self.status_line =
                                                    format!("Write skipped for {}: empty value", reg.name);
                                                self.status_error = true;
                                            } else {
                                                let _ = self.command_tx.send(WorkerCommand::Write {
                                                    index: i,
                                                    value_text,
                                                });
                                            }
                                        }
                                    });
                                } else {
                                    ui.label("-");
                                }
                            });
                        });
                    }
                });
        });

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn main() -> Result<()> {
    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "modbus_explorer.yaml".to_string());
    let cfg = parse_config(&cfg_path)?;

    let (command_tx, command_rx) = mpsc::channel::<WorkerCommand>();
    let (event_tx, event_rx) = mpsc::channel::<WorkerEvent>();
    let worker_cfg = cfg.clone();

    thread::spawn(move || run_worker(worker_cfg, command_rx, event_tx));

    let app = ExplorerApp::new(cfg, command_tx, event_rx);
    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default().with_maximized(true),
        ..NativeOptions::default()
    };
    let app_title = "Modbus Explorer".to_string();

    eframe::run_native(
        &app_title,
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow!("Failed to start egui app: {e}"))
}
