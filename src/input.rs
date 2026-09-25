//! Reading keys: raw mode for the whole run, a blocking wait that costs nothing while idle, and
//! the rule for folding a burst of keystrokes into one repaint.
//!
//! # Provenance
//!
//! Ported, with [`crate::paint`], from `src/ui.rs` of the sibling project `terminal_choice`
//! (commit `4f7a222`). Both projects are GPL-3.0-only and share an owner. See that module's
//! header for why this is a copy rather than a shared crate.

/// How many keystrokes may be folded into one frame before painting anyway.
///
/// Insurance rather than tuning. The drain ends on its own the moment the terminal has nothing
/// waiting, and reading a key is microseconds against milliseconds to paint — so a human, even
/// leaning on an arrow key, can never outrun it. What this bounds is the pathological case: a
/// paste, or a terminal replaying a long escape burst, where input arrives faster than any
/// reader. Without it, such a stream could hold the screen still indefinitely.
pub(crate) const COALESCE: usize = 128;

/// Whether to swallow the repaint this keystroke earned, because more input is already waiting.
///
/// Pure, so the one rule that must never bend can be tested: A KEYSTROKE THAT ENDS THE RUN IS
/// NEVER SWALLOWED. A run that has ended is not waiting for a tidier moment to say so.
pub(crate) fn coalesce(redraws: bool, pending: bool, drained: usize) -> bool {
    redraws && pending && drained < COALESCE
}

/// The input the picker reads: stdin when it is a terminal, `/dev/tty` otherwise — the same
/// choice `console` makes internally, so the descriptor we wait on and configure is the one it
/// reads. The `File` half keeps a non-stdin tty open for as long as the handle lives.
pub(crate) fn input_fd() -> std::io::Result<(std::os::fd::RawFd, Option<std::fs::File>)> {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        Ok((stdin.as_raw_fd(), None))
    } else {
        let tty = std::fs::File::open("/dev/tty")?;
        Ok((tty.as_raw_fd(), Some(tty)))
    }
}

/// Raw terminal mode for the WHOLE run, restored on drop.
///
/// `console` only enters raw mode inside each `read_key` call — correct for a one-off prompt,
/// but between this loop's keys the terminal would sit in cooked mode with echo on: every arrow
/// press would PRINT (`^[[B`) instead of acting, and canonical buffering would hold the bytes
/// back until Enter, so the picker would seem deaf. (Exactly that shipped once in the sibling
/// project; scripted ptys masked it, because their input arrives pre-buffered with newlines in
/// it.) Holding raw for the run's lifetime gives byte-at-a-time reads with no echo; the output
/// flags keep their original state so `\n` still starts a fresh line.
pub(crate) struct RawMode {
    fd: std::os::fd::RawFd,
    original: libc::termios,
}

impl RawMode {
    pub(crate) fn engage(fd: std::os::fd::RawFd) -> std::io::Result<Self> {
        // SAFETY: tcgetattr/tcsetattr write only the termios handed to them; the fd is the
        // terminal this picker runs on.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut original) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            raw.c_oflag = original.c_oflag; // keep output post-processing: `\n` stays a newline
            if libc::tcsetattr(fd, libc::TCSADRAIN, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { fd, original })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the very state tcgetattr produced, on the same fd.
        unsafe {
            let _ = libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.original);
        }
    }
}

/// What a bounded wait on the terminal found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// Bytes are waiting to be read.
    Ready,
    /// The time ran out with nothing to read.
    TimedOut,
    /// The terminal went away — which a caller should read as closing rather than spin on (a
    /// hung-up fd stays "ready" for ever without ever having input).
    HungUp,
}

