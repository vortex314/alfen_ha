use anyhow::{Context, Result};
use byteorder::{BigEndian, ByteOrder};
use std::time::Duration;
use tokio::time;
use tokio_modbus::prelude::*;

const REG_MAX_CURRENT: u16 = 1210; // FLOAT32 (2 regs)
const REG_SETPOINT_ACCOUNTED: u16 = 1214; // UNSIGNED16 (1 reg)
const REG_VALID_TIME: u16 = 1208; // UNSIGNED32 (2 regs)

fn f32_to_regs(v: f32) -> [u16; 2] {
    let bytes = v.to_be_bytes();
    [
        BigEndian::read_u16(&bytes[0..2]),
        BigEndian::read_u16(&bytes[2..4]),
    ]
}

fn regs_to_f32(regs: &[u16]) -> f32 {
    let mut buf = [0u8; 4];
    BigEndian::write_u16(&mut buf[0..2], regs[0]);
    BigEndian::write_u16(&mut buf[2..4], regs[1]);
    f32::from_be_bytes(buf)
}

fn regs_to_u32(regs: &[u16]) -> u32 {
    ((regs[0] as u32) << 16) | (regs[1] as u32)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "192.168.0.50".to_string());
    let port: u16 = args
        .next()
        .as_deref()
        .unwrap_or("502")
        .parse()
        .context("Invalid port")?;
    let slave: u8 = args
        .next()
        .as_deref()
        .unwrap_or("200")
        .parse()
        .context("Invalid slave")?;

    let addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .context("Invalid host/port")?;

    println!("Connecting to {addr} (slave {slave})");
    let mut ctx = tcp::connect_slave(addr, Slave(slave))
        .await
        .context("Failed to connect")?;

    let regs = f32_to_regs(11.0);
    ctx.write_multiple_registers(REG_MAX_CURRENT, &regs)
        .await
        .context("Failed to write 11.0 to register 1210")?;
    println!("Wrote 11.0A to reg {REG_MAX_CURRENT} as raw {regs:?}");

    let mut ticker = time::interval(Duration::from_millis(100));
    ticker.tick().await;

    loop {
        ticker.tick().await;

        let max_regs = match ctx.read_holding_registers(REG_MAX_CURRENT, 2).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("read 1210 failed: {e}");
                continue;
            }
        };

        let accounted = match ctx.read_holding_registers(REG_SETPOINT_ACCOUNTED, 1).await {
            Ok(v) => v.first().copied().unwrap_or(0),
            Err(e) => {
                eprintln!("read 1214 failed: {e}");
                continue;
            }
        };

        let valid_regs = match ctx.read_holding_registers(REG_VALID_TIME, 2).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("read 1208 failed: {e}");
                continue;
            }
        };

        let max_current = regs_to_f32(&max_regs);
        let valid_time = regs_to_u32(&valid_regs);

        println!(
            "1210={max_current:.2}A (raw={max_regs:?}) 1214={accounted} 1208={valid_time}s"
        );
    }
}
