//! Asking the kernel which keys are physically down — the release source for terminals that
//! report none.
//!
//! A classic terminal says when a key goes down and never when it comes up, so "paint while Space
//! is held" cannot be heard through it. The kitty keyboard protocol fixes that where the terminal
//! speaks it (see [`crate::keys`]). Everywhere else, Linux will answer the question directly: the
//! input devices under `/dev/input` report the state of every key, below X11, Wayland and the
//! console alike. That is the approach the developer's own `sequencer` takes for the same reason.
//!
//! # What is read, and what is never read
//!
//! The devices are asked ONE question — `EVIOCGKEY`, "which keys are down right now?" — and
//! `read()` is never called on them. So the keystroke STREAM never enters this process: no key
//! events, no timing, nothing typed into another window. The answer to the question is a bitmap of
//! the whole keyboard, which is unavoidable with this call; it is used for the few keys a stroke is
//! bound to and dropped. And it is asked only at the moments that need it: on a key the TERMINAL
//! delivered, and while a stroke is held.
//!
//! Only RELEASES are ever taken from here. A stroke begins on the terminal's own key press, which
//! proves the terminal has focus — Wayland offers no other way to know — and the device only says
//! when the physical key that started it came back up. Nothing pressed in another window can start
//! anything, because nothing pressed is read at all.
//!
//! # Binding a stroke to the key that went down
//!
//! Devices report PHYSICAL keys; the terminal reports characters AFTER the keyboard layout. Under
//! Colemak the Backspace character comes from the Caps Lock key; under xmodmap or kanata, from
//! anything. So a stroke is not bound to `KEY_BACKSPACE` — it is bound to whichever key went down
//! between the previous terminal event and this one, on whichever device showed it. The canonical
//! key is preferred when it is down, since that is the overwhelmingly common case; the difference
//! is the fallback that makes a remapped key work instead of silently never holding.
//!
//! That same comparison is what closes the race that would otherwise be this design's worst
//! failure. A fast tap's physical release reaches the device BEFORE the terminal's byte for the
//! press has travelled through the terminal emulator (and tmux, and ssh). By the time the press is
//! seen, nothing is down — so it is a tap, which is exactly what it was. No stroke opens that a
//! release will never close.
//!
//! # Permission
//!
//! `/dev/input/event*` is normally readable by root and the `input` group only. Membership of that
//! group lets a program observe every key pressed on the machine, password prompts included — this
//! module declines to, but the group grants it regardless. Without access, the picker simply has no
//! release source here and hold keys tap.

use std::fs::File;
use std::path::PathBuf;

/// Size of the kernel's key bitmap: `KEY_MAX / 8 + 1` with `KEY_MAX = 767`
/// (`linux/input-event-codes.h`), so every key code has a bit.
pub(crate) const KEY_BITMAP_BYTES: usize = 96;

/// Which keys are down, one bit per key code.
pub(crate) type KeyBits = [u8; KEY_BITMAP_BYTES];

/// Key codes from `linux/input-event-codes.h`, for the keys this module has an opinion about.
pub(crate) mod code {
    pub const BACKSPACE: u16 = 14;
    pub const ENTER: u16 = 28;
    pub const SPACE: u16 = 57;
    pub const KEYPAD_ENTER: u16 = 96;
    /// Held alongside a pen key — or pressed by the terminal on the way to a character — and never
    /// the key that started a stroke.
    pub const MODIFIERS: [u16; 8] = [29, 42, 54, 56, 97, 100, 125, 126];
    /// The keys a person holds WHILE a pen key is down, and so never the one that started it:
    /// the arrows, Home, End, Page Up and Page Down.
    pub const NAVIGATION: [u16; 8] = [102, 103, 104, 105, 106, 107, 108, 109];
    /// Codes from here up are mouse, joystick and touch buttons, not keyboard keys.
    pub const FIRST_BUTTON: u16 = 256;
}

