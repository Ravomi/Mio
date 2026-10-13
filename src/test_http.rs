//! Small, bounded request reader shared by local provider protocol tests.
use std::io::Read;
pub fn read_json(socket: &mut impl Read) -> (String, serde_json::Value) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 65536, "request headers exceed test limit");
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let header = String::from_utf8(bytes).unwrap();
    let length: usize = header
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse().unwrap())
        })
        .unwrap();
    assert!(length < 1024 * 1024, "request body exceeds test limit");
    let mut body = vec![0; length];
    socket.read_exact(&mut body).unwrap();
    (header, serde_json::from_slice(&body).unwrap())
}
