use std::{collections::HashMap, io::{Write, stdout}, time::Duration};

use running_average::RealTimeRunningAverage;


fn main() -> Result<(), std::io::Error> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:52224")?;
    let mut tws = HashMap::<String, RealTimeRunningAverage<f64>>::new();
    let mut maxs = HashMap::<String, f64>::new();

    println!("Bound to port 52224");
    loop {
        let mut buf = [0u8; 1400];
        let (amt, addr) = socket.recv_from(&mut buf)?;
        let tw = tws.entry(addr.to_string()).or_insert_with(|| RealTimeRunningAverage::new(Duration::from_secs(4)));
        tw.insert(amt as f64);
        let max = maxs.entry(addr.to_string()).or_insert(0.);
        if *max < tw.measurement().to_rate() {
            *max = tw.measurement().to_rate();
        }
        for (addr, max) in maxs.iter() {
            print!("[{addr}]: {}; ", max);
        }
        print!("\r");
        stdout().flush()?;
    }
}