/// `_IOC(_IOC_READ, 'E', nr, size)`, the ioctl request number for reading `size` bytes.
///
/// The packing is ARCHITECTURE-SPECIFIC, and libc keeps its own copy of these constants private
/// (`unix/linux_like/mod.rs`), so it is replicated here. powerpc, sparc and mips give the size 13
/// bits and the direction 3; everything using `asm-generic/ioctl.h` — x86, ARM, RISC-V — gives 14
/// and 2. `_IOC_READ` happens to be 2 on both, which is exactly the coincidence that would make a
/// hard-coded x86 number look right and be wrong elsewhere. The test pins the x86 values against
/// numbers printed from the real kernel headers.
const fn ioc_read(nr: u32, size: u32) -> u32 {
    #[cfg(any(
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ))]
    const SIZE_BITS: u32 = 13;
    #[cfg(not(any(
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    )))]
    const SIZE_BITS: u32 = 14;
    const READ: u32 = 2;
    (READ << (16 + SIZE_BITS)) | (size << 16) | ((b'E' as u32) << 8) | nr
}

/// `EVIOCGKEY(len)`: the current state of every key.
const EVIOCGKEY: u32 = ioc_read(0x18, KEY_BITMAP_BYTES as u32);
/// `EVIOCGBIT(EV_KEY, len)`: which keys the device HAS — how a keyboard is told from a mouse.
const EVIOCGBIT_KEY: u32 = ioc_read(0x20 + 1, KEY_BITMAP_BYTES as u32);

/// Whether `code` is set in `bits`.
pub(crate) fn is_down(bits: &KeyBits, code: u16) -> bool {
    let at = code as usize;
    at < KEY_BITMAP_BYTES * 8 && bits[at / 8] & (1 << (at % 8)) != 0
}

/// Something that can say which keys are down on each keyboard — the ONE seam between this module
/// and the kernel. [`Keyboards`] is the real one; tests supply their own, so everything above the
/// ioctl runs here, where there is no `/dev/input` to open.
pub(crate) trait KeyState {
    fn snapshot(&self) -> Snapshot;
}

impl KeyState for Keyboards {
    fn snapshot(&self) -> Snapshot {
        Keyboards::snapshot(self)
    }
}

/// Every keyboard this process can open, keyed by path so a snapshot survives hot-plugging.
#[derive(Debug)]
pub(crate) struct Keyboards {
    devices: Vec<(PathBuf, File)>,
}

/// The keyboards, opened — by a process that could, and that may since have given up the right
/// to: see [`crate::elevate`].
///
/// Opaque on purpose. All a holder can do is hand them to [`crate::ui::run_with_devices`]; what
/// is asked of them, and what is never read, is this module's business alone.
#[derive(Debug)]
pub struct InputDevices(pub(crate) Keyboards);

impl InputDevices {
    /// Open every keyboard this process can read. `None` when it can read none — no permission,
    /// no devices, not Linux — which is the ordinary case and not an error.
    pub fn open() -> Option<Self> {
        Keyboards::open().map(Self)
    }
}

impl Keyboards {
    /// Open whatever keyboards are readable. `None` when none are — no permission, no devices, or
    /// not Linux — which is the ordinary case and not an error.
    pub(crate) fn open() -> Option<Self> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/dev/input").ok()?.flatten() {
            let path = entry.path();
            let is_event =
                path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event"));
            if !is_event {
                continue;
            }
            let Ok(file) = File::open(&path) else { continue };
            if query(&file, EVIOCGBIT_KEY).is_some_and(|keys| is_down(&keys, code::SPACE)) {
                found.push((path, file));
            }
        }
        (!found.is_empty()).then_some(Self { devices: found })
    }

    /// Which keys are down on each keyboard, right now.
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.devices
            .iter()
            .filter_map(|(path, file)| Some((path.clone(), query(file, EVIOCGKEY)?)))
            .collect()
    }
}

/// One `EVIOCG*` bitmap query. `None` if the device refused or went away.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn query(file: &File, request: u32) -> Option<KeyBits> {
    use std::os::fd::AsRawFd;
    let mut bits: KeyBits = [0; KEY_BITMAP_BYTES];
    // SAFETY: the request is a read of exactly `KEY_BITMAP_BYTES` into `bits`, which is that many
    // bytes and lives on this frame; the fd is open for as long as `file` is borrowed. The request
    // type is `libc::Ioctl` — `c_ulong` on glibc, `c_int` on musl — hence the conversion.
    let ok = unsafe { libc::ioctl(file.as_raw_fd(), request as libc::Ioctl, bits.as_mut_ptr()) };
    (ok >= 0).then_some(bits)
}

