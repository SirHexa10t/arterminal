//! `--su`: sudo for one run, and root for one moment.
//!
//! Held keys need someone to say when a key comes UP. A terminal speaking the kitty keyboard
//! protocol does; everywhere else the input devices under `/dev/input` can (see
//! `crate::devices`) — but only for root and the `input` group. Joining that group is the
//! standing fix, and it lets every program the user runs read every key on the machine. This is
//! the narrow alternative: ask sudo for THIS run, open the keyboards, and give root back before
//! anything else happens. Modelled on the `elevate.rs` of the developer's `sequencer`.
//!
//! Four outcomes, decided in this order:
//!
//! 1. **Already root on sudo's behalf** — this is the re-run below, or somebody's
//!    `sudo arterminal --su`. Open the keyboards, drop to the user sudo names, verify the drop,
//!    go ahead. Never asks again, whatever the devices said: `/dev/input` can be missing even
//!    for root (a container, a VM), and re-asking would loop on the password prompt forever.
//! 2. **No need** — the keyboards opened as things stand (the `input` group, a root login), or
//!    there is nowhere to find any (not Linux). Go ahead; sudo is never mentioned.
//! 3. **Need, but nobody to ask** — stdin is not a terminal. Go ahead without: a pipeline cannot
//!    answer a password prompt, and must never block on one.
//! 4. **Need, and a terminal** — explain, then run this same binary again under sudo and wait
//!    for it. That child is case 1; the whole session happens there.
//!
//! # Hard ordering constraints
//!
//! These are properties of WHERE [`for_this_run`] is called, so they are written down here
//! rather than left as the order the code happens to have:
//!
//! * **Before any terminal state changes.** The password prompt must meet a terminal in its
//!   ordinary state — echo on, no keyboard protocol pushed — and the elevated child must be
//!   unprivileged before raw mode or anything else the picker does. Call it first thing in
//!   `main`. The tempting later "improvement", asking lazily on the first held key, would put a
//!   password prompt into a raw terminal and is exactly wrong.
//! * **Nothing else while root.** Every descriptor opened before the drop survives it. That is
//!   the whole point for the keyboards and the whole hazard for anything else: the art file is
//!   opened only after this returns, or `--su /etc/shadow` would be a root file viewer and
//!   writer. Even a `stat` of its path would leak whether a root-only file exists. So the root
//!   half of this module does one piece of I/O of its own — [`InputDevices::open`] — and then
//!   drops. The drop consults the user and group databases, as every drop must, and opens no
//!   path anyone running it chose.
//!
//! # The drop
//!
//! Supplementary groups, then the group, then the user, because each step gives up the right
//! to take the next: `initgroups`, `setresgid`, `setresuid`. The `setres*` forms set real,
//! effective and saved ids all at once, where `setuid`'s doing so is conditional on still being
//! root — the very property being destroyed. Every return is checked, and a failure is fatal:
//! continuing as root would break the promise made at the password prompt.
//!
//! Then it is VERIFIED rather than trusted, because a `setuid(0)` that fails proves only that
//! the user ids are gone: the group ids must all be the user's, `setgid(0)` must fail too, and
//! the supplementary groups must be exactly the user's own — root's `0` left in that list would
//! pass every uid check while the process stayed privileged.
//!
//! The groups are the user's own, not sequencer's "just `input`": sequencer keeps `input` so it
//! can open keyboards plugged in mid-session, and this does not hot-plug, so keeping it would
//! only widen the user for nothing.

use crate::devices::InputDevices;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The first argument of `--su`'s own re-run under sudo: how that child knows it IS one. The
/// command line is the one channel sudo leaves alone — its environment reset may delete
/// anything, `SUDO_UID` included, and a re-run that cannot tell it is one would go on as root.
///
/// A caller that parses its own command line must skip it: see [`without_rerun_mark`].
pub const RERUN_MARK: &str = "--arterminal-su-rerun";

/// `args` without [`RERUN_MARK`], which only ever leads.
pub fn without_rerun_mark(args: &[String]) -> &[String] {
    match args.split_first() {
        Some((first, rest)) if first == RERUN_MARK => rest,
        _ => args,
    }
}