/// The kitty keyboard protocol, turned on for the whole run and off again on drop — the only way
/// a terminal will say a key was RELEASED. See [`crate::keys`].
///
/// Declared after [`RawMode`] wherever both are held, so it drops first and the flags come off
/// while the terminal is still raw.
///
/// WHAT DROP DOES NOT COVER, stated exactly rather than as "every path": `Drop` runs on scope exit
/// and on unwinding panics. It does NOT run on `std::process::exit`, `abort`, or a signal —
/// SIGTERM from `kill`, SIGHUP when the window closes. The first two do not occur while a picker
/// runs today (the binary's `fail` exits only after `run` has returned); the signals can. That
/// exposure is not new — [`RawMode`] has it too — but the consequence is worse here: flag 8 makes
/// EVERY key an escape sequence, so a shell left with the flags pushed cannot even be typed into
/// well enough to run `reset`. A handler that writes the pop (`write(2)` is async-signal-safe) and
/// re-raises would close it; it is deliberately a separate decision, not built yet.
pub(crate) struct KeyboardProtocol;

/// What the terminal agreed to, of the flags asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Granted {
    /// Key releases arrive: flags 2 and 8 both — see [`KeyboardProtocol::FLAGS`].
    pub(crate) releases: bool,
    /// Modified keys arrive unambiguous — flag 1 — so Shift on a Ctrl chord is reported, and
    /// Ctrl+Shift+Z is told from Ctrl+Z. A classic terminal sends one byte for both.
    pub(crate) shift_on_ctrl: bool,
}

impl KeyboardProtocol {
    /// The flags asked for: disambiguate (1), event types (2), every key as an escape (8), and the
    /// text each key produced (16).
    ///
    /// Flag 8 is required for releases of the keys this program holds, by TWO rules of the spec
    /// that are worth citing separately, because only one of them names Space: Enter, Tab and
    /// Backspace "will not have release events unless Report all keys as escape codes is also
    /// set" (stated outright); and a key that produces text — Space — is "reported as plain UTF-8
    /// text", with no event structure for a release to live in, unless flag 8 turns it into an
    /// escape (the general rule). Flag 8 withholds typed text in exchange, which 16 puts back so
    /// names can still be typed. Flag 4, alternate keys, is not needed and not asked for.
    const FLAGS: u32 = 1 | 2 | 8 | 16;

    /// Ask whether the terminal speaks the protocol and, if it does, turn it on.
    ///
    /// Returns the guard and what was really granted: a terminal can answer the query and still
    /// decline some flags, so after pushing them it is asked again. Keys typed while this runs are
    /// not lost — they are decoded into `backlog` for the caller to replay.
    pub(crate) fn engage(
        fd: std::os::fd::RawFd,
        decoder: &mut crate::keys::Decoder,
        backlog: &mut Vec<crate::keys::KeyEvent>,
    ) -> std::io::Result<Option<(Self, Granted)>> {
        if query_flags(fd, decoder, backlog)?.is_none() {
            return Ok(None);
        }
        write_to_terminal(&format!("\x1b[>{}u", Self::FLAGS))?;
        let guard = Self;
        let flags = query_flags(fd, decoder, backlog)?.unwrap_or(0);
        let granted =
            Granted { releases: flags & 0b1010 == 0b1010, shift_on_ctrl: flags & 0b1 != 0 };
        Ok(Some((guard, granted)))
    }
}

impl Drop for KeyboardProtocol {
    fn drop(&mut self) {
        let _ = write_to_terminal("\x1b[<u");
    }
}

/// How long to wait for a terminal to answer the device-attributes query. Every real terminal
/// answers within milliseconds locally; the margin is for ssh. One that never answers is treated
/// as not speaking the protocol, which costs nothing but hold-to-paint.
const QUERY_TIMEOUT_MS: i32 = 500;

