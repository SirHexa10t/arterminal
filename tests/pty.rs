//! The running program, end to end: the real binary on a pseudo-terminal, with this test playing
//! the terminal.
//!
//! # Why this exists beside the unit tests
//!
//! Everything that DECIDES what appears is pure and unit-tested. What is left is the loop that
//! connects it to a terminal — raw mode and its restoration, the keyboard-protocol handshake and
//! its undoing, the salvage on Ctrl+C, where the keys come from — and none of that can be seen
//! without a terminal. Each test here starts the binary on a fresh pseudo-terminal, answers the
//! questions it asks the way a real terminal would, types at it, and checks what it left behind:
//! the file, the exit code, and the terminal's own settings.
//!
//! # Rules for adding one
//!
//! * Wait for what the program SAYS, never for a length of time: [`Session::expect`] reads until
//!   the output contains something, with a generous deadline that only a hang ever reaches.
//! * Work on a copy in a directory of the test's own, never on a file in the repo.
//! * Never exercise a path that runs sudo. `--su` is driven only with stdin not a terminal, which
//!   is the path that must NOT ask.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long any one expectation may take. A healthy run answers in milliseconds; this is only
/// ever reached by a hang, and then it turns the hang into a failure with the output attached.
const DEADLINE: Duration = Duration::from_secs(10);

/// How the pretend terminal answers the keyboard-protocol question.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Terminal {
    /// Answers only the device attributes: no keyboard protocol, presses only.
    Classic,
    /// Speaks the kitty keyboard protocol and grants what is asked.
    Kitty,
}

/// One run of the binary on its own pseudo-terminal.
struct Session {
    master: File,
    slave: OwnedFd,
    child: Child,
    terminal: Terminal,
    /// Everything the program has written so far.
    out: Vec<u8>,
    /// How far into `out` the terminal's questions have been answered.
    answered: usize,
    /// The keyboard flags last pushed, which the next query reports.
    flags: u32,
    /// Whether this terminal draws East Asian Ambiguous characters two columns wide.
    wide: bool,
}

impl Session {
    /// Start the binary with `args`, `rows` by `cols`, playing `terminal`.
    fn start(args: &[OsString], terminal: Terminal, stdin_is_terminal: bool) -> Self {
        let (master, slave) = open_pty(24, 100);
        let stdin = match stdin_is_terminal {
            true => Stdio::from(slave.try_clone().expect("dup")),
            false => Stdio::null(),
        };
        let stderr = Stdio::from(slave.try_clone().expect("dup"));
        let tty = slave.as_raw_fd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_arterminal"));
        command
            .args(args)
            .stdin(stdin)
            .stdout(Stdio::null())
            .stderr(stderr)
            .env("TERM", "xterm-256color")
            .env_remove("SUDO_UID")
            .env_remove("NO_COLOR");
        // SAFETY: only async-signal-safe calls between fork and exec — a new session, then the
        // pty as its controlling terminal, so the program's /dev/tty is this pty.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(tty, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("the binary starts");
        Self { master, slave, child, terminal, out: Vec::new(), answered: 0, flags: 0, wide: false }
    }

    /// Read until the output's TEXT contains `needle` — escape sequences removed, because what a
    /// test asks is what the program said, and the colour between a key and its action is layout
    /// — answering the terminal's questions on the way.
    fn expect(&mut self, needle: &str) {
        let start = Instant::now();
        while !self.text().contains(needle) {
            assert!(
                start.elapsed() < DEADLINE,
                "never saw {needle:?}; the output was:\n{}",
                String::from_utf8_lossy(&self.out)
            );
            self.pump(Duration::from_millis(50));
        }
    }

