//! Checks the driver-free packet source on real hardware: opens it, counts
//! TCP:443 segments in each direction and prints what it saw.
//!
//! Needs Administrator, like the capture service itself:
//!
//! ```powershell
//! cargo build --example rawsock_probe
//! Start-Process -Verb RunAs target\debug\examples\rawsock_probe.exe
//! ```

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(15);
    match arc_live_capture::probe_packet_source(seconds) {
        Ok(report) => println!("{report}"),
        Err(error) => println!("ОШИБКА: {error:#}"),
    }
    println!("\nОкно закроется через 20 секунд.");
    std::thread::sleep(std::time::Duration::from_secs(20));
}
