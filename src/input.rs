//! Keyboard input read straight from the terminal. Hotkeys are physical keys: with the kitty
//! keyboard protocol (kitty, foot, WezTerm, Ghostty, Alacritty, Konsole…) the terminal tells the
//! key of the standard US layout behind every press, so they work the same in any layout and
//! with Caps Lock on; crossterm drops that part, hence an own parser. Without the protocol the
//! Russian layout is mapped by a table. Text fields get the characters as typed.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

/// Kitty keyboard protocol flags: disambiguate escape codes (1), report alternate keys — shifted
/// and base layout (4), all keys as escape codes (8), associated text (16).
const KITTY_FLAGS: u8 = 1 | 4 | 8 | 16;
/// A lone ESC not followed by more within this time is the Esc key.
const ESC_WAIT: Duration = Duration::from_millis(30);

/// A key press: as typed (for text fields) and as a hotkey (the physical key in the US layout,
/// Shift applied, Caps Lock ignored).
#[derive(Debug, Clone, PartialEq)]
pub struct Press {
    pub typed: KeyEvent,
    pub hotkey: KeyEvent,
}

impl From<KeyEvent> for Press {
    /// A press without the physical key known (tests, terminals without the protocol): the
    /// hotkey is the character, Russian layout mapped to its keys.
    fn from(typed: KeyEvent) -> Press {
        let hotkey = match typed.code {
            KeyCode::Char(c) => KeyEvent::new(KeyCode::Char(ru_to_en(c)), typed.modifiers),
            _ => typed,
        };
        Press { typed, hotkey }
    }
}

impl From<KeyCode> for Press {
    fn from(code: KeyCode) -> Press {
        KeyEvent::from(code).into()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Key(Press),
    Paste(String),
}

/// Maps Russian (ЙЦУКЕН) layout characters to their QWERTY keys so hotkeys work on either layout.
pub fn ru_to_en(c: char) -> char {
    const RU: &str = "йцукенгшщзхъфывапролджэячсмитьбю.ЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮ,ё";
    const EN: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,./QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>?`";
    RU.chars().position(|r| r == c).and_then(|i| EN.chars().nth(i)).unwrap_or(c)
}

/// The character Shift gives on a key of the US layout.
fn us_shift(c: char) -> char {
    const PLAIN: &str = "`1234567890-=[]\\;',./";
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    match PLAIN.chars().position(|p| p == c) {
        Some(i) => SHIFTED.chars().nth(i).unwrap_or(c),
        None => c.to_ascii_uppercase(),
    }
}

/// Modifiers of a CSI parameter (1 + bits: shift 1, alt 2, ctrl 4; the rest is ignored).
fn modifiers(param: u32) -> KeyModifiers {
    let bits = param.saturating_sub(1);
    let mut m = KeyModifiers::NONE;
    m.set(KeyModifiers::SHIFT, bits & 1 != 0);
    m.set(KeyModifiers::ALT, bits & 2 != 0);
    m.set(KeyModifiers::CONTROL, bits & 4 != 0);
    m
}

fn key(code: KeyCode, mods: KeyModifiers) -> Input {
    let k = KeyEvent::new(code, mods);
    Input::Key(Press { typed: k, hotkey: k })
}

/// What the start of a buffer holds.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// An event (or nothing worth reporting) and the bytes it took.
    Done(Option<Input>, usize),
    /// The terminal supports the kitty keyboard protocol (its reply to the query).
    KittySupported(usize),
    /// Bracketed paste starts.
    PasteStart(usize),
    /// More bytes are needed.
    Incomplete,
}