    /// Read whatever arrives within `wait`, and answer any questions in it.
    fn pump(&mut self, wait: Duration) {
        let mut poll =
            libc::pollfd { fd: self.master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, owned here, for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, wait.as_millis() as libc::c_int) };
        if ready > 0 {
            let mut chunk = [0u8; 65536];
            match self.master.read(&mut chunk) {
                Ok(n) => self.out.extend_from_slice(&chunk[..n]),
                Err(_) => return, // the other side is gone: the program exited
            }
        }
        self.answer();
    }

    /// Answer every question asked since the last answer, in the order asked, the way
    /// `self.terminal` would: a keyboard-flags push is remembered, the flags query answered only
    /// by a kitty-protocol terminal, a cursor-position request with where a `·` left the cursor —
    /// column 2, or 3 on a terminal drawing ambiguous characters wide — and the device
    /// attributes always.
    fn answer(&mut self) {
        const QUERIES: [&[u8]; 4] = [b"\x1b[>", b"\x1b[?u", b"\x1b[6n", b"\x1b[c"];
        loop {
            let pending = &self.out[self.answered..];
            let Some((at, query)) = QUERIES
                .iter()
                .filter_map(|query| find(pending, query).map(|at| (at, *query)))
                .min_by_key(|(at, _)| *at)
            else {
                break;
            };
            let after = self.answered + at + query.len();
            let reply = match query {
                b"\x1b[>" => {
                    // A push, `CSI > flags u` — incomplete until its `u` has arrived.
                    let Some(end) = self.out[after..].iter().position(|&b| b == b'u') else {
                        break;
                    };
                    let digits = std::str::from_utf8(&self.out[after..after + end]).unwrap_or("");
                    self.flags = digits.parse().unwrap_or(0);
                    self.answered = after + end + 1;
                    continue;
                }
                b"\x1b[?u" => match self.terminal {
                    Terminal::Kitty => format!("\x1b[?{}u", self.flags),
                    Terminal::Classic => String::new(),
                },
                b"\x1b[6n" => format!("\x1b[1;{}R", if self.wide { 3 } else { 2 }),
                _ => "\x1b[?62;22c".to_string(),
            };
            self.answered = after;
            self.send(reply.as_bytes());
        }
    }

    /// Everything written so far, as text with every control sequence taken out.
    fn text(&self) -> String {
        strip(&String::from_utf8_lossy(&self.out))
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("the pty takes input");
    }

    /// Type `keys`, one sequence at a time, each after the frame for the one before is drawn.
    fn keys(&mut self, keys: &[&[u8]]) {
        for key in keys {
            self.send(key);
            self.pump(Duration::from_millis(30));
        }
    }

    /// Wait for the program to exit, and hand back how it did and what the terminal was left as.
    /// Everything it wrote stays in `out`.
    fn finish(&mut self) -> (ExitStatus, libc::termios) {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("waitable") {
                break status;
            }
            if start.elapsed() > DEADLINE {
                self.child.kill().ok();
                panic!("the program never exited:\n{}", String::from_utf8_lossy(&self.out));
            }
            self.pump(Duration::from_millis(20));
        };
        // The last things a program writes — the flags' pop, the erased frame — are still in the
        // pty when it exits. Drain them before anyone asks what was said.
        let mut drained = self.out.len();
        loop {
            self.pump(Duration::from_millis(50));
            if self.out.len() == drained {
                break;
            }
            drained = self.out.len();
        }
        // SAFETY: a termios filled by tcgetattr on a live descriptor.
        let settings = unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            libc::tcgetattr(self.slave.as_raw_fd(), &mut settings);
            settings
        };
        (status, settings)
    }
}

/// A failed assertion must not leave the program running: that turns one failure into a hung
/// run and a leaked process. Harmless after [`Session::finish`], which has already reaped it.
impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn open_pty(rows: u16, cols: u16) -> (File, OwnedFd) {
    // Mutable, and passed as a RAW pointer: Linux declares these two parameters `const` and the
    // BSDs do not, a `*mut` goes where a `*const` is wanted but not the other way round — and a
    // `&mut` would draw Linux's lint against needless mutable references.
    let mut size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master, mut slave) = (0, 0);
    // SAFETY: out-pointers to locals; the name buffer is not asked for; the size outlives the call.
    let made = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    assert_eq!(made, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors were just opened for us and are owned by nothing else.
    unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// Whether the terminal was handed back in its ordinary, cooked state.
fn restored(settings: &libc::termios) -> bool {
    let local = settings.c_lflag;
    local & libc::ICANON != 0 && local & libc::ECHO != 0 && local & libc::ISIG != 0
}

/// A directory of the test's own, with `art` written into it as `art.txt`.
fn scratch(test: &str, art: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("arterminal-pty-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("art.txt");
    std::fs::write(&path, art).expect("written");
    (dir, path)
}

/// The cells a document on disk has ink on.
fn inked(path: &Path) -> Vec<(usize, usize)> {
    let text = std::fs::read_to_string(path).expect("readable");
    let doc = arterminal::document::parse(&text).expect("a document");
    let mut cells = Vec::new();
    for (y, row) in doc.canvas.rows().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            if cell.ink.is_some() {
                cells.push((x, y));
            }
        }
    }
    cells
}

const DOTS: &str = "......\n......\n......\n";
const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";
const RIGHT: &[u8] = b"\x1b[C";
const PAGE_UP: &[u8] = b"\x1b[5~";