/// What `--su` came to.
#[derive(Debug)]
pub enum Elevation {
    /// Go ahead in this process — with the keyboards, if any opened — and tell the user `note`
    /// first, if there is something worth saying: why holding will not work here, say.
    Proceed { devices: Option<InputDevices>, note: Option<String> },
    /// The whole session ran in a child under sudo. Exit with this code.
    Finished(i32),
}

/// Act on `--su` for this run. Call it FIRST — see the module docs for why that is a hard rule.
///
/// # Errors
///
/// When this process is root on sudo's behalf and cannot become the user again, or cannot prove
/// that it did. The caller must stop: going on would run the session as root.
pub fn for_this_run() -> io::Result<Elevation> {
    // As root, this is the one piece of I/O done before the drop. As the user, the same call is
    // the test of whether sudo is needed at all.
    let devices = InputDevices::open();
    let situation = Situation {
        root: is_root(),
        under_sudo: std::env::var_os("SUDO_UID").is_some(),
        rerun: std::env::args_os().nth(1).as_deref() == Some(OsStr::new(RERUN_MARK)),
        devices_readable: devices.is_some(),
        interactive: interactive(),
        linux: cfg!(target_os = "linux"),
    };
    match plan(situation) {
        Plan::DropThenProceed => {
            let caller = Caller::from_sudo_env(
                std::env::var("SUDO_UID").ok().as_deref(),
                std::env::var("SUDO_GID").ok().as_deref(),
            )?;
            drop_root(&caller)?;
            let note = devices.is_none().then(|| {
                "--su: even root cannot open a keyboard under /dev/input here, so held keys tap; \
                 b and del still drag"
                    .to_string()
            });
            Ok(Elevation::Proceed { devices, note })
        }
        Plan::Proceed => Ok(Elevation::Proceed { devices, note: note_for(situation) }),
        Plan::RerunUnderSudo => Ok(Elevation::Finished(rerun_under_sudo())),
        Plan::Refuse => Err(refuse(
            "this is --su's own re-run, but sudo did not say who ran it (SUDO_UID is unset)"
                .to_string(),
        )),
    }
}

/// What this process is, as far as `--su` is concerned — gathered once, so the decision is a
/// pure function of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Situation {
    /// The effective user is root.
    root: bool,
    /// sudo started this process: `SUDO_UID` is set.
    under_sudo: bool,
    /// This is `--su`'s own re-run: the command line leads with [`RERUN_MARK`].
    rerun: bool,
    /// The keyboards opened as things stand.
    devices_readable: bool,
    /// Stdin is a terminal, so a password can be asked for.
    interactive: bool,
    /// Linux, where the input devices this is all for exist at all.
    linux: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Go ahead as things stand.
    Proceed,
    /// Root on sudo's behalf: keep the keyboards, give root back, then go ahead.
    DropThenProceed,
    /// Ask sudo to run this again.
    RerunUnderSudo,
    /// Stop: root, and nobody to become.
    Refuse,
}

/// The decision, in the order the module docs give it.
fn plan(situation: Situation) -> Plan {
    match situation {
        Situation { root: true, under_sudo: true, .. } => Plan::DropThenProceed,
        // Our own re-run, root, with nobody named to become — sudo was set up to delete
        // `SUDO_UID`, or something stands between. Going on would run the session as root, and
        // silently, since root can read the devices: the one way this module could fail open.
        Situation { root: true, rerun: true, .. } => Plan::Refuse,
        // THE LOOP GUARD. Started by sudo or by our own re-run without being root, or root with
        // nobody to drop to (a root login): never ask again, whatever the devices said.
        Situation { under_sudo: true, .. }
        | Situation { rerun: true, .. }
        | Situation { root: true, .. } => Plan::Proceed,
        Situation { devices_readable: true, .. } => Plan::Proceed,
        Situation { linux: false, .. } | Situation { interactive: false, .. } => Plan::Proceed,
        _ => Plan::RerunUnderSudo,
    }
}

/// What to tell the user when `--su` goes ahead without asking.
fn note_for(situation: Situation) -> Option<String> {
    match situation {
        Situation { devices_readable: true, .. } => None,
        Situation { linux: false, .. } => {
            Some("--su opens Linux input devices, and there are none here".to_string())
        }
        Situation { root: false, under_sudo: false, rerun: false, interactive: false, .. } => Some(
            "--su needs a terminal to ask for a password; going ahead without held keys"
                .to_string(),
        ),
        _ => {
            Some("--su: no keyboard under /dev/input could be opened, so held keys tap".to_string())
        }
    }
}