/// A kitty `CSI … u` key: `code[:shifted[:base]] ; mods[:event] ; text`.
fn kitty_key(params: &str) -> Option<Input> {
    let mut fields = params.split(';');
    let keys: Vec<Option<u32>> = fields.next()?.split(':').map(|k| k.parse().ok()).collect();
    let mods_field = fields.next().unwrap_or("1");
    let mut mod_parts = mods_field.split(':');
    let mods = modifiers(mod_parts.next().and_then(|m| m.parse().ok()).unwrap_or(1));
    // Releases and repeats (only with the event-types flag, not asked for) are not presses.
    if mod_parts.next().is_some_and(|e| e == "3") {
        return None;
    }
    let text: String = fields.next().unwrap_or("").split(':').filter_map(|c| c.parse().ok()).filter_map(char::from_u32).collect();
    let code = keys.first().copied().flatten()?;
    let special = match code {
        13 | 57414 => Some(KeyCode::Enter),
        9 => Some(if mods.contains(KeyModifiers::SHIFT) { KeyCode::BackTab } else { KeyCode::Tab }),
        27 => Some(KeyCode::Esc),
        8 | 127 => Some(KeyCode::Backspace),
        57399..=57408 => char::from_digit(code - 57399, 10).map(KeyCode::Char),
        // Other functional keys: modifiers and locks pressed alone, media keys…
        57344..=63743 => return None,
        _ => None,
    };
    if let Some(code) = special {
        let mods = if code == KeyCode::BackTab { mods - KeyModifiers::SHIFT } else { mods };
        return Some(key(code, mods));
    }
    let c = char::from_u32(code)?;
    let shifted = keys.get(1).copied().flatten().and_then(char::from_u32);
    let base = keys.get(2).copied().flatten().and_then(char::from_u32);
    let plain = mods - KeyModifiers::SHIFT;
    // As typed: the text the terminal reports, else the shifted or the plain character.
    let typed_char = text.chars().next().filter(|t| !t.is_control()).or(shifted).unwrap_or(c);
    let typed = KeyEvent::new(KeyCode::Char(typed_char), if text.is_empty() && shifted.is_none() { mods } else { plain });
    // The physical key: the base layout one, or the key itself in a Latin layout. Some terminals
    // (Konsole) apply Caps Lock or Shift to the key code of a non-Latin layout: lower it.
    let physical = base.unwrap_or(c).to_lowercase().next().unwrap_or(c);
    let physical = if base.is_none() { ru_to_en(physical) } else { physical };
    let hotkey = if mods.contains(KeyModifiers::SHIFT) {
        KeyEvent::new(KeyCode::Char(us_shift(physical)), plain)
    } else {
        KeyEvent::new(KeyCode::Char(physical), mods)
    };
    Some(Input::Key(Press { typed, hotkey }))
}

/// A `CSI params final` sequence other than kitty keys.
fn csi(params: &str, last: u8) -> Parsed {
    let nums: Vec<u32> = params.split(';').map(|p| p.split(':').next().unwrap_or("").parse().unwrap_or(1)).collect();
    let mods = modifiers(nums.get(1).copied().unwrap_or(1));
    let code = match last {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => return Parsed::Done(Some(key(KeyCode::BackTab, KeyModifiers::NONE)), 0),
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'S' => KeyCode::F(4),
        b'~' => match nums.first().copied().unwrap_or(0) {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            n @ 11..=15 => KeyCode::F((n - 10) as u8),
            n @ 17..=21 => KeyCode::F((n - 11) as u8),
            n @ 23..=24 => KeyCode::F((n - 12) as u8),
            200 => return Parsed::PasteStart(0),
            _ => return Parsed::Done(None, 0),
        },
        // Cursor position reports, device attributes and the like.
        _ => return Parsed::Done(None, 0),
    };
    Parsed::Done(Some(key(code, mods)), 0)
}