/// Off Linux there is nothing to ask: the request numbers above are the Linux kernel's, and libc
/// names the ioctl request type only for Linux-like targets. FreeBSD's evdev answers the same
/// question under different numbers, and is left unwired because nothing here can test it.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn query(_: &File, _: u32) -> Option<KeyBits> {
    None
}

/// Each device's key bitmap at one moment.
pub(crate) type Snapshot = Vec<(PathBuf, KeyBits)>;

/// Which physical keys, on which device, are holding one terminal key down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    device: PathBuf,
    keys: Vec<u16>,
}

/// A terminal key, as the picker knows it.
type TerminalKey = crate::keys::KeyCode;

/// The correlation between terminal key events and physical key state — pure, over snapshots,
/// so every case the kernel could present is a test.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    /// The snapshot taken at the previous terminal key event: what was already down.
    previous: Snapshot,
    /// What holds each terminal key that is holding the pen — more than one when, say, Space is
    /// still down while Backspace is pressed on top of it.
    bound: Vec<(TerminalKey, Binding)>,
    /// Hold presses in a row that no physical key accounted for, and whether that has been said.
    unmatched_in_a_row: u32,
    ever_matched: bool,
    warned: bool,
}

/// What a hold-key press turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Press {
    /// A physical key is down behind it: the stroke holds until that key comes up.
    Held,
    /// Nothing new is down — a tap that already finished, or a keyboard these devices cannot see.
    Tap,
    /// The key bound to the stroke is still down: the terminal's auto-repeat, not a new press.
    Repeat,
}

/// After this many hold presses in a row with no physical key behind any of them, and none ever
/// matched, the devices are probably not the keyboard being typed on — say so once. Three, because
/// a held key auto-repeats many times a second, so a real hold produces this quickly, while a
/// person tapping deliberately rarely taps the same key three times with nothing between.
pub(crate) const UNMATCHED_BEFORE_WARNING: u32 = 3;

impl Tracker {
    /// Whether any terminal key is being held by a device key.
    pub(crate) fn is_holding(&self) -> bool {
        !self.bound.is_empty()
    }

    /// Any terminal key event: remember what is down, for the next press to be compared against.
    /// A key that is not a hold key also breaks a run of unmatched presses.
    pub(crate) fn observe(&mut self, now: Snapshot) {
        self.previous = now;
        self.unmatched_in_a_row = 0;
    }

    /// A hold-key press of `key` arrived from the terminal. `canonical` is the physical key that
    /// makes it on a standard layout, preferred when it is down; otherwise whatever went down
    /// since the last terminal event is taken to be the key. A physical key already holding some
    /// OTHER terminal key is never taken for this one.
    pub(crate) fn press(&mut self, now: Snapshot, canonical: &[u16], key: TerminalKey) -> Press {
        if let Some(at) = self.bound.iter().position(|(held, _)| *held == key) {
            if still_down(&self.bound[at].1, &now) {
                self.previous = now;
                return Press::Repeat;
            }
            self.bound.remove(at);
        }
        let taken: Vec<u16> = self.bound.iter().flat_map(|(_, b)| b.keys.iter().copied()).collect();
        let binding = bind(&self.previous, &now, canonical, &taken);
        self.previous = now;
        match binding {
            Some(binding) => {
                self.bound.push((key, binding));
                self.ever_matched = true;
                self.unmatched_in_a_row = 0;
                Press::Held
            }
            None => {
                self.unmatched_in_a_row += 1;
                Press::Tap
            }
        }
    }

