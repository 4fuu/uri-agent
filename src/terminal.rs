use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::thread::{self, JoinHandle};
use tokio::sync::Notify;

type SharedWriter = Arc<Mutex<BufWriter<Box<dyn Write + Send>>>>;

pub struct EmbeddedTerminal {
    parser: Arc<RwLock<vt100::Parser>>,
    output_notify: Arc<Notify>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<SharedWriter>,
    child: Option<Box<dyn Child + Send + Sync>>,
    reader: Option<JoinHandle<()>>,
    rows: u16,
    cols: u16,
}

impl EmbeddedTerminal {
    pub fn start(mut command: CommandBuilder, cwd: &Path, rows: u16, cols: u16) -> Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        command.cwd(cwd);
        let pair = NativePtySystem::default()
            .openpty(pty_size(rows, cols))
            .context("cannot create embedded terminal")?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cannot read embedded terminal")?;
        let writer = pair
            .master
            .take_writer()
            .context("cannot write embedded terminal")?;
        let child = pair
            .slave
            .spawn_command(command)
            .context("cannot start embedded command")?;
        drop(pair.slave);

        let writer = Arc::new(Mutex::new(BufWriter::new(writer)));
        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 2_000)));
        let output_notify = Arc::new(Notify::new());
        let reader_parser = parser.clone();
        let reader_writer = writer.clone();
        let reader_notify = output_notify.clone();
        let reader = thread::spawn(move || {
            let mut buffer = [0_u8; 8_192];
            let mut queries = CursorPositionQuery::default();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let replies = process_output(
                            &mut reader_parser
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                            &mut queries,
                            &buffer[..read],
                        );
                        if !replies.is_empty() {
                            let mut writer = reader_writer
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let _ = writer.write_all(&replies).and_then(|()| writer.flush());
                        }
                        reader_notify.notify_one();
                    }
                }
            }
            reader_notify.notify_one();
        });

        Ok(Self {
            parser,
            output_notify,
            master: Some(pair.master),
            writer: Some(writer),
            child: Some(child),
            reader: Some(reader),
            rows,
            cols,
        })
    }

    pub fn screen(&self) -> RwLockReadGuard<'_, vt100::Parser> {
        self.parser
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn output_notifier(&self) -> Arc<Notify> {
        self.output_notify.clone()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if self.rows == rows && self.cols == cols {
            return Ok(());
        }
        if let Some(master) = &self.master {
            master
                .resize(pty_size(rows, cols))
                .context("cannot resize embedded terminal")?;
        }
        self.parser
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .screen_mut()
            .set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    pub fn send_key(&mut self, key: KeyEvent) -> Result<()> {
        let application_cursor = self.screen().screen().application_cursor();
        if let Some(bytes) = key_bytes(key, application_cursor) {
            self.write(&bytes)?;
        }
        Ok(())
    }

    pub fn send_paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self.screen().screen().bracketed_paste();
        if bracketed {
            self.write(b"\x1b[200~")?;
        }
        self.write(text.as_bytes())?;
        if bracketed {
            self.write(b"\x1b[201~")?;
        }
        Ok(())
    }

    pub fn send_mouse(&mut self, mouse: MouseEvent, column: u16, row: u16) -> Result<()> {
        let (mode, encoding) = {
            let parser = self.screen();
            (
                parser.screen().mouse_protocol_mode(),
                parser.screen().mouse_protocol_encoding(),
            )
        };
        if mode == vt100::MouseProtocolMode::None || encoding != vt100::MouseProtocolEncoding::Sgr {
            return Ok(());
        }
        let (button, suffix) = match mouse.kind {
            MouseEventKind::Down(button) => (mouse_button(button), 'M'),
            MouseEventKind::Up(_) => (3, 'm'),
            MouseEventKind::Drag(button) => (32 + mouse_button(button), 'M'),
            MouseEventKind::Moved => return Ok(()),
            MouseEventKind::ScrollUp => (64, 'M'),
            MouseEventKind::ScrollDown => (65, 'M'),
            MouseEventKind::ScrollLeft => (66, 'M'),
            MouseEventKind::ScrollRight => (67, 'M'),
        };
        let modifiers = u16::from(mouse.modifiers.contains(KeyModifiers::SHIFT)) * 4
            + u16::from(mouse.modifiers.contains(KeyModifiers::ALT)) * 8
            + u16::from(mouse.modifiers.contains(KeyModifiers::CONTROL)) * 16;
        self.write(
            format!(
                "\x1b[<{};{};{}{}",
                button + modifiers,
                column.saturating_add(1),
                row.saturating_add(1),
                suffix
            )
            .as_bytes(),
        )
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .map(|child| child.try_wait())
            .transpose()
            .context("cannot query embedded command")
            .map(Option::flatten)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            let mut writer = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writer
                .write_all(bytes)
                .and_then(|()| writer.flush())
                .context("cannot send input to embedded terminal")?;
        }
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(child) = self.child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child.take();
        self.writer.take();
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Default)]
struct CursorPositionQuery {
    matched: usize,
}

