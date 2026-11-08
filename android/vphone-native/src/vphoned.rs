//! vphoned, the guest daemon, over the Unix socket of QEMU's virtio-vsock-uds device.
//! `CONNECT <port>\n` / `OK <n>\n`, then HTTP (tools/vphoned.sh).

use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PORT: u32 = 1339;

#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Client {
    pub fn new(socket: &Path) -> Client {
        Client {
            socket: socket.to_path_buf(),
        }
    }

    /// One HTTP exchange; the response's status and body. The write side stays open until the
    /// answer arrives: vphoned drops a half-closed connection before it answers an RPC.
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> io::Result<(u32, Vec<u8>)> {
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);

        writer.write_all(format!("CONNECT {PORT}\n").as_bytes())?;
        let mut ok = String::new();
        reader.read_line(&mut ok)?;
        if !ok.starts_with("OK ") {
            return Err(bad(format!(
                "the vsock device answered {:?}",
                ok.trim_end()
            )));
        }

        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        if let Some(body) = body {
            request += &format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            );
        }
        request += "\r\n";
        request += body.unwrap_or("");
        writer.write_all(request.as_bytes())?;

        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                bad(format!(
                    "not an HTTP response: {:?}",
                    status_line.trim_end()
                ))
            })?;
        let mut length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
        }
        let mut body = Vec::new();
        match length {
            Some(n) => {
                body.resize(n, 0);
                reader.read_exact(&mut body)?;
            }
            None => {
                reader.read_to_end(&mut body)?;
            }
        }
        drop(writer);
        Ok((status, body))
    }

    pub fn healthy(&self, timeout: Duration) -> bool {
        self.exchange("GET", "/v1/health", None, timeout)
            .ok()
            .and_then(|(_, body)| serde_json::from_slice::<Value>(&body).ok())
            .is_some_and(|v| v["status"] == "ok")
    }

    /// A vphoned RPC; its result, or vphoned's message.
    pub fn rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let body = json!({"method": method, "params": params, "id": 1}).to_string();
        let (status, reply) = self
            .exchange("POST", "/v1/rpc", Some(&body), timeout)
            .map_err(|e| format!("{method}: {e}"))?;
        let reply: Value =
            serde_json::from_slice(&reply).map_err(|e| format!("{method}: HTTP {status}: {e}"))?;
        if let Some(err) = reply.get("error").filter(|e| !e.is_null()) {
            return Err(format!(
                "{method}: {}",
                err["message"].as_str().unwrap_or("failed")
            ));
        }
        Ok(reply["result"].clone())
    }

    /// Whether the screen is unlocked after `screen.unlock`.
    pub fn unlock(&self, timeout: Duration) -> Result<bool, String> {
        Ok(self.rpc("screen.unlock", json!({}), timeout)?["locked"] == false)
    }

    pub fn button(&self, name: &str) -> Result<(), String> {
        self.rpc(
            "input.button",
            json!({"name": name}),
            Duration::from_secs(30),
        )
        .map(drop)
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.rpc(
            "system.shutdown",
            json!({"force": true}),
            Duration::from_secs(30),
        )
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A vsock device with vphoned behind it: checks the request, answers `response`.
    fn serve(
        name: &str,
        response: &'static str,
        wants: &'static [&'static str],
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/sock");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut connect = String::new();
            r.read_line(&mut connect).unwrap();
            assert_eq!(connect, "CONNECT 1339\n");
            s.write_all(b"OK 1073741824\n").unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut b = [0u8];
                r.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            let head = String::from_utf8(head).unwrap();
            assert!(head.contains("Host: localhost\r\n"));
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .map_or(0, |v| v.parse().unwrap());
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let all = head + &String::from_utf8(body).unwrap();
            for want in wants {
                assert!(all.contains(want), "{want} not in {all}");
            }
            s.write_all(response.as_bytes()).unwrap();
        });
        (path, handle)
    }

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn health() {
        let (p, h) = serve(
            "h",
            "HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}",
            &["GET /v1/health "],
        );
        assert!(Client::new(&p).healthy(T));
        h.join().unwrap();
    }

    #[test]
    fn unlock_reads_locked_false() {
        let body =
            "HTTP/1.1 200 OK\r\nContent-Length: 28\r\n\r\n{\"result\":{\"locked\":false}} \n";
        let (p, h) = serve(
            "u",
            body,
            &["POST /v1/rpc ", "\"method\":\"screen.unlock\""],
        );
        assert_eq!(Client::new(&p).unlock(T), Ok(true));
        h.join().unwrap();
    }

    #[test]
    fn rpc_error_message_comes_back() {
        let body = "HTTP/1.1 200 OK\r\n\r\n{\"error\":{\"message\":\"no such button\"}}";
        let (p, h) = serve("e", body, &["\"name\":\"lock\""]);
        let err = Client::new(&p).button("lock").unwrap_err();
        assert!(err.contains("no such button"), "{err}");
        h.join().unwrap();
    }
}