/// `CSI ? u` then `CSI c`, per the spec's detection recipe: a flags reply before the attributes
/// reply means the protocol is spoken; the attributes reply alone means it is not.
fn query_flags(
    fd: std::os::fd::RawFd,
    decoder: &mut crate::keys::Decoder,
    backlog: &mut Vec<crate::keys::KeyEvent>,
) -> std::io::Result<Option<u32>> {
    use crate::keys::Decoded;
    write_to_terminal("\x1b[?u\x1b[c")?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(QUERY_TIMEOUT_MS as u64);
    let mut flags = None;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
        if left == 0 || wait_for_input(fd, left)? != Readiness::Ready {
            return Ok(flags);
        }
        for decoded in decoder.feed(&read_available(fd)?) {
            match decoded {
                Decoded::KeyboardFlags(value) => flags = Some(value),
                Decoded::DeviceAttributes => return Ok(flags),
                Decoded::Key(key) => backlog.push(key),
                Decoded::CursorPosition { .. } => {}
            }
        }
    }
}

/// Whether this terminal draws East Asian Ambiguous characters two columns wide — MEASURED, by
/// drawing one (`·`) at the start of the line and asking where the cursor went: column 2 is one
/// cell, column 3 two. `None` when the terminal does not answer, so the caller can fall back to
/// something weaker; the device attributes ride behind the question as its end marker, so a
/// silent terminal costs nothing. The glyph is erased again either way.
///
/// Asked, not taken from the locale, because the setting belongs to the terminal: an English
/// locale can run a terminal set to wide, and a Japanese one a terminal set to narrow.
pub(crate) fn ambiguous_is_wide(
    fd: std::os::fd::RawFd,
    decoder: &mut crate::keys::Decoder,
    backlog: &mut Vec<crate::keys::KeyEvent>,
) -> std::io::Result<Option<bool>> {
    use crate::keys::Decoded;
    decoder.expect_cursor_report(true);
    let asked = (|| -> std::io::Result<Option<u32>> {
        write_to_terminal("\r\u{b7}\x1b[6n\x1b[c")?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(QUERY_TIMEOUT_MS as u64);
        let mut column = None;
        loop {
            let left =
                deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
            if left == 0 || wait_for_input(fd, left)? != Readiness::Ready {
                return Ok(column);
            }
            for decoded in decoder.feed(&read_available(fd)?) {
                match decoded {
                    Decoded::CursorPosition { col, .. } => column = Some(col),
                    Decoded::DeviceAttributes => return Ok(column),
                    Decoded::Key(key) => backlog.push(key),
                    Decoded::KeyboardFlags(_) => {}
                }
            }
        }
    })();
    decoder.expect_cursor_report(false);
    write_to_terminal("\r\x1b[K")?;
    Ok(asked?.map(|column| column >= 3))
}

/// The locale's word on ambiguous width, for a terminal that gave none: Chinese, Japanese and
/// Korean locales are where terminals most often draw those characters wide. A weak guess — the
/// setting is the terminal's own — so it is only ever the fallback for [`ambiguous_is_wide`].
pub(crate) fn locale_says_wide() -> bool {
    locale_is_cjk(
        ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
            .as_deref(),
    )
}

/// Whether the locale `name` is a Chinese, Japanese or Korean one.
fn locale_is_cjk(name: Option<&str>) -> bool {
    name.is_some_and(|name| ["ja", "zh", "ko"].iter().any(|lang| name.starts_with(lang)))
}

/// A control sequence to the terminal, flushed at once. Stderr, because that is where the picker
/// draws and so the stream that reaches the terminal it is talking to.
fn write_to_terminal(sequence: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    err.write_all(sequence.as_bytes())?;
    err.flush()
}

