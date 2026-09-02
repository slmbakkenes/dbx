//! Clipboard via the terminal itself (OSC 52), plus the native tool when present.
//!
//! OSC 52 is what makes yanking work over SSH, where `pbcopy` is the wrong machine.

use std::io::Write;

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn copy(text: &str) {
    // OSC 52 first: it reaches the clipboard of the machine you are sitting at.
    let payload = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let mut out = std::io::stdout();
    let _ = out.write_all(payload.as_bytes());
    let _ = out.flush();

    #[cfg(target_os = "macos")]
    native("pbcopy", &[], text);
    #[cfg(target_os = "linux")]
    native("xclip", &["-selection", "clipboard"], text);
}

#[allow(dead_code)]
fn native(cmd: &str, args: &[&str], text: &str) {
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("héllo".as_bytes()), "aMOpbGxv");
    }
}
