//! QEMU's monitor protocol, for powering the VM off when the guest cannot do it itself.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub fn quit(socket: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?; // the greeting
    writer.write_all(b"{\"execute\":\"qmp_capabilities\"}\n")?;
    line.clear();
    reader.read_line(&mut line)?;
    writer.write_all(b"{\"execute\":\"quit\"}\n")?;
    line.clear();
    // Its "return", or the end of the connection when QEMU is quicker.
    let _ = reader.read_line(&mut line);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    #[test]
    fn negotiates_then_quits() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/sock");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qmp");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            s.write_all(b"{\"QMP\":{}}\n").unwrap();
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            assert!(l.contains("qmp_capabilities"));
            s.write_all(b"{\"return\":{}}\n").unwrap();
            l.clear();
            r.read_line(&mut l).unwrap();
            assert!(l.contains("\"quit\""));
        });
        quit(&path).unwrap();
        server.join().unwrap();
    }
}
