//! snare is a GitHub webhooks daemon. Architecturally it is split in two:
//!   * The `httpserver` listens for incoming hooks, checks that they're valid, and adds them to a
//!     `Queue`.
//!   * The `jobrunner` pops elements from the `Queue` and runs them in parallel.
//! These two components run as two different threads: the `httpserver` writes a solitary byte to
//! an "event pipe" to wake up the `jobrunner` when the queue has new elements. We also wake up the
//! `jobrunner` on SIGHUP and SIGCHLD.

mod config;
mod config_ast;
mod httpserver;
mod jobrunner;
mod queue;

use std::{
    env::{self, current_exe, set_current_dir},
    error::Error,
    ffi::CString,
    fmt::Display,
    os::unix::io::RawFd,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use getopts::Options;
use hyper::Server;
use libc::{c_char, openlog, syslog, LOG_CONS, LOG_CRIT, LOG_DAEMON, LOG_ERR};
use nix::{
    fcntl::OFlag,
    unistd::{daemon, pipe2, setresgid, setresuid, Gid, Uid},
};
use tokio::runtime::Runtime;
use users::{get_user_by_name, os::unix::UserExt};

use config::Config;
use queue::Queue;

/// Default location of `snare.conf`.
const SNARE_CONF_PATH: &str = "/etc/snare/snare.conf";

pub(crate) struct Snare {
    /// Are we currently running as a daemon?
    daemonised: bool,
    /// The location of snare.conf; this file will be reloaded if SIGHUP is received.
    conf_path: PathBuf,
    /// The current configuration: note that this can change at any point due to SIGHUP. All calls
    /// to `conf.lock().unwrap()` are considered safe since the only way this can fail is if the
    /// other thread has `panic`ed, at which point we're already doomed.
    conf: Mutex<Config>,
    /// The current queue of incoming jobs. All calls to `queue.lock().unwrap()` are considered
    /// safe since the only way this can fail is if the other thread has `panic`ed, at which point
    /// we're already doomed.
    queue: Mutex<Queue>,
    /// The read end of the pipe used by the httpserver and the SIGHUP handler to wake up the job
    /// runner thread.
    event_read_fd: RawFd,
    /// The write end of the pipe used by the httpserver and the SIGHUP handler to wake up the job
    /// runner thread.
    event_write_fd: RawFd,
    /// Has a SIGHUP event occurred? If so, the jobrunner will process it, and set this to false in
    /// case future SIGHUP events are detected.
    sighup_occurred: Arc<AtomicBool>,
}

impl Snare {
    /// Check to see if we've received a SIGHUP since the last check. If so, we will try reloading
    /// the snare.conf file specified when we started. **Note that another thread may have called
    /// this function and caused the config to have changed.**
    fn check_for_sighup(&self) {
        if self.sighup_occurred.load(Ordering::Relaxed) {
            match Config::from_path(&self.conf_path) {
                Ok(conf) => *self.conf.lock().unwrap() = conf,
                Err(msg) => self.error(&msg),
            }
            self.sighup_occurred.store(false, Ordering::Relaxed);
        }
    }

    /// Log `msg` as an error.
    ///
    /// # Panics
    ///
    /// If `msg` contains a `NUL` byte.
    fn error(&self, msg: &str) {
        if self.daemonised {
            // We know that `%s` and `<can't represent as CString>` are both valid C strings, and
            // that neither unwrap() can fail.
            let fmt = CString::new("%s").unwrap();
            let msg = CString::new(msg)
                .unwrap_or_else(|_| CString::new("<can't represent as CString>").unwrap());
            unsafe {
                syslog(LOG_ERR, fmt.as_ptr(), msg.as_ptr());
            }
        } else {
            eprintln!("{}", msg);
        }
    }

    /// Log `msg` as an error, with extra information in the Rust [`Error`](::Error) `err` and then
    /// exit(1).
    ///
    /// # Panics
    ///
    /// If `msg` contains a `NUL` byte.
    fn error_err<E: Into<Box<dyn Error>> + Display>(&self, msg: &str, err: E) {
        self.error(&format!("{}: {}", msg, err));
    }

    /// Log `msg` as a fatal error and then exit(1).
    ///
    /// # Panics
    ///
    /// If `msg` contains a `NUL` byte.
    fn fatal(&self, msg: &str) -> ! {
        fatal(self.daemonised, msg);
    }

    /// Log `msg` as a fatal error, with extra information in the Rust [`Error`](::Error) `err` and
    /// then exit(1).
    ///
    /// # Panics
    ///
    /// If `msg` contains a `NUL` byte.
    fn fatal_err<E: Into<Box<dyn Error>> + Display>(&self, msg: &str, err: E) -> ! {
        self.fatal(&format!("{}: {}", msg, err));
    }
}

/// Try to find a `snare.conf` file.
fn search_snare_conf() -> Option<PathBuf> {
    let p = PathBuf::from(SNARE_CONF_PATH);
    if p.is_file() {
        return Some(p);
    }
    None
}

/// If the config specified a 'user' then switch to that and update $HOME and $USER appropriately.
/// This function must not be called after daemonisation.
fn change_user(conf: &Config) {
    match conf.user {
        Some(ref user) => match get_user_by_name(&user) {
            Some(u) => {
                let gid = Gid::from_raw(u.primary_group_id());
                if let Err(e) = setresgid(gid, gid, gid) {
                    fatal_err(false, &format!("Can't switch to group '{}'", user), e);
                }
                let uid = Uid::from_raw(u.uid());
                if let Err(e) = setresuid(uid, uid, uid) {
                    fatal_err(false, &format!("Can't switch to user '{}'", user), e);
                }
                env::set_var("HOME", u.home_dir());
                env::set_var("USER", user);
            }
            None => fatal(false, &format!("Unknown user '{}'", user)),
        },
        None => {
            if Uid::current().is_root() {
                fatal(
                    false,
                    "The 'user' option must be set if snare is run as root",
                );
            }
        }
    }
}

fn progname() -> String {
    match current_exe() {
        Ok(p) => p
            .file_name()
            .map(|x| x.to_str().unwrap_or("snare"))
            .unwrap_or("snare")
            .to_owned(),
        Err(_) => "snare".to_owned(),
    }
}

/// Exit with a fatal error.
fn fatal(daemonised: bool, msg: &str) -> ! {
    if daemonised {
        // We know that `%s` and `<can't represent as CString>` are both valid C strings, and
        // that neither unwrap() can fail.
        let fmt = CString::new("%s").unwrap();
        let msg = CString::new(msg)
            .unwrap_or_else(|_| CString::new("<can't represent as CString>").unwrap());
        unsafe {
            syslog(LOG_CRIT, fmt.as_ptr(), msg.as_ptr());
        }
    } else {
        eprintln!("{}", msg);
    }
    process::exit(1);
}

/// Exit with a fatal error, printing the contents of `err`.
fn fatal_err<E: Into<Box<dyn Error>> + Display>(daemonised: bool, msg: &str, err: E) -> ! {
    fatal(daemonised, &format!("{}: {}", msg, err));
}

/// Print out program usage then exit. This function must not be called after daemonisation.
fn usage() -> ! {
    eprintln!("Usage: {} [-c <config-path>] [-d]", progname());
    process::exit(1)
}

pub fn main() {
    let args: Vec<String> = env::args().collect();
    let matches = Options::new()
        .optmulti("c", "config", "Path to snare.conf.", "<conf-path>")
        .optflag(
            "d",
            "",
            "Don't detach from the terminal and log errors to stderr.",
        )
        .optflag("h", "help", "")
        .parse(&args[1..])
        .unwrap_or_else(|_| usage());
    if matches.opt_present("h") {
        usage();
    }

    let daemonise = !matches.opt_present("d");

    let conf_path = match matches.opt_str("c") {
        Some(p) => PathBuf::from(&p),
        None => search_snare_conf().unwrap_or_else(|| fatal(false, "Can't find snare.conf")),
    };
    let conf = Config::from_path(&conf_path).unwrap_or_else(|m| fatal(false, &m));

    change_user(&conf);

    set_current_dir("/").unwrap_or_else(|_| fatal(false, "Can't chdir to '/'"));
    if daemonise {
        if let Err(e) = daemon(true, false) {
            fatal_err(false, "Couldn't daemonise: {}", e);
        }
    }

    // openlog's first argument `ident` is incompletely specified, but in practise we have to
    // assume that syslog merely stores a pointer to the string (i.e. it doesn't copy the string).
    // We thus deliberately leak memory here in order that the pointer always points to valid
    // memory. The unwrap() here is ugly, but if it fails, it means we've run out of memory, so
    // it's neither likely to fail nor, if it does, can we do anything to clear up from it.
    let progname =
        Box::into_raw(CString::new(progname()).unwrap().into_boxed_c_str()) as *const c_char;
    unsafe {
        openlog(progname, LOG_CONS, LOG_DAEMON);
    }

    let (event_read_fd, event_write_fd) = match pipe2(OFlag::O_NONBLOCK) {
        Ok(p) => p,
        Err(e) => fatal_err(daemonise, "Can't create pipe", e),
    };
    let sighup_occurred = Arc::new(AtomicBool::new(false));
    {
        let sighup_occurred = Arc::clone(&sighup_occurred);
        if let Err(e) = unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGHUP, move || {
                // All functions called in this function must be signal safe. See signal(3).
                sighup_occurred.store(true, Ordering::Relaxed);
                nix::unistd::write(event_write_fd, &[0]).ok();
            })
        } {
            fatal_err(daemonise, "Can't install SIGHUP handler", e);
        }
        if let Err(e) = unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGCHLD, move || {
                // All functions called in this function must be signal safe. See signal(3).
                nix::unistd::write(event_write_fd, &[0]).ok();
            })
        } {
            fatal_err(daemonise, "Can't install SIGCHLD handler", e);
        }
    }

    let snare = Arc::new(Snare {
        daemonised: daemonise,
        conf_path,
        conf: Mutex::new(conf),
        queue: Mutex::new(Queue::new()),
        event_read_fd,
        event_write_fd,
        sighup_occurred,
    });

    match jobrunner::attend(Arc::clone(&snare)) {
        Ok(x) => x,
        Err(e) => snare.fatal_err("Couldn't start runner thread", e),
    }

    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => snare.fatal_err("Couldn't start tokio runtime.", e),
    };
    rt.block_on(async {
        let server = match Server::try_bind(&snare.conf.lock().unwrap().listen) {
            Ok(s) => s,
            Err(e) => snare.fatal_err("Couldn't bind to address", e),
        };

        httpserver::serve(server, Arc::clone(&snare)).await;
    });
}
