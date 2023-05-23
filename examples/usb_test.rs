use tokio::time::Duration;
use windlass::{mcu_command, mcu_reply, McuConnection};

mcu_command!(DebugNop, "debug_nop");
mcu_command!(GetClock, "get_clock");
mcu_reply!(Clock, "clock", clock: u32);
mcu_command!(GetUptime, "get_uptime");
mcu_reply!(Uptime, "uptime", high: u32, clock: u32);
mcu_reply!(
    AdxlStatus,
    "adxl345_status",
    oid: u8,
    sequence: u16,
    data: Vec<u8>,
);
mcu_reply!(Stats, "stats", count: u32, sum: u32, sumsq: u32);

mcu_command!(
    ConfigEndstop,
    "config_endstop",
    oid: u8,
    pin: u8,
    pull_up: u8
);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let target = std::env::args().nth(1).expect("Missing path");
    let builder = tokio_serial::new(target, 96_000);
    let port = tokio_serial::SerialStream::open(&builder).expect("USB open");

    let mut mcu = McuConnection::connect(port).await.expect("MCU connect");

    let mut resp = mcu.register_response(Stats).expect("Register");

    let clock = mcu.send_receive(GetClock::encode(), Clock);
    let uptime = mcu.send_receive(GetUptime::encode(), Uptime).await;
    println!("Uptime: {uptime:?}");
    let clock = clock.await;
    println!("Clock: {clock:?}");

    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            _ = &mut timeout => break,
            s = resp.recv() => {
                println!("Stats: {s:?}");
            }
            err = mcu.closed() => {
                println!("MCU connection lost: {err:?}");
                break;
            }
        }
    }
    mcu.close().await;
}