/// A classic terminal reports presses only: Space taps one cell, `b` drags, and Ctrl+C keeps the
/// unsaved work beside the file — and the terminal comes back exactly as it was lent.
#[test]
fn a_classic_terminal_taps_toggles_and_salvages_on_ctrl_c() {
    let (dir, path) = scratch("classic", DOTS);
    let mut session = Session::start(&[path.clone().into()], Terminal::Classic, true);
    session.expect("^X/esc close");
    session.expect("run with --su");
    // [+] opens the colour dial; Enter keeps the colour it offers and asks its name; Enter keeps
    // that as offered too. (Not Esc: a lone Esc is only Esc once the decoder's timeout has passed,
    // and this file never waits on a clock.) Then pick it as the brush, and go down to (0,0).
    session.keys(&[b"\r", b"\r", b"\r", UP, b" ", DOWN, DOWN]);
    session.keys(&[b" ", RIGHT, b"b", RIGHT, RIGHT, b"b", RIGHT]);
    session.keys(&[b"\x03"]);
    let (status, settings) = session.finish();
    assert!(status.success(), "exit {status}");
    assert!(restored(&settings), "raw mode was undone");
    let salvaged = path.with_file_name("art.txt.arterminal.tmp");
    assert_eq!(inked(&salvaged), [(0, 0), (1, 0), (2, 0), (3, 0)], "a tap, then a toggled drag");
    assert!(inked(&path).is_empty(), "the original is untouched");
    std::fs::remove_dir_all(dir).ok();
}

/// A kitty-protocol terminal reports releases: the pen is down exactly while Space is. The flags
/// pushed at the start come off at the end, and Ctrl+S writes the file itself.
#[test]
fn a_kitty_terminal_holds_keys_saves_and_pops_its_flags() {
    let (dir, path) = scratch("kitty", DOTS);
    let mut session = Session::start(&[path.clone().into()], Terminal::Kitty, true);
    session.expect("^X/esc close");
    assert!(contains(&session.out, b"\x1b[>27u"), "the protocol was asked for");
    // [+]: the dial, its colour kept, and its name as offered — Esc there gives up only renaming.
    session.keys(&[b"\x1b[13u", b"\x1b[13u", b"\x1b[27u"]);
    session.keys(&[UP, b"\x1b[32u", b"\x1b[32;1:3u", DOWN, DOWN]);
    session.keys(&[b"\x1b[32u", RIGHT, RIGHT, b"\x1b[32;1:3u", RIGHT]);
    session.keys(&[b"\x1b[122;5u", b"\x1b[122;6u"]); // undo, then ctrl+shift+z redoes it
    session.keys(&[b"\x1b[115;5u"]); // ctrl+s
    session.expect("saved to");
    session.keys(&[b"\x1b[27u"]);
    let (status, settings) = session.finish();
    assert!(status.success(), "exit {status}");
    assert!(restored(&settings), "raw mode was undone");
    assert!(contains(&session.out, b"\x1b[<u"), "and the protocol's flags were popped");
    assert_eq!(inked(&path), [(0, 0), (1, 0), (2, 0)], "held Space painted exactly its drag");
    std::fs::remove_dir_all(dir).ok();
}

/// `--su` with no terminal on stdin must not ask for a password — a pipeline cannot answer one —
/// and goes ahead without held keys, saying why. The one `--su` path that never runs sudo, which
/// is exactly why it is the one tested here.
#[test]
fn su_with_no_terminal_on_stdin_goes_ahead_without_asking() {
    let (dir, path) = scratch("su", DOTS);
    let args: [OsString; 2] = ["--su".into(), path.clone().into()];
    let mut session = Session::start(&args, Terminal::Classic, false);
    session.expect("--su needs a terminal to ask for a password");
    session.expect("^X/esc close");
    assert!(!contains(&session.out, b"THIS RUN only"), "no password prompt was printed");
    session.keys(&[b"\x03"]);
    let (status, settings) = session.finish();
    assert!(status.success(), "exit {status}");
    assert!(restored(&settings));
    std::fs::remove_dir_all(dir).ok();
}

/// A drawing past the size line in either direction is asked about before anything is built;
/// declining opens nothing and leaves quietly, and agreeing opens it as usual.
#[test]
fn a_drawing_past_the_size_line_is_asked_about_before_it_is_built() {
    let wide = format!("{}\n..\n", ".".repeat(10_001));
    for (answer, opens) in [(b"n\r", false), (b"y\r", true)] {
        let (dir, path) = scratch(if opens { "large-yes" } else { "large-no" }, &wide);
        let mut session = Session::start(&[path.clone().into()], Terminal::Classic, true);
        session.expect("is 10,001 columns by 2 rows");
        session.expect("Open it anyway? [y/N]");
        session.send(answer);
        match opens {
            true => {
                session.expect("^X/esc close");
                session.keys(&[b"\x03"]);
            }
            false => session.expect("not opened"),
        }
        let (status, settings) = session.finish();
        assert!(status.success(), "exit {status}");
        assert!(restored(&settings));
        assert_eq!(session.text().contains("^X/esc close"), opens, "opened only on a yes");
        std::fs::remove_dir_all(dir).ok();
    }
}