    /// The terminal keys whose physical keys have come up since the last look — and which are
    /// therefore no longer held.
    pub(crate) fn released(&mut self, now: &Snapshot) -> Vec<TerminalKey> {
        let (up, down): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.bound).into_iter().partition(|(_, b)| !still_down(b, now));
        self.bound = down;
        up.into_iter().map(|(key, _)| key).collect()
    }

    /// Forget every binding but those of `keys` — the picker let the others go some other way (a
    /// toggle took over, the cursor left the canvas, an undo).
    pub(crate) fn keep_only(&mut self, keys: &[TerminalKey]) {
        self.bound.retain(|(key, _)| keys.contains(key));
    }

    /// Whether to say, once, that the devices seem not to be this keyboard.
    pub(crate) fn should_warn(&mut self) -> bool {
        let warn = !self.warned
            && !self.ever_matched
            && self.unmatched_in_a_row >= UNMATCHED_BEFORE_WARNING;
        self.warned |= warn;
        warn
    }
}

/// The key behind a press: the canonical key if it is down anywhere, otherwise the keys newly down
/// on exactly one device. Modifiers and navigation keys never count — they are what a person holds
/// alongside a pen key, not the pen key — and nor do keys `taken` by another held key.
fn bind(previous: &Snapshot, now: &Snapshot, canonical: &[u16], taken: &[u16]) -> Option<Binding> {
    for (device, bits) in now {
        if let Some(&key) =
            canonical.iter().find(|&&key| is_down(bits, key) && !taken.contains(&key))
        {
            return Some(Binding { device: device.clone(), keys: vec![key] });
        }
    }
    let mut candidates = now.iter().filter_map(|(device, bits)| {
        let before = previous.iter().find(|(d, _)| d == device).map(|(_, b)| b);
        let fresh: Vec<u16> = (0..code::FIRST_BUTTON)
            .filter(|&key| is_down(bits, key) && !before.is_some_and(|b| is_down(b, key)))
            .filter(|key| !code::MODIFIERS.contains(key) && !code::NAVIGATION.contains(key))
            .filter(|key| !taken.contains(key))
            .collect();
        (!fresh.is_empty()).then(|| Binding { device: device.clone(), keys: fresh })
    });
    let first = candidates.next()?;
    // Two devices both showing a fresh key is ambiguous; guessing would bind a stroke to a key the
    // person may let go of first. A tap is the honest answer.
    candidates.next().is_none().then_some(first)
}

/// Whether any key of the binding is still down on its device. A device that has vanished holds
/// nothing, so unplugging the keyboard ends the stroke.
fn still_down(binding: &Binding, now: &Snapshot) -> bool {
    now.iter()
        .find(|(device, _)| *device == binding.device)
        .is_some_and(|(_, bits)| binding.keys.iter().any(|&key| is_down(bits, key)))
}

/// How often a held stroke asks the device whether its key is still down. Fast enough that a
/// release is noticed within a frame at 60 Hz, and only while a stroke is actually held: an idle
/// picker still blocks with no timeout and costs nothing.
pub(crate) const HOLD_POLL_MS: i32 = 16;

/// The glue between terminal key events and physical key state, for [`crate::ui::run`].
///
/// It turns what the devices know into what [`crate::ui::apply`] already understands — a release
/// event — so `apply` never learns that devices exist. A hold-key press the devices can account
/// for is passed through and binds; one they cannot is passed through AND followed at once by its
/// own release, which makes it a tap; one that is the terminal's auto-repeat of a bound key
/// becomes a repeat, which `apply` ignores for pen keys. Each held key is bound on its own, so
/// with Space and Backspace both down, each one's release is reported when it comes.
pub(crate) struct Holds<K: KeyState = Keyboards> {
    keyboards: K,
    tracker: Tracker,
}

impl Holds<Keyboards> {
    pub(crate) fn open() -> Option<Self> {
        Some(Self::watching(Keyboards::open()?))
    }
}

impl<K: KeyState> Holds<K> {
    pub(crate) fn watching(keyboards: K) -> Self {
        let mut tracker = Tracker::default();
        tracker.observe(keyboards.snapshot());
        Self { keyboards, tracker }
    }

    /// Whether the run loop should wake on a timer to watch for a release.
    pub(crate) fn is_holding(&self) -> bool {
        self.tracker.is_holding()
    }

