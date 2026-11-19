//! `/copy`'s clipboard write: OSC 52 for terminals that support it (also
//! over SSH), plus PowerShell's `Set-Clipboard` on Windows so classic
//! conhost works too.
//!
//! Not `clip.exe`: measured on Windows 11, a UTF-16LE BOM is kept on the
//! clipboard as a U+FEFF, and without one its encoding guess garbles short
//! text such as `中文` or a lone `—`; UTF-8 depends on the console code page.

use std::io::Write;

use base64::Engine;

/// The text as base64 of its UTF-8 bytes: pure ASCII, so neither a terminal
/// nor a console code page can alter it on the way.
pub fn base64_utf8(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// Puts `text` on the clipboard. OSC 52 is always sent (a terminal that
/// does not support it ignores it); on Windows `Set-Clipboard` decides success.
pub fn copy(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    let osc = format!("\x1b]52;c;{}\x07", base64_utf8(text));
    let _ = out.write_all(osc.as_bytes()).and_then(|()| out.flush());
    #[cfg(windows)]
    {
        set_clipboard(text)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

#[cfg(windows)]
pub fn set_clipboard(text: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    // Its own hidden console, so PowerShell cannot reset SE's raw mode.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$s=[Console]::In.ReadToEnd(); Set-Clipboard -Value \
             ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s.Trim())))",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start powershell: {e}"))?;
    let written = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(base64_utf8(text).as_bytes()))
        .unwrap_or(Ok(()));
    let status = child.wait().map_err(|e| format!("powershell: {e}"))?;
    written.map_err(|e| format!("powershell: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Set-Clipboard exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_ascii_and_decodes_back_to_the_exact_text() {
        let text = "héllo 😀 中文\n";
        let payload = base64_utf8(text);
        assert!(payload.is_ascii());
        let back = base64::engine::general_purpose::STANDARD
            .decode(&payload)
            .unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), text);
    }

    /// Overwrites the real clipboard, so it only runs when asked:
    /// `cargo test -p se clip_round_trip -- --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn clip_round_trip_through_get_clipboard() {
        let long = "x".repeat(5000);
        let cases = [
            "copy test héllo 😀 中文",
            "中文",
            "—",
            "中a",
            "line one\nline two",
            "trailing newline\n",
            long.as_str(),
        ];
        for text in cases {
            let started = std::time::Instant::now();
            set_clipboard(text).expect("Set-Clipboard");
            eprintln!("set_clipboard took {:?}", started.elapsed());
            // Read back as base64 too, so no console encoding is involved.
            let out = std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-Command",
                    "[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((Get-Clipboard -Raw)))",
                ])
                .output()
                .expect("powershell");
            let b64 = String::from_utf8(out.stdout).expect("ascii");
            let got = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .expect("base64");
            assert_eq!(String::from_utf8(got).expect("utf-8"), text, "{text:?}");
        }
    }
}
