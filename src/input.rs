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

/// Sleep until the terminal has input (or is gone). `Ok(true)`: a key is waiting, and
/// `Term::read_key` will return without ever reaching its zero-timeout polling. `Ok(false)`:
/// hangup — the terminal went away, which a caller should read as closing rather than spin on
/// (a hung-up fd stays "ready" forever without ever having input).
///
/// This is THE idle state of a running picker, so it is a plain blocking `poll` owned here:
/// whether an idle picker costs 0% CPU should not depend on a dependency's key-reading
/// internals. It only works because [`RawMode`] holds the terminal non-canonical — cooked mode
/// releases bytes to `poll` a full line at a time.
pub(crate) fn await_input(fd: std::os::fd::RawFd) -> std::io::Result<bool> {
    let mut watch = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    loop {
        // SAFETY: `poll` reads and writes only the pollfd handed to it, which lives on this
        // stack frame; a negative timeout blocks until the fd has news.
        let ready = unsafe { libc::poll(&mut watch, 1, -1) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // a signal woke us early; the wait itself is still on
            }
            return Err(err);
        }
        if watch.revents & libc::POLLIN != 0 {
            return Ok(true);
        }
        if watch.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(false);
        }
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