    /// Route one terminal event. `would_hold` says whether this press would hold a pen right now
    /// — on a cell, not while naming — since only then is there a stroke to bind.
    ///
    /// Returns the event to apply, possibly relabelled, and a release to apply straight after it
    /// when the press turned out to be a tap. Also returns whether to warn that these devices do
    /// not seem to be the keyboard being typed on.
    pub(crate) fn route(
        &mut self,
        event: crate::keys::KeyEvent,
        would_hold: bool,
    ) -> (crate::keys::KeyEvent, Option<crate::keys::KeyEvent>, bool) {
        use crate::keys::{KeyCode, KeyEvent, KeyKind};
        let canonical: &[u16] = match event.code {
            KeyCode::Char(' ') => &[code::SPACE],
            KeyCode::Enter => &[code::ENTER, code::KEYPAD_ENTER],
            KeyCode::Backspace => &[code::BACKSPACE],
            _ => &[],
        };
        let plain = !event.mods.ctrl && !event.mods.alt;
        if canonical.is_empty() || !plain || event.kind != KeyKind::Press || !would_hold {
            self.tracker.observe(self.keyboards.snapshot());
            return (event, None, false);
        }
        match self.tracker.press(self.keyboards.snapshot(), canonical, event.code) {
            Press::Held => (event, None, false),
            Press::Repeat => (KeyEvent { kind: KeyKind::Repeat, ..event }, None, false),
            Press::Tap => {
                let warn = self.tracker.should_warn();
                (event, Some(KeyEvent::release(event.code)), warn)
            }
        }
    }

    /// The releases of every bound key that has come up since the last look.
    pub(crate) fn poll_release(&mut self) -> Vec<crate::keys::KeyEvent> {
        if !self.tracker.is_holding() {
            return Vec::new();
        }
        let now = self.keyboards.snapshot();
        self.tracker.released(&now).into_iter().map(crate::keys::KeyEvent::release).collect()
    }

    /// Only `held` are still holding the pen: the picker let go of the rest some other way — a
    /// toggle took over, the cursor left the canvas, an undo — so stop watching for releases
    /// nobody is waiting on.
    pub(crate) fn sync(&mut self, held: &[crate::keys::KeyCode]) {
        self.tracker.keep_only(held);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{KeyCode, KeyEvent, KeyKind};

    fn bits(down: &[u16]) -> KeyBits {
        let mut b = [0u8; KEY_BITMAP_BYTES];
        for &key in down {
            b[key as usize / 8] |= 1 << (key % 8);
        }
        b
    }

    fn at(device: &str, down: &[u16]) -> (PathBuf, KeyBits) {
        (PathBuf::from(device), bits(down))
    }

    const SPACE: &[u16] = &[code::SPACE];
    const BACKSPACE: &[u16] = &[code::BACKSPACE];
    const CAPS_LOCK: u16 = 58;
    const RIGHT: u16 = 106;
    const LEFT_SHIFT: u16 = 42;
    /// The terminal keys the tests hold.
    const SPACE_KEY: KeyCode = KeyCode::Char(' ');
    const BACKSPACE_KEY: KeyCode = KeyCode::Backspace;

    /// Printed from the real headers (`linux/input.h`, x86_64): if the packing were wrong, this
    /// fails here instead of every ioctl failing with EINVAL on a user's machine.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(EVIOCGKEY, 0x8060_4518, "EVIOCGKEY(96)");
        assert_eq!(EVIOCGBIT_KEY, 0x8060_4521, "EVIOCGBIT(EV_KEY, 96)");
        assert_eq!(ioc_read(0x20, 4), 0x8004_4520, "EVIOCGBIT(0, 4)");
    }

    #[test]
    fn a_bit_is_down_only_where_it_was_set() {
        let b = bits(&[code::SPACE, 0, 255]);
        assert!(is_down(&b, code::SPACE) && is_down(&b, 0) && is_down(&b, 255));
        assert!(!is_down(&b, code::ENTER));
        assert!(!is_down(&b, 9999), "out of range is up, not a panic");
    }