/// Parses the event at the start of `buf`.
fn parse(buf: &[u8]) -> Parsed {
    let Some(&first) = buf.first() else { return Parsed::Incomplete };
    if first != 0x1b {
        return match first {
            b'\r' | b'\n' => Parsed::Done(Some(key(KeyCode::Enter, KeyModifiers::NONE)), 1),
            b'\t' => Parsed::Done(Some(key(KeyCode::Tab, KeyModifiers::NONE)), 1),
            0x7f | 0x08 => Parsed::Done(Some(key(KeyCode::Backspace, KeyModifiers::NONE)), 1),
            0 => Parsed::Done(Some(key(KeyCode::Char(' '), KeyModifiers::CONTROL)), 1),
            c @ 1..=26 => Parsed::Done(Some(Press::from(KeyEvent::new(KeyCode::Char((b'a' + c - 1) as char), KeyModifiers::CONTROL)).into()), 1),
            0x1c..=0x1f => Parsed::Done(None, 1),
            _ => {
                let len = match first {
                    0..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => return Parsed::Done(None, 1),
                };
                if buf.len() < len {
                    return Parsed::Incomplete;
                }
                match std::str::from_utf8(&buf[..len]).ok().and_then(|s| s.chars().next()) {
                    Some(c) => Parsed::Done(Some(Press::from(KeyEvent::from(KeyCode::Char(c))).into()), len),
                    None => Parsed::Done(None, 1),
                }
            }
        };
    }
    let Some(&second) = buf.get(1) else { return Parsed::Incomplete };
    match second {
        b'[' => {
            // Parameters, intermediates, then the final byte (0x40..=0x7e).
            let Some(end) = buf[2..].iter().position(|b| (0x40..=0x7e).contains(b)) else {
                return Parsed::Incomplete;
            };
            let len = end + 3;
            let params = String::from_utf8_lossy(&buf[2..len - 1]).into_owned();
            let last = buf[len - 1];
            if let Some(p) = params.strip_prefix('?') {
                // `CSI ? flags u`: the kitty protocol query was answered; `CSI ? … c`: device attributes.
                return if last == b'u' && p.chars().all(|c| c.is_ascii_digit()) { Parsed::KittySupported(len) } else { Parsed::Done(None, len) };
            }
            if last == b'u' {
                return Parsed::Done(kitty_key(&params), len);
            }
            match csi(&params, last) {
                Parsed::Done(ev, _) => Parsed::Done(ev, len),
                Parsed::PasteStart(_) => Parsed::PasteStart(len),
                other => other,
            }
        }
        b'O' => {
            let Some(&c) = buf.get(2) else { return Parsed::Incomplete };
            let code = match c {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                b'M' => KeyCode::Enter,
                b'P'..=b'S' => KeyCode::F(c - b'P' + 1),
                _ => return Parsed::Done(None, 3),
            };
            Parsed::Done(Some(key(code, KeyModifiers::NONE)), 3)
        }
        // OSC, DCS, APC (kitty graphics replies), SOS, PM: skipped up to ST (or BEL for OSC).
        b']' | b'P' | b'_' | b'X' | b'^' => {
            for i in 2..buf.len() {
                if buf[i] == 0x07 && second == b']' {
                    return Parsed::Done(None, i + 1);
                }
                if buf[i] == 0x1b && buf.get(i + 1) == Some(&b'\\') {
                    return Parsed::Done(None, i + 2);
                }
            }
            Parsed::Incomplete
        }
        0x1b => Parsed::Done(Some(key(KeyCode::Esc, KeyModifiers::NONE)), 1),
        _ => {
            // Alt + a key.
            match parse(&buf[1..]) {
                Parsed::Done(Some(Input::Key(mut p)), n) => {
                    p.typed.modifiers |= KeyModifiers::ALT;
                    p.hotkey.modifiers |= KeyModifiers::ALT;
                    Parsed::Done(Some(Input::Key(p)), n + 1)
                }
                Parsed::Incomplete => Parsed::Incomplete,
                _ => Parsed::Done(None, 2),
            }
        }
    }
}

impl From<Press> for Input {
    fn from(p: Press) -> Input {
        Input::Key(p)
    }
}

/// Reads and parses the terminal's input. Created after the terminal is in raw mode; `finish`
/// must be called before leaving it.
pub struct Reader {
    buf: Vec<u8>,
    paste: Option<Vec<u8>>,
    kitty: bool,
}