impl CursorPositionQuery {
    fn advance(&mut self, byte: u8) -> bool {
        const QUERY: &[u8] = b"\x1b[6n";
        if byte == QUERY[self.matched] {
            self.matched += 1;
            if self.matched == QUERY.len() {
                self.matched = 0;
                return true;
            }
        } else {
            self.matched = usize::from(byte == QUERY[0]);
        }
        false
    }
}

fn process_output(
    parser: &mut vt100::Parser,
    queries: &mut CursorPositionQuery,
    bytes: &[u8],
) -> Vec<u8> {
    let mut replies = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if queries.advance(byte) {
            parser.process(&bytes[start..=index]);
            let (row, col) = parser.screen().cursor_position();
            replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            start = index + 1;
        }
    }
    parser.process(&bytes[start..]);
    replies
}

impl Drop for EmbeddedTerminal {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn mouse_button(button: crossterm::event::MouseButton) -> u16 {
    match button {
        crossterm::event::MouseButton::Left => 0,
        crossterm::event::MouseButton::Middle => 1,
        crossterm::event::MouseButton::Right => 2,
    }
}

fn key_bytes(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let cursor = |standard: &[u8], application: &[u8]| {
        if application_cursor {
            application.to_vec()
        } else {
            standard.to_vec()
        }
    };
    let mut bytes = match key.code {
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Left => cursor(b"\x1b[D", b"\x1bOD"),
        KeyCode::Right => cursor(b"\x1b[C", b"\x1bOC"),
        KeyCode::Up => cursor(b"\x1b[A", b"\x1bOA"),
        KeyCode::Down => cursor(b"\x1b[B", b"\x1bOB"),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(number) => function_key(number)?.to_vec(),
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let control = match character.to_ascii_uppercase() {
                '@' | ' ' => 0,
                value @ 'A'..='_' => value as u8 & 0x1f,
                '?' => 0x7f,
                _ => return None,
            };
            vec![control]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Esc => vec![0x1b],
        _ => return None,
    };
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn function_key(number: u8) -> Option<&'static [u8]> {
    Some(match number {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use futures_util::FutureExt;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[test]
    fn keys_are_encoded_for_terminal_programs() {
        assert_eq!(
            key_bytes(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), false),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn cursor_position_queries_are_answered_across_reads() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        let mut queries = CursorPositionQuery::default();

        assert!(process_output(&mut parser, &mut queries, b"hello\x1b[").is_empty());
        assert_eq!(
            process_output(&mut parser, &mut queries, b"6n"),
            b"\x1b[1;6R"
        );
        assert_eq!(parser.screen().contents(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn embedded_terminal_reads_command_output() {
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "printf terminal-ready"]);
        let mut terminal =
            EmbeddedTerminal::start(command, Path::new(env!("CARGO_MANIFEST_DIR")), 24, 80)
                .unwrap();
        let output_notify = terminal.output_notifier();
        let deadline = Instant::now() + Duration::from_secs(2);

        while Instant::now() < deadline {
            if terminal
                .screen()
                .screen()
                .contents()
                .contains("terminal-ready")
            {
                assert!(output_notify.notified().now_or_never().is_some());
                return;
            }
            let _ = terminal.try_wait().unwrap();
            thread::sleep(Duration::from_millis(10));
        }

        panic!("PTY output was not parsed into the terminal screen");
    }
}