    #[test]
    fn a_held_space_binds_and_its_release_ends_the_stroke() {
        let mut t = Tracker::default();
        assert_eq!(t.press(vec![at("kbd", &[code::SPACE])], SPACE, SPACE_KEY), Press::Held);
        assert!(t.is_holding());
        assert!(
            t.released(&vec![at("kbd", &[code::SPACE, RIGHT])]).is_empty(),
            "an arrow alongside"
        );
        assert_eq!(
            t.released(&vec![at("kbd", &[RIGHT])]),
            [SPACE_KEY],
            "space up, arrow down: over"
        );
        assert!(!t.is_holding());
    }

    /// THE RACE. A fast tap's release reaches the device before the terminal's press byte does, so
    /// at the press nothing is down. That must be a tap — never a stroke waiting on a release that
    /// has already happened.
    #[test]
    fn a_tap_already_released_by_the_time_its_byte_arrives_is_a_tap() {
        let mut t = Tracker::default();
        t.observe(vec![at("kbd", &[])]);
        assert_eq!(t.press(vec![at("kbd", &[])], SPACE, SPACE_KEY), Press::Tap);
        assert!(!t.is_holding(), "nothing is left waiting for a release");
    }

    /// The terminal's auto-repeat of a held key is not a new press.
    #[test]
    fn the_terminals_auto_repeat_is_a_repeat_while_the_key_is_down() {
        let mut t = Tracker::default();
        t.press(vec![at("kbd", &[code::SPACE])], SPACE, SPACE_KEY);
        assert_eq!(t.press(vec![at("kbd", &[code::SPACE])], SPACE, SPACE_KEY), Press::Repeat);
        assert!(t.is_holding());
    }

    /// Colemak puts Backspace on Caps Lock. Asking only for KEY_BACKSPACE would never see it held;
    /// binding to what went down does.
    #[test]
    fn a_remapped_key_binds_to_the_physical_key_that_went_down() {
        let mut t = Tracker::default();
        t.observe(vec![at("kbd", &[])]);
        assert_eq!(t.press(vec![at("kbd", &[CAPS_LOCK])], BACKSPACE, BACKSPACE_KEY), Press::Held);
        assert!(t.released(&vec![at("kbd", &[CAPS_LOCK])]).is_empty());
        assert_eq!(t.released(&vec![at("kbd", &[])]), [BACKSPACE_KEY], "caps lock up: over");
    }

    /// Keys already down before this press — an arrow being held, Shift — are not the pen key.
    #[test]
    fn keys_already_down_and_modifiers_are_never_taken_for_the_pen_key() {
        let mut t = Tracker::default();
        t.observe(vec![at("kbd", &[RIGHT])]);
        let now = vec![at("kbd", &[RIGHT, LEFT_SHIFT, CAPS_LOCK])];
        assert_eq!(t.press(now, BACKSPACE, BACKSPACE_KEY), Press::Held);
        assert!(t.released(&vec![at("kbd", &[CAPS_LOCK])]).is_empty(), "right let go: still held");
        assert_eq!(
            t.released(&vec![at("kbd", &[RIGHT, LEFT_SHIFT])]),
            [BACKSPACE_KEY],
            "caps lock let go: over"
        );
    }

    /// With a laptop keyboard and an external one, the OTHER keyboard's space must not lift a pen
    /// held on this one.
    #[test]
    fn the_release_is_taken_only_from_the_device_that_showed_the_press() {
        let mut t = Tracker::default();
        t.press(vec![at("laptop", &[]), at("usb", &[code::SPACE])], SPACE, SPACE_KEY);
        assert!(t
            .released(&vec![at("laptop", &[code::SPACE]), at("usb", &[code::SPACE])])
            .is_empty());
        assert!(
            t.released(&vec![at("laptop", &[]), at("usb", &[code::SPACE])]).is_empty(),
            "laptop's space up"
        );
        assert_eq!(
            t.released(&vec![at("laptop", &[code::SPACE]), at("usb", &[])]),
            [SPACE_KEY],
            "usb's up: over"
        );
    }

    #[test]
    fn unplugging_the_keyboard_ends_the_stroke() {
        let mut t = Tracker::default();
        t.press(vec![at("usb", &[code::SPACE])], SPACE, SPACE_KEY);
        assert_eq!(t.released(&vec![]), [SPACE_KEY], "a device that is gone holds nothing");
    }

