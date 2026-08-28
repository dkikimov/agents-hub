//! The guest's clipboard requests (OSC 52) and the local clipboard they land in.

use crate::proto::unb64;
use anyhow::{bail, Result};
use std::io::Write as _;
use std::process::Stdio;
use tui_term::vt100;

/// Copies a pane asked for, queued until the frame that produced them is fully
/// processed. Attached to every parser, drained only for the pane you're looking at.
#[derive(Default)]
pub struct Clipboard(Vec<Vec<u8>>);

impl vt100::Callbacks for Clipboard {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        if let Ok(data) = std::str::from_utf8(data) {
            if let Ok(data) = unb64(data) {
                self.0.push(data);
            }
        }
    }
}

impl Clipboard {
    pub fn take(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.0)
    }

    /// Drains either way: a copy that arrived during replay, or for a pane you aren't
    /// looking at, is dropped rather than queued to fire later.
    pub fn take_if(&mut self, allowed: bool) -> Vec<Vec<u8>> {
        let copies = self.take();
        if allowed {
            copies
        } else {
            Vec::new()
        }
    }
}

pub fn copy_local(data: &[u8]) -> Result<()> {
    let mut child = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        bail!("pbcopy stdin unavailable")
    };
    stdin.write_all(data)?;
    drop(stdin);
    if !child.wait()?.success() {
        bail!("pbcopy failed")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_copy_survives_split_pty_chunks() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, Clipboard::default());
        parser.process(b"\x1b]52;c;aGV");
        parser.process(b"sbG8=\x07");
        parser.process(b"\x1b]52;c;?\x07"); // reads never reach the local clipboard

        assert_eq!(parser.callbacks_mut().take(), vec![b"hello".to_vec()]);
        assert!(parser.callbacks_mut().take().is_empty());
    }

    #[test]
    fn suppressed_osc52_copies_are_discarded() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, Clipboard::default());
        parser.process(b"\x1b]52;c;c3RhbGU=\x07");

        assert!(parser.callbacks_mut().take_if(false).is_empty());
        assert!(parser.callbacks_mut().take_if(true).is_empty());
    }
}