/// A file coloured with a terminal palette slot opens with nothing asked: the slot becomes a
/// slot swatch, tagged as one, and saving writes it back as the same slot — the file says what it
/// said.
#[test]
fn slot_colours_open_as_slot_swatches_and_save_back_as_slots() {
    let slotted = "\x1b[38;5;196mab\x1b[0m..\n";
    let (dir, path) = scratch("slots", slotted);
    let mut session = Session::start(&[path.clone().into()], Terminal::Kitty, true);
    session.expect("slot 196");
    session.expect("^X/esc close");
    assert!(!session.text().contains("[y/N]"), "nothing is asked about a slot");
    session.keys(&[b"\x1b[115;5u"]); // ctrl+s
    session.expect("saved to");
    session.keys(&[b"\x1b[27u"]);
    let (status, _) = session.finish();
    assert!(status.success(), "exit {status}");
    let saved = std::fs::read_to_string(&path).expect("readable");
    assert!(
        saved.starts_with("\x1b[38;5;196mab\x1b[0m..\n"),
        "the slot, spelled as drawn: {saved:?}"
    );
    assert!(saved.contains("colour 1\tslot 196\n"), "and its swatch, as a slot: {saved:?}");
    std::fs::remove_dir_all(dir).ok();
}

/// The colour dial, end to end: F2 opens it on the swatch's colour, Page Up turns the hue ten
/// degrees a press, Enter keeps the colour — the drawing recoloured with it — and Ctrl+S writes
/// what was kept.
#[test]
fn the_dial_recolours_a_swatch_and_the_drawing_with_it() {
    let (dir, path) = scratch("dial", "\x1b[38;2;255;0;0mab\x1b[0m..\n");
    let mut session = Session::start(&[path.clone().into()], Terminal::Kitty, true);
    session.expect("^X/esc close");
    session.keys(&[b"\x1bOQ"]); // F2, on the red swatch the cursor starts on
    session.expect("H:   0 ; S: 100 ; B: 100");
    session.keys(&[PAGE_UP; 12]);
    session.expect("H: 120 ; S: 100 ; B: 100");
    session.keys(&[b"\x1b[13u", b"\x1b[13u"]); // keep the colour, then the name
    session.keys(&[b"\x1b[115;5u"]); // ctrl+s
    session.expect("saved to");
    session.keys(&[b"\x1b[27u"]);
    let (status, _) = session.finish();
    assert!(status.success(), "exit {status}");
    let saved = std::fs::read_to_string(&path).expect("readable");
    assert!(saved.starts_with("\x1b[38;2;0;255;0mab\x1b[0m..\n"), "green now: {saved:?}");
    assert!(saved.contains("colour 1\t#00ff00\n"), "and so is its swatch: {saved:?}");
    std::fs::remove_dir_all(dir).ok();
}

/// A colour typed on the dial, end to end: F2, then `#` and six hex digits typed as a terminal
/// sends them, and Ctrl+S writes exactly that colour — no trip through hue and back.
#[test]
fn a_colour_typed_in_hex_is_saved_exactly() {
    let (dir, path) = scratch("hex", "\x1b[38;2;255;0;0mab\x1b[0m..\n");
    let mut session = Session::start(&[path.clone().into()], Terminal::Kitty, true);
    session.expect("^X/esc close");
    session.keys(&[b"\x1bOQ", b"#", b"4", b"b", b"0", b"0", b"8", b"2"]); // F2, then #4b0082
    session.expect("#4b0082");
    session.keys(&[b"\x1b[13u", b"\x1b[13u"]); // keep the colour, then the name
    session.keys(&[b"\x1b[115;5u"]); // ctrl+s
    session.expect("saved to");
    session.keys(&[b"\x1b[27u"]);
    let (status, _) = session.finish();
    assert!(status.success(), "exit {status}");
    let saved = std::fs::read_to_string(&path).expect("readable");
    assert!(saved.starts_with("\x1b[38;2;75;0;130mab\x1b[0m..\n"), "indigo, exactly: {saved:?}");
    assert!(saved.contains("colour 1\t#4b0082\n"), "and its swatch: {saved:?}");
    std::fs::remove_dir_all(dir).ok();
}

