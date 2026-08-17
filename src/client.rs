use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::protocol::{Request, Response};
use crate::TyrionError;

pub fn send_request(socket_path: &Path, request: &Request) -> Result<Response, TyrionError> {
    let mut stream = UnixStream::connect(socket_path)?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = Vec::new();
    BufReader::new(stream).read_to_end(&mut response)?;
    Ok(serde_json::from_slice(&response)?)
}