impl Reader {
    /// Asks the terminal whether it speaks the kitty keyboard protocol; the protocol is turned
    /// on when (if) it answers.
    pub fn new() -> Reader {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?u").and_then(|()| out.flush());
        Reader { buf: Vec::new(), paste: None, kitty: false }
    }

    /// Turns the protocol off again.
    pub fn finish(&mut self) {
        if std::mem::take(&mut self.kitty) {
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[<u").and_then(|()| out.flush());
        }
    }

    /// Waits up to `timeout` for input; returns what came.
    pub fn read(&mut self, timeout: Duration) -> io::Result<Vec<Input>> {
        if !wait(timeout)? {
            return Ok(Vec::new());
        }
        self.fill()?;
        let mut events = Vec::new();
        loop {
            if let Some(paste) = &mut self.paste {
                let Some(end) = self.buf.windows(6).position(|w| w == b"\x1b[201~") else {
                    paste.append(&mut self.buf);
                    break;
                };
                paste.extend(self.buf.drain(..end));
                self.buf.drain(..6);
                let text = String::from_utf8_lossy(&self.paste.take().unwrap_or_default()).into_owned();
                events.push(Input::Paste(text));
                continue;
            }
            match parse(&self.buf) {
                Parsed::Done(ev, n) => {
                    self.buf.drain(..n);
                    events.extend(ev);
                }
                Parsed::KittySupported(n) => {
                    self.buf.drain(..n);
                    if !self.kitty {
                        self.kitty = true;
                        let mut out = io::stdout();
                        out.write_all(format!("\x1b[>{KITTY_FLAGS}u").as_bytes())?;
                        out.flush()?;
                    }
                }
                Parsed::PasteStart(n) => {
                    self.buf.drain(..n);
                    self.paste = Some(Vec::new());
                }
                Parsed::Incomplete if self.buf == [0x1b] => {
                    // A lone ESC: the Esc key, unless the rest of a sequence is on its way.
                    if wait(ESC_WAIT)? {
                        self.fill()?;
                        continue;
                    }
                    self.buf.clear();
                    events.push(key(KeyCode::Esc, KeyModifiers::NONE));
                }
                Parsed::Incomplete => break,
            }
        }
        Ok(events)
    }

    fn fill(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 4096];
        let n = io::stdin().lock().read(&mut chunk)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }
}