    /// Two devices each showing a fresh non-canonical key is ambiguous: guess wrong and the stroke
    /// ends on the wrong key's release. A tap is the honest answer.
    #[test]
    fn a_fresh_key_on_two_devices_at_once_is_a_tap_rather_than_a_guess() {
        let mut t = Tracker::default();
        t.observe(vec![at("a", &[]), at("b", &[])]);
        assert_eq!(
            t.press(vec![at("a", &[CAPS_LOCK]), at("b", &[CAPS_LOCK])], BACKSPACE, BACKSPACE_KEY),
            Press::Tap
        );
    }

    /// The silent case: devices are readable but are not the keyboard being typed on (ssh into a
    /// desktop, a container). Every hold becomes a tap and would say nothing — so after a run of
    /// unmatched presses, say it once.
    #[test]
    fn devices_that_never_match_are_reported_once() {
        let mut t = Tracker::default();
        for _ in 0..UNMATCHED_BEFORE_WARNING - 1 {
            t.press(vec![at("other", &[])], SPACE, SPACE_KEY);
            assert!(!t.should_warn());
        }
        t.press(vec![at("other", &[])], SPACE, SPACE_KEY);
        assert!(t.should_warn(), "enough in a row");
        t.press(vec![at("other", &[])], SPACE, SPACE_KEY);
        assert!(!t.should_warn(), "and only once");
    }

    /// Keyboards whose state a test sets by hand.
    struct Fake(std::cell::RefCell<Snapshot>);

    impl KeyState for Fake {
        fn snapshot(&self) -> Snapshot {
            self.0.borrow().clone()
        }
    }

    fn fake(down: &[u16]) -> Fake {
        Fake(std::cell::RefCell::new(vec![at("kbd", down)]))
    }

    fn set(holds: &Holds<Fake>, down: &[u16]) {
        *holds.keyboards.0.borrow_mut() = vec![at("kbd", down)];
    }

    #[test]
    fn a_press_the_device_can_see_passes_through_and_its_release_is_reported() {
        let mut holds = Holds::watching(fake(&[]));
        set(&holds, &[code::SPACE]);
        let (event, then, _) = holds.route(KeyEvent::press(KeyCode::Char(' ')), true);
        assert_eq!((event.kind, then), (KeyKind::Press, None));
        assert!(holds.is_holding());
        assert!(holds.poll_release().is_empty(), "still down");
        set(&holds, &[]);
        assert_eq!(holds.poll_release(), [KeyEvent::release(KeyCode::Char(' '))]);
        assert!(!holds.is_holding());
    }

    #[test]
    fn a_press_the_device_cannot_see_is_followed_by_its_own_release() {
        let mut holds = Holds::watching(fake(&[]));
        let (event, then, _) = holds.route(KeyEvent::press(KeyCode::Backspace), true);
        assert_eq!(event.kind, KeyKind::Press);
        assert_eq!(then, Some(KeyEvent::release(KeyCode::Backspace)), "a tap");
        assert!(!holds.is_holding());
    }

    #[test]
    fn the_terminals_auto_repeat_of_a_held_key_is_relabelled_a_repeat() {
        let mut holds = Holds::watching(fake(&[]));
        set(&holds, &[code::SPACE]);
        holds.route(KeyEvent::press(KeyCode::Char(' ')), true);
        let (again, then, _) = holds.route(KeyEvent::press(KeyCode::Char(' ')), true);
        assert_eq!((again.kind, then), (KeyKind::Repeat, None));
    }

    /// Only a press that would hold a pen binds: Space on a swatch picks a brush, and Space while
    /// naming types. Neither is a stroke.
    #[test]
    fn a_press_that_would_not_hold_anything_binds_nothing() {
        let mut holds = Holds::watching(fake(&[]));
        set(&holds, &[code::SPACE]);
        let (event, then, _) = holds.route(KeyEvent::press(KeyCode::Char(' ')), false);
        assert_eq!((event.kind, then), (KeyKind::Press, None));
        assert!(!holds.is_holding());
        let (other, then, _) = holds.route(KeyEvent::press(KeyCode::Right), true);
        assert_eq!((other.code, then), (KeyCode::Right, None), "not a hold key");
    }

