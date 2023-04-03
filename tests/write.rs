use std::{error, io};
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use sbp_settings::Client;

pub fn connect_tcp(ip: impl Into<String>) -> Result<TcpStream, Box<dyn error::Error>> {
    let mut addrs = ip.into().to_socket_addrs()?;
    for addr in addrs {
        if let Ok(conn) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            return Ok(conn);
        }
    }
    Err(Box::new(io::Error::new(ErrorKind::ConnectionRefused, "connection refused")))
}

#[test]
fn write_to_tcp() -> Result<(), Box<dyn error::Error>> {
    let ip = "10.1.54.1:55555";

    // connect to socket
    eprintln!("connecting {ip}");
    let reader = connect_tcp(ip)?;
    reader.set_read_timeout(Some(Duration::from_secs(5)))?;
    let writer = reader.try_clone()?;
    eprintln!("connected {ip}\n");

    // libsettings client
    let mut client = Client::new(reader, writer);

    eprintln!("curr val: {:?}", client.read_setting("simulator", "base_ecef_z")?.unwrap().value);

    let dummy_value = "1.1231234";
    eprintln!("attempting to write: {dummy_value}");
    client.write_setting("simulator", "base_ecef_z", dummy_value)?;

    eprintln!("new val: {:?}", client.read_setting("simulator", "base_ecef_z")?.unwrap().value);
    Ok(())
}