/// Whether stdin has data within `timeout`.
fn wait(timeout: Duration) -> io::Result<bool> {
    let mut fd = libc::pollfd { fd: io::stdin().as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut fd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
    match r {
        r if r > 0 => Ok(true),
        0 => Ok(false),
        _ => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(mut bytes: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        loop {
            match parse(bytes) {
                Parsed::Done(ev, n) => {
                    out.extend(ev);
                    bytes = &bytes[n..];
                }
                Parsed::KittySupported(n) | Parsed::PasteStart(n) => bytes = &bytes[n..],
                Parsed::Incomplete => return out,
            }
        }
    }

    /// (typed char, hotkey char, hotkey modifiers) of one key press.
    fn press(bytes: &[u8]) -> (KeyCode, KeyCode, KeyModifiers) {
        match parse_all(bytes).as_slice() {
            [Input::Key(p)] => (p.typed.code, p.hotkey.code, p.hotkey.modifiers),
            other => panic!("{bytes:?}: {other:?}"),
        }
    }

    fn ch(c: char) -> KeyCode {
        KeyCode::Char(c)
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;

    /// What Konsole 26.08 sent with flags 29 (recorded): the same physical key in both layouts,
    /// with and without Shift and Caps Lock.
    #[test]
    fn kitty_physical_keys() {
        assert_eq!(press(b"\x1b[108;1;108u"), (ch('l'), ch('l'), NONE));
        assert_eq!(press(b"\x1b[108:76;2;76u"), (ch('L'), ch('L'), NONE));
        // Caps Lock: the text is upper case, the hotkey is not.
        assert_eq!(press(b"\x1b[108;1;76u"), (ch('L'), ch('l'), NONE));
        assert_eq!(press(b"\x1b[1076::108;1;1076u"), (ch('д'), ch('l'), NONE));
        assert_eq!(press(b"\x1b[1044::108;2;1044u"), (ch('Д'), ch('L'), NONE));
        assert_eq!(press(b"\x1b[1044::108;1;1044u"), (ch('Д'), ch('l'), NONE));
        // Shift on a non-letter key gives the US shifted character: `?` for help on any layout.
        assert_eq!(press(b"\x1b[46::47;2;44u"), (ch(','), ch('?'), NONE));
        // Ctrl+C in the Russian layout.
        assert_eq!(press(b"\x1b[1089::99;5u"), (ch('с'), ch('c'), KeyModifiers::CONTROL));
        assert_eq!(press(b"\x1b[32;1;32u"), (ch(' '), ch(' '), NONE));
        assert_eq!(press(b"\x1b[13;1;13u").1, KeyCode::Enter);
        assert_eq!(press(b"\x1b[27u").1, KeyCode::Esc);
        assert_eq!(press(b"\x1b[127u").1, KeyCode::Backspace);
        assert_eq!(press(b"\x1b[9;2u").1, KeyCode::BackTab);
        // Shift, Caps Lock, Ctrl pressed alone are no keys.
        assert!(parse_all(b"\x1b[57441;2u\x1b[57358u\x1b[57442;5u").is_empty());
        // Without the base key (a Latin layout, or a terminal that does not tell it) the Russian
        // layout is still mapped.
        assert_eq!(press(b"\x1b[1090;1;1090u"), (ch('т'), ch('n'), NONE));
    }

    #[test]
    fn legacy_keys() {
        assert_eq!(press(b"n"), (ch('n'), ch('n'), NONE));
        assert_eq!(press("т".as_bytes()), (ch('т'), ch('n'), NONE));
        assert_eq!(press(b"\x03"), (ch('c'), ch('c'), KeyModifiers::CONTROL));
        assert_eq!(press(b"\x1b[D").1, KeyCode::Left);
        assert_eq!(press(b"\x1bOA").1, KeyCode::Up);
        assert_eq!(press(b"\x1b[1;5C"), (KeyCode::Right, KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(press(b"\x1b[5~").1, KeyCode::PageUp);
        assert_eq!(press(b"\x1b[15~").1, KeyCode::F(5));
        assert_eq!(press(b"\x1bOP").1, KeyCode::F(1));
        assert_eq!(press(b"\x1b[Z").1, KeyCode::BackTab);
        assert_eq!(press(b"\r").1, KeyCode::Enter);
        assert_eq!(press(b"\x7f").1, KeyCode::Backspace);
        assert_eq!(press(b"\x1bx"), (ch('x'), ch('x'), KeyModifiers::ALT));
        // Replies of the terminal are skipped: device attributes, cursor position, kitty graphics.
        assert!(parse_all(b"\x1b[?62;1;4c\x1b[12;40R\x1b_Gi=1;OK\x1b\\\x1b]11;rgb:0/0/0\x07").is_empty());
        assert_eq!(parse(b"\x1b[?0u"), Parsed::KittySupported(5));
        // A sequence cut in the middle waits for the rest.
        assert_eq!(parse(b"\x1b[1;5"), Parsed::Incomplete);
        assert_eq!(parse(&"д".as_bytes()[..1]), Parsed::Incomplete);
    }

    #[test]
    fn layout() {
        assert_eq!(ru_to_en('д'), 'l');
        assert_eq!(ru_to_en('т'), 'n');
        assert_eq!(ru_to_en('Д'), 'L');
        assert_eq!(ru_to_en('.'), '/');
        assert_eq!(ru_to_en('Ф'), 'A');
        assert_eq!(ru_to_en('x'), 'x');
        assert_eq!(us_shift('/'), '?');
        assert_eq!(us_shift('l'), 'L');
        assert_eq!(us_shift('='), '+');
    }
}