/// A crumpled save from the running program, with either key: Ctrl+Shift+S on a terminal that
/// speaks the kitty protocol, Alt+S on one that does not. What lands beside the file is the
/// drawing crumpled — and the file itself is untouched.
#[test]
fn a_crumpled_save_lands_beside_the_file_from_either_key() {
    let keys: [(Terminal, &[u8], &str); 2] =
        [(Terminal::Kitty, b"\x1b[115;6u", "kitty"), (Terminal::Classic, b"\x1bs", "classic")];
    for (terminal, key, name) in keys {
        let (dir, path) = scratch(&format!("crumple-{name}"), DOTS);
        let mut session = Session::start(&[path.clone().into()], terminal, true);
        session.expect("^X/esc close");
        session.keys(&[key]);
        session.expect("crumpled to");
        session.keys(&[b"\x18"]); // ctrl+x: nothing unsaved, so it closes at once
        let (status, _) = session.finish();
        assert!(status.success(), "{name}: exit {status}");
        let crumpled = std::fs::read_to_string(path.with_file_name("art.txt.crumpled"))
            .unwrap_or_else(|why| panic!("{name}: no crumpled copy: {why}"));
        let plain = arterminal::crumple::uncrumple(&crumpled).expect("a crumpled form");
        assert!(plain.starts_with(DOTS.trim_end()), "{name}: the drawing: {plain:?}");
        assert_eq!(std::fs::read_to_string(&path).expect("readable"), DOTS, "{name}: untouched");
        std::fs::remove_dir_all(dir).ok();
    }
}

/// A drawing opened from its crumpled form, alone in its directory, saves plain with Ctrl+S —
/// to its name without `.crumpled`, as text — and leaves the crumpled file as it was.
#[test]
fn ctrl_s_saves_a_crumpled_drawing_plain_under_its_plain_name() {
    let (dir, plain) = scratch("uncrumple", "");
    std::fs::remove_file(&plain).expect("only the crumpled form is there");
    let crumpled = plain.with_file_name("art.txt.crumpled");
    let form = arterminal::crumple::crumple(DOTS);
    std::fs::write(&crumpled, &form).expect("written");
    let mut session = Session::start(&[crumpled.clone().into()], Terminal::Kitty, true);
    session.expect("^X/esc close");
    session.keys(&[b"\x1b[115;5u"]); // ctrl+s
    session.expect("saved to");
    session.keys(&[b"\x1b[27u"]);
    let (status, _) = session.finish();
    assert!(status.success(), "exit {status}");
    assert_eq!(std::fs::read_to_string(&plain).expect("a plain file now"), DOTS, "as text");
    assert_eq!(std::fs::read_to_string(&crumpled).expect("still there"), form, "untouched");
    std::fs::remove_dir_all(dir).ok();
}

/// A terminal that draws East Asian Ambiguous characters wide is asked, not guessed at: the
/// probe's `·` is drawn and erased again, and every line of the frame then fits the terminal as
/// IT measures them — the `·` between hints two columns each.
#[test]
fn a_terminal_that_draws_ambiguous_characters_wide_gets_frames_that_fit_it() {
    let (dir, path) = scratch("wide", DOTS);
    let mut session = Session::start(&[path.clone().into()], Terminal::Classic, true);
    session.wide = true;
    session.expect("^X/esc close");
    session.keys(&[DOWN]); // one more frame, drawn after the answer for certain
    session.expect("Draw:");
    let raw = String::from_utf8_lossy(&session.out).into_owned();
    let probe = raw.find("\r\u{b7}\x1b[6n").expect("the terminal was asked");
    assert!(raw[probe..].contains("\r\x1b[K"), "and the probe's glyph erased again");
    let last = raw.rfind("A").map_or(raw.as_str(), |at| &raw[at + 1..]);
    // Each line ends in an erase-to-end, then a newline the pty turns into CR LF on the way out.
    for line in last.split("\x1b[K") {
        let plain = strip(line.trim_start_matches(['\r', '\n']));
        let columns = unicode_width::UnicodeWidthStr::width_cjk(plain.as_str());
        assert!(columns <= 100, "{columns} columns on a 100-column terminal: {plain:?}");
    }
    session.keys(&[b"\x03"]);
    let (status, _) = session.finish();
    assert!(status.success(), "exit {status}");
    std::fs::remove_dir_all(dir).ok();
}

/// Text with every CSI sequence taken out.
fn strip(line: &str) -> String {
    let mut text = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.next_if_eq(&'[').is_some() {
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
            continue;
        }
        text.push(c);
    }
    text
}