/// What the user reads before sudo asks for a password: what it is for, exactly how long root
/// lasts, and the standing alternative with its own cost named.
fn session_prompt() -> &'static str {
    "arterminal: --su: sudo opens the keyboards under /dev/input for THIS RUN only, so held keys\n\
     work in any terminal. Root is dropped as soon as they are open, before the art file is even\n\
     read, and nothing persists after exit. To stop being asked, join the input group (see\n\
     --help) — which lets every program you run read every key typed on the machine.\n"
}

/// Run this binary again under sudo, with the same arguments, and hand back its exit code.
fn rerun_under_sudo() -> i32 {
    // The binary that is running, by the kernel's account — never argv[0], which whoever started
    // us controls: a planted or PATH-relative argv[0] would have the user type their password
    // for some other program.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("arterminal: --su: cannot find this binary to run it under sudo: {err}");
            return 2;
        }
    };
    let sudo = match trusted_sudo(&SUDO_PATHS) {
        Ok(sudo) => sudo,
        Err(err) => {
            eprintln!("arterminal: --su: {err}");
            return 2;
        }
    };
    // Only a ticket this run created is revoked afterwards. One the user already held is theirs,
    // part of whatever they were doing, and is left exactly as found. Tickets are per terminal by
    // default, and this one is busy with the picker until the child exits, so nothing else can
    // take a ticket in it between the probe and the revoke.
    let had_ticket = sudo_ticket_exists(&sudo);
    match had_ticket {
        true => eprintln!(
            "arterminal: --su: using your cached sudo to open the keyboards; root is dropped as \
             soon as they are open"
        ),
        false => eprint!("{}", session_prompt()),
    }
    let status = sudo_command(&sudo, &exe, std::env::args_os().skip(1)).status();
    if !had_ticket {
        revoke_sudo_ticket(&sudo);
    }
    match status {
        // A child killed by a signal has no code; 2 is "unusable", which is what it was.
        Ok(status) => status.code().unwrap_or(2),
        Err(err) => {
            eprintln!("arterminal: --su: could not run sudo: {err}");
            2
        }
    }
}

/// `sudo -- <exe> RERUN_MARK <args>`: this exact binary with this exact command line, marked as
/// the re-run, so the child lands in [`for_this_run`] again and finds itself root. Nothing is
/// rebuilt from parsed flags, so there is nothing to drift when a flag is added.
fn sudo_command(sudo: &Path, exe: &Path, args: impl IntoIterator<Item = OsString>) -> Command {
    let mut command = Command::new(sudo);
    command.arg("--").arg(exe).arg(RERUN_MARK).args(args);
    command
}

/// Where sudo is taken from: fixed system paths, and never `PATH`. The program asking for the
/// password is exactly what a planted `sudo` early in `PATH` would impersonate — the `argv[0]`
/// threat again, at the one moment the user has just been told to type their password.
///
/// Most trusted first. `/usr/local/bin` comes last because it is the one most often writable by a
/// local admin group, and it must never shadow NixOS's wrapper.
const SUDO_PATHS: [&str; 4] =
    ["/usr/bin/sudo", "/bin/sudo", "/run/wrappers/bin/sudo", "/usr/local/bin/sudo"];

/// The first of `candidates` that nobody but root could have put where it is — the path as
/// written and the path it resolves to, both — returned resolved, so that what is checked is
/// what runs. Refused rather than guessed when there is none: a sudo that someone else could
/// have placed is not one to hand a password to.
fn trusted_sudo(candidates: &[&str]) -> io::Result<PathBuf> {
    // TWO passes, and neither covers the other. A link planted in a writable directory resolves
    // to a clean root-owned target, so only the pass on the path AS WRITTEN catches it, on the
    // directory holding the link. A chain leading THROUGH a writable directory is caught only by
    // the pass on the RESOLVED path. Folding them into one reopens one hole or the other — the
    // test planting a link to /bin/sh is the case that proves it.
    candidates
        .iter()
        .map(Path::new)
        .filter(|path| only_root_could_have_put(path))
        .find_map(|path| {
            std::fs::canonicalize(path).ok().filter(|real| only_root_could_have_put(real))
        })
        .ok_or_else(|| {
            let looked = candidates.join(", ");
            io::Error::new(io::ErrorKind::NotFound, format!("no root-owned sudo at {looked}"))
        })
}