/// Sleep until the terminal has input, is gone, or `timeout_ms` passes — negative for no limit.
///
/// Unbounded, this is THE idle state of a running picker, so it is a plain blocking `poll` owned
/// here: whether an idle picker costs 0% CPU should not depend on a dependency's internals. It
/// only works because [`RawMode`] holds the terminal non-canonical — cooked mode releases bytes
/// to `poll` a full line at a time. Bounded, it is how a lone `ESC` is told apart from the start
/// of a longer sequence: see [`ESCAPE_TIMEOUT_MS`].
pub(crate) fn wait_for_input(
    fd: std::os::fd::RawFd,
    timeout_ms: i32,
) -> std::io::Result<Readiness> {
    let mut watch = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    loop {
        // SAFETY: `poll` reads and writes only the pollfd handed to it, which lives on this
        // stack frame.
        let ready = unsafe { libc::poll(&mut watch, 1, timeout_ms) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // a signal woke us early; the wait itself is still on
            }
            return Err(err);
        }
        if ready == 0 {
            return Ok(Readiness::TimedOut);
        }
        if watch.revents & libc::POLLIN != 0 {
            return Ok(Readiness::Ready);
        }
        if watch.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(Readiness::HungUp);
        }
    }
}

/// How long to wait for the rest of an escape sequence before deciding a lone `ESC` was the
/// Escape key.
///
/// A terminal writes a whole sequence at once, so its bytes arrive within microseconds on a local
/// machine; the margin is for ssh, where a sequence can straddle two packets. 25 ms is well under
/// what a person perceives as lag on the Escape key and well over one network hop on a sane link.
/// Too short and an arrow over a slow link turns into an Escape; too long and closing feels sticky.
pub(crate) const ESCAPE_TIMEOUT_MS: i32 = 25;

/// How long to wait for the rest of a sequence that is PROVABLY unfinished — an `ESC [` or `ESC O`
/// introducer held, a UTF-8 character split across reads. No valid input ends there, so this is
/// not a latency bet the way [`ESCAPE_TIMEOUT_MS`] is: it only bounds how long a link can stall
/// mid-sequence before the fragment is dropped rather than held for ever. Generous on purpose —
/// cutting it short is the exact tear the decoder exists to prevent.
pub(crate) const SEQUENCE_TIMEOUT_MS: i32 = 500;

/// Whatever the terminal has sent, up to what one read returns. Empty means the fd closed.
pub(crate) fn read_available(fd: std::os::fd::RawFd) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: `read` writes at most `buf.len()` bytes into `buf`, which lives here.
        let got = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        buf.truncate(got as usize);
        return Ok(buf);
    }
}

/// Whether a keystroke is ALREADY waiting to be read — asked, not waited on.
///
/// The same `poll` as [`await_input`] with a zero timeout, which is the whole difference: that
/// one blocks until there is news, this one reports whether there is any right now.
pub(crate) fn input_pending(fd: std::os::fd::RawFd) -> bool {
    let mut watch = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: as `await_input` — `poll` touches only the pollfd on this stack frame. A zero
    // timeout cannot block.
    unsafe { libc::poll(&mut watch, 1, 0) > 0 && watch.revents & libc::POLLIN != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holding an arrow key used to queue one full repaint per keystroke, and a big grid takes
    /// long enough to draw that the backlog shows as the UI freezing for seconds. Keys are folded
    /// into one frame while more are already waiting.
    #[test]
    fn keystrokes_are_folded_into_one_frame_while_more_are_queued() {
        assert!(coalesce(true, true, 0), "more waiting — no need to paint yet");
        assert!(!coalesce(true, false, 0), "nothing waiting — this is the frame to paint");
    }

    /// The rule that must never bend. A keystroke that ends the run is not a redraw, so it can
    /// never be deferred for a tidier moment however much input is queued behind it.
    #[test]
    fn ending_the_run_is_never_folded_away() {
        assert!(!coalesce(false, true, 0), "closing is not a repaint to be swallowed");
        assert!(!coalesce(false, true, COALESCE - 1));
    }

    /// The escape hatch: a paste long enough to outrun the drain gets painted anyway, so the
    /// screen cannot be held still by a stream that never stops.
    #[test]
    fn a_long_enough_burst_paints_anyway() {
        assert!(coalesce(true, true, COALESCE - 1), "still inside the allowance");
        assert!(!coalesce(true, true, COALESCE), "allowance spent — paint regardless");
        assert!(!coalesce(true, true, COALESCE * 10));
    }
}
