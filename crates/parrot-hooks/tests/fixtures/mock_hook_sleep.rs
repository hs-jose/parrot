// Mock external hook: sleeps to trigger a timeout fail-open.
use std::io::Read;
use std::time::Duration;

fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    std::thread::sleep(Duration::from_secs(10));
}