/// Whether `path` is a file that only root could have put there: it, and every directory above
/// it, owned by root and writable by nobody else. The directories are the point. A file's own
/// owner says nothing about a link to it, and a directory anyone else can write lets them plant
/// one — symbolic or hard — to any root-owned program at all.
fn only_root_could_have_put(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    // `metadata`, which follows links — never `symlink_metadata`, however much like hardening
    // that looks. A link's own mode is always 0777, and on merged-/usr systems `/bin` IS a link
    // to `usr/bin`, so judging links rather than what they lead to would refuse a perfectly good
    // `/bin/sudo`. The guarantee comes from the directories, not from telling links from files.
    let root_only = |at: &Path| {
        std::fs::metadata(at).is_ok_and(|meta| meta.uid() == 0 && meta.mode() & 0o022 == 0)
    };
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) && path.ancestors().all(root_only)
}

/// Whether sudo would run something right now without asking — a cached ticket.
fn sudo_ticket_exists(sudo: &Path) -> bool {
    silent_sudo(sudo, &["-n", "true"])
}

/// Forget the ticket (`sudo -k`).
fn revoke_sudo_ticket(sudo: &Path) {
    let _ = silent_sudo(sudo, &["-k"]);
}

/// `sudo <args>` with every stream closed, reporting only whether it succeeded.
fn silent_sudo(sudo: &Path, args: &[&str]) -> bool {
    Command::new(sudo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether there is a person at a terminal to ask.
fn interactive() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal()
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The user sudo says ran it: who to become again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Caller {
    uid: u32,
    gid: u32,
}

impl Caller {
    /// Read from sudo's `SUDO_UID` and `SUDO_GID`, FAILING CLOSED: unset, not a number, or root's
    /// own `0` are refused, never defaulted — a defaulted 0 would turn the drop into a no-op that
    /// then reports success.
    fn from_sudo_env(uid: Option<&str>, gid: Option<&str>) -> io::Result<Self> {
        fn id(name: &str, value: Option<&str>) -> io::Result<u32> {
            let value = value.ok_or_else(|| refuse(format!("{name} is not set")))?;
            match value.parse::<u32>() {
                Ok(0) => Err(refuse(format!("{name} is 0, so there is nobody to become but root"))),
                Ok(id) => Ok(id),
                Err(_) => Err(refuse(format!("{name} is not a user id: {value:?}"))),
            }
        }
        Ok(Self { uid: id("SUDO_UID", uid)?, gid: id("SUDO_GID", gid)? })
    }
}

fn refuse(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("cannot give root back: {why}"))
}

#[cfg(target_os = "linux")]
use linux::drop_root;

/// Off Linux nothing was opened as root, because there is nothing to open: `plan` only ever
/// drops for a root process that sudo started, and here that process has no keyboards either.
/// Refused rather than skipped all the same, so a future caller cannot mistake it for a drop.
#[cfg(not(target_os = "linux"))]
fn drop_root(_: &Caller) -> io::Result<()> {
    Err(refuse("dropping root is only written for Linux".to_string()))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{refuse, Caller};
    use std::ffi::{CStr, CString};
    use std::io;

    /// Become `caller` for good, and prove it. See the module docs for the order and the checks.
    pub(super) fn drop_root(caller: &Caller) -> io::Result<()> {
        let name = user_name(caller.uid)?;
        let expected = group_list(&name, caller.gid)?;
        // SAFETY (all three): plain syscalls on this process's own credentials, with a
        // NUL-terminated name that outlives the call; the results are checked right here.
        check(unsafe { libc::initgroups(name.as_ptr(), caller.gid) }, "initgroups")?;
        check(unsafe { libc::setresgid(caller.gid, caller.gid, caller.gid) }, "setresgid")?;
        check(unsafe { libc::setresuid(caller.uid, caller.uid, caller.uid) }, "setresuid")?;
        verify(caller, &expected)
    }

    /// That this process is `caller` and nothing more: every user and group id theirs, neither
    /// kind of root regainable, and exactly their supplementary groups.
    pub(super) fn verify(caller: &Caller, expected_groups: &[u32]) -> io::Result<()> {
        let (mut real, mut effective, mut saved) = (0, 0, 0);
        // SAFETY: the out-pointers are to live locals of the right type.
        check(unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) }, "getresuid")?;
        if [real, effective, saved] != [caller.uid; 3] {
            return Err(refuse(format!(
                "user ids are {real}/{effective}/{saved}, not {}",
                caller.uid
            )));
        }
        // SAFETY: as above.
        check(unsafe { libc::getresgid(&mut real, &mut effective, &mut saved) }, "getresgid")?;
        if [real, effective, saved] != [caller.gid; 3] {
            return Err(refuse(format!(
                "group ids are {real}/{effective}/{saved}, not {}",
                caller.gid
            )));
        }
        // If either probe SUCCEEDS the process is root again at this very moment, and an error
        // unwinding from here would run everything above it as root — the destructors, the
        // caller's error path. So it dies where it stands.
        // SAFETY: both are plain syscalls on this process's own credentials.
        if unsafe { libc::setuid(0) } == 0 {
            die_as_root("root is still regainable after the drop");
        }
        // SAFETY: as above.
        if unsafe { libc::setgid(0) } == 0 {
            die_as_root("group root is still regainable after the drop");
        }
        let mut now = current_groups()?;
        let mut expected = expected_groups.to_vec();
        now.sort_unstable();
        now.dedup();
        expected.sort_unstable();
        expected.dedup();
        if now != expected {
            return Err(refuse(format!("groups are {now:?}, not the user's own {expected:?}")));
        }
        Ok(())
    }

    /// Stop at once: this process is root when it should not be, and must do nothing more.
    fn die_as_root(why: &str) -> ! {
        eprintln!("arterminal: --su: {why}; stopping at once");
        // SAFETY: `_exit` ends the process there and then: no unwinding, no destructors, nothing
        // further run with the privilege that should have been gone.
        unsafe { libc::_exit(2) }
    }

    fn check(result: libc::c_int, call: &str) -> io::Result<()> {
        match result {
            0 => Ok(()),
            _ => Err(refuse(format!("{call} failed: {}", io::Error::last_os_error()))),
        }
    }

    /// The login name of `uid`, from the user database.
    pub(super) fn user_name(uid: u32) -> io::Result<CString> {
        let mut buffer = vec![0 as libc::c_char; 1024];
        loop {
            // SAFETY: zeroed is a valid `passwd` (null pointers, zero ids) for getpwuid_r to fill.
            let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: every pointer is to a live local or to `buffer`, whose length is passed.
            let error = unsafe {
                libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found)
            };
            match error {
                libc::ERANGE if buffer.len() < 1 << 20 => buffer.resize(buffer.len() * 2, 0),
                0 if !found.is_null() => {
                    // SAFETY: on success `pw_name` points into `buffer`, NUL-terminated.
                    return Ok(unsafe { CStr::from_ptr(entry.pw_name) }.to_owned());
                }
                0 => return Err(refuse(format!("no user has uid {uid}"))),
                error => {
                    let why = io::Error::from_raw_os_error(error);
                    return Err(refuse(format!("cannot look up uid {uid}: {why}")));
                }
            }
        }
    }

    /// The groups `name` belongs to, `gid` among them — what `initgroups` will set.
    pub(super) fn group_list(name: &CStr, gid: u32) -> io::Result<Vec<u32>> {
        let mut groups = vec![0; 64];
        loop {
            let mut count = groups.len() as libc::c_int;
            // SAFETY: `groups` holds `count` entries and outlives the call; the name is valid.
            let found =
                unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
            match found {
                -1 if (count as usize) > groups.len() => groups.resize(count as usize, 0),
                -1 if groups.len() < 1 << 16 => groups.resize(groups.len() * 2, 0),
                -1 => return Err(refuse("the user is in too many groups".to_string())),
                _ => {
                    groups.truncate(count as usize);
                    return Ok(groups);
                }
            }
        }
    }

    /// This process's supplementary groups.
    pub(super) fn current_groups() -> io::Result<Vec<u32>> {
        // SAFETY: a zero-length query writes nothing and returns the count.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count < 0 {
            return Err(refuse(format!("getgroups failed: {}", io::Error::last_os_error())));
        }
        let mut groups = vec![0; count as usize];
        // SAFETY: `groups` holds exactly `count` entries.
        let count = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        match count {
            -1 => Err(refuse(format!("getgroups failed: {}", io::Error::last_os_error()))),
            count => {
                groups.truncate(count as usize);
                Ok(groups)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Situation = Situation {
        root: false,
        under_sudo: false,
        rerun: false,
        devices_readable: false,
        interactive: true,
        linux: true,
    };

    /// Every row of the decision, including the one that matters most: however sudo started
    /// this process, it never asks again — `/dev/input` is missing in this very container, and
    /// without the guard the re-run would find it missing too and ask for a password forever.
    #[test]
    fn the_plan_asks_only_a_plain_user_at_a_terminal_who_cannot_read_the_devices() {
        assert_eq!(plan(USER), Plan::RerunUnderSudo);
        assert_eq!(plan(Situation { devices_readable: true, ..USER }), Plan::Proceed, "the group");
        assert_eq!(plan(Situation { interactive: false, ..USER }), Plan::Proceed, "a pipeline");
        assert_eq!(plan(Situation { linux: false, ..USER }), Plan::Proceed, "no /dev/input at all");

        let child = Situation { root: true, under_sudo: true, ..USER };
        assert_eq!(plan(child), Plan::DropThenProceed);
        for readable in [false, true] {
            let child = Situation { devices_readable: readable, ..child };
            assert_eq!(plan(child), Plan::DropThenProceed, "readable={readable}: never re-asks");
        }
        assert_eq!(plan(Situation { root: true, ..USER }), Plan::Proceed, "a root login");
        assert_eq!(plan(Situation { under_sudo: true, ..USER }), Plan::Proceed, "sudo -u someone");

        // Our own re-run: dropping when sudo said who ran it, REFUSING when it did not — never
        // going on as root — and never asking again when it is not root at all.
        let rerun = Situation { rerun: true, ..USER };
        assert_eq!(
            plan(Situation { root: true, under_sudo: true, ..rerun }),
            Plan::DropThenProceed
        );
        assert_eq!(plan(Situation { root: true, ..rerun }), Plan::Refuse, "no SUDO_UID");
        let readable = Situation { root: true, devices_readable: true, ..rerun };
        assert_eq!(plan(readable), Plan::Refuse, "however readable the devices are");
        assert_eq!(plan(rerun), Plan::Proceed, "not root: nothing to drop, and no second prompt");
    }

    #[test]
    fn the_rerun_mark_is_skipped_only_where_it_is_put() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let marked = args(&[RERUN_MARK, "--su", "art.txt"]);
        assert_eq!(without_rerun_mark(&marked), &args(&["--su", "art.txt"])[..]);
        let plain = args(&["--su", "art.txt"]);
        assert_eq!(without_rerun_mark(&plain), &plain[..]);
        let later = args(&["art.txt", RERUN_MARK]);
        assert_eq!(without_rerun_mark(&later), &later[..], "only ever the first argument");
    }

    /// sudo comes from fixed system paths, and only a file that nobody but root could have put
    /// there counts — so anything this unprivileged test can create is exactly what must be
    /// refused: a file of its own, and a link to a real root-owned program alike.
    #[test]
    fn only_a_sudo_nobody_but_root_could_have_placed_is_trusted() {
        let dir = std::env::temp_dir().join(format!("arterminal-sudo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let planted = dir.join("sudo");
        std::fs::write(&planted, "#!/bin/sh\n").expect("written");
        let refused = trusted_sudo(&[planted.to_str().expect("utf-8")]).expect_err("not root's");
        assert_eq!(refused.kind(), io::ErrorKind::NotFound, "{refused}");

        // A link in a directory this test can write, to a program root really does own: the
        // file checks out, and the directory is what gives it away.
        let real = Path::new("/bin/sh");
        if only_root_could_have_put(&std::fs::canonicalize(real).expect("a shell")) {
            let link = dir.join("linked-sudo");
            std::os::unix::fs::symlink(real, &link).expect("linked");
            assert!(!only_root_could_have_put(&link), "planted where others can write");
            assert!(trusted_sudo(&[link.to_str().expect("utf-8")]).is_err());
        }

        let missing = dir.join("absent").to_str().expect("utf-8").to_string();
        assert!(trusted_sudo(&[&missing]).is_err(), "a path with nothing there is passed over");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
        assert!(SUDO_PATHS.iter().all(|path| path.starts_with('/')), "absolute, never PATH");
        assert_eq!(SUDO_PATHS.last(), Some(&"/usr/local/bin/sudo"), "the least trusted, last");
    }

    #[test]
    fn going_ahead_without_the_devices_says_why() {
        assert_eq!(note_for(Situation { devices_readable: true, ..USER }), None);
        let pipeline = note_for(Situation { interactive: false, ..USER }).expect("a note");
        assert!(pipeline.contains("needs a terminal"), "{pipeline}");
        let elsewhere = note_for(Situation { linux: false, ..USER }).expect("a note");
        assert!(elsewhere.contains("Linux"), "{elsewhere}");
        let root = note_for(Situation { root: true, ..USER }).expect("a note");
        assert!(root.contains("could be opened"), "{root}");
    }

    /// FAIL CLOSED: sudo's word on who to become is taken only when it is complete and is not
    /// root. A default here would be a drop that did nothing and said it had.
    #[test]
    fn who_to_become_is_read_strictly() {
        assert_eq!(
            Caller::from_sudo_env(Some("1000"), Some("100")).unwrap(),
            Caller { uid: 1000, gid: 100 }
        );
        for (uid, gid) in [
            (None, Some("100")),
            (Some("1000"), None),
            (Some("0"), Some("100")),
            (Some("1000"), Some("0")),
            (Some("root"), Some("100")),
            (Some("-1"), Some("100")),
            (Some(""), Some("100")),
        ] {
            let refused = Caller::from_sudo_env(uid, gid).expect_err("refused");
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{uid:?} {gid:?}");
        }
    }

    /// The child is this very binary, named by the path given — `current_exe`, never argv[0] —
    /// run by the sudo given, marked as the re-run, with the command line passed through
    /// untouched.
    #[test]
    fn the_rerun_is_this_binary_with_the_same_arguments() {
        let args = ["--su", "art.txt"].map(OsString::from);
        let command =
            sudo_command(Path::new("/usr/bin/sudo"), Path::new("/opt/bin/arterminal"), args);
        assert_eq!(command.get_program(), "/usr/bin/sudo");
        let passed: Vec<_> = command.get_args().collect();
        assert_eq!(passed, ["--", "/opt/bin/arterminal", RERUN_MARK, "--su", "art.txt"]);
    }

    /// The prompt keeps every promise the user is agreeing to.
    #[test]
    fn the_password_prompt_says_what_it_is_for_and_how_long_root_lasts() {
        let flat = session_prompt().split_whitespace().collect::<Vec<_>>().join(" ");
        for promise in [
            "THIS RUN only",
            "Root is dropped as soon as they are open",
            "before the art file is even read",
            "nothing persists after exit",
            "join the input group",
            "every program you run read every key",
        ] {
            assert!(flat.contains(promise), "the prompt lost {promise:?}: {flat}");
        }
    }

    /// Tests run unprivileged, so the drop's first step must refuse — the failure path, taken
    /// for real — and the verification must pass for who this process already is.
    #[cfg(target_os = "linux")]
    #[test]
    fn unprivileged_the_drop_fails_closed_and_the_check_knows_who_we_are() {
        if is_root() {
            return; // Nothing here is meaningful as root, and it must never try a real drop.
        }
        // SAFETY: no preconditions.
        let me = Caller { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() } };
        let refused = drop_root(&me).expect_err("the drop needs root");
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
        // SAFETY: no preconditions.
        assert_eq!(unsafe { libc::getuid() }, me.uid, "and refusing changed nothing");

        let groups = linux::current_groups().expect("readable");
        linux::verify(&me, &groups).expect("this process is exactly itself");
        let other = Caller { uid: me.uid + 1, ..me };
        assert!(linux::verify(&other, &groups).is_err(), "a different user is noticed");
        let mut extra = groups.clone();
        extra.push(0);
        assert!(linux::verify(&me, &extra).is_err(), "a group list that differs is noticed");
    }
}