    #[test]
    fn a_stroke_the_picker_ended_itself_is_no_longer_watched() {
        let mut holds = Holds::watching(fake(&[]));
        set(&holds, &[code::SPACE]);
        holds.route(KeyEvent::press(KeyCode::Char(' ')), true);
        holds.sync(&[]);
        assert!(!holds.is_holding());
        set(&holds, &[]);
        assert!(holds.poll_release().is_empty(), "no release for a stroke that is already over");
    }

    #[test]
    fn one_match_ever_means_no_warning_and_other_keys_break_a_run() {
        let mut t = Tracker::default();
        t.press(vec![at("kbd", &[code::SPACE])], SPACE, SPACE_KEY);
        t.released(&vec![at("kbd", &[])]);
        for _ in 0..10 {
            t.press(vec![at("kbd", &[])], SPACE, SPACE_KEY);
        }
        assert!(!t.should_warn(), "this keyboard has been seen to work");

        let mut u = Tracker::default();
        for _ in 0..10 {
            u.press(vec![at("other", &[])], SPACE, SPACE_KEY);
            u.observe(vec![at("other", &[])]); // another key between each
        }
        assert!(!u.should_warn(), "taps separated by other keys are just taps");
    }

    /// Space held, then Backspace pressed on top of it: two stroke keys, each bound to its own
    /// physical key, and each one's release reported when — and only when — it comes.
    #[test]
    fn two_held_keys_are_bound_apart_and_released_apart() {
        let mut t = Tracker::default();
        t.observe(vec![at("kbd", &[])]);
        assert_eq!(t.press(vec![at("kbd", &[code::SPACE])], SPACE, SPACE_KEY), Press::Held);
        let both = vec![at("kbd", &[code::SPACE, code::BACKSPACE])];
        assert_eq!(t.press(both.clone(), BACKSPACE, BACKSPACE_KEY), Press::Held);
        assert_eq!(t.press(both.clone(), BACKSPACE, BACKSPACE_KEY), Press::Repeat, "its repeat");
        assert!(t.released(&both).is_empty(), "both still down");
        assert_eq!(t.released(&vec![at("kbd", &[code::SPACE])]), [BACKSPACE_KEY], "backspace up");
        assert!(t.is_holding(), "space still holds");
        assert_eq!(t.released(&vec![at("kbd", &[])]), [SPACE_KEY], "then space");
        assert!(!t.is_holding());
    }

    /// A physical key already holding one terminal key is never taken for another: with Space
    /// bound, a remapped Backspace binds to the key that actually went down for it.
    #[test]
    fn a_key_already_holding_one_stroke_key_is_not_taken_for_the_next() {
        let mut t = Tracker::default();
        t.press(vec![at("kbd", &[code::SPACE])], SPACE, SPACE_KEY);
        // Space is down but was not in the snapshot before this press — the case where only the
        // exclusion keeps it from being read as the new key.
        t.previous = vec![at("kbd", &[])];
        let now = vec![at("kbd", &[code::SPACE, CAPS_LOCK])];
        assert_eq!(t.press(now, BACKSPACE, BACKSPACE_KEY), Press::Held);
        assert_eq!(t.released(&vec![at("kbd", &[code::SPACE])]), [BACKSPACE_KEY], "caps lock up");
    }

    #[test]
    fn syncing_forgets_only_the_keys_the_picker_let_go() {
        let mut holds = Holds::watching(fake(&[]));
        set(&holds, &[code::SPACE]);
        holds.route(KeyEvent::press(KeyCode::Char(' ')), true);
        set(&holds, &[code::SPACE, code::BACKSPACE]);
        holds.route(KeyEvent::press(KeyCode::Backspace), true);
        holds.sync(&[KeyCode::Char(' ')]);
        set(&holds, &[]);
        assert_eq!(holds.poll_release(), [KeyEvent::release(KeyCode::Char(' '))], "only space's");
    }
}
