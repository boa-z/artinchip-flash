use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, OnceLock};

static VERBOSE: AtomicBool = AtomicBool::new(false);
static GUI_OUTPUT: OnceLock<mpsc::SyncSender<String>> = OnceLock::new();
static DROPPED: AtomicBool = AtomicBool::new(false);

pub fn gui_output() -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::sync_channel(2048);
    let _ = GUI_OUTPUT.set(sender);
    receiver
}

pub fn take_dropped_output() -> bool {
    DROPPED.swap(false, Ordering::Relaxed)
}

pub fn emit(args: std::fmt::Arguments<'_>) {
    if let Some(sender) = GUI_OUTPUT.get() {
        if sender.try_send(args.to_string()).is_err() {
            DROPPED.store(true, Ordering::Relaxed);
        }
    } else {
        eprintln!("{args}");
    }
}

/// Enable (`true`) or disable verbose transport-level logging.
///
/// CLI `--verbose` and GUI `config.verbose` both funnel through here so the
/// library never decides on its own what reaches stderr.
pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::SeqCst);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::SeqCst)
}

/// Low-level transport chatter (CBW/CSW bytes, UART framing, reconnect polls).
/// Gated behind [`verbose`] so GUI logs and default CLI output stay clean.
#[macro_export]
macro_rules! log_verbose {
    ($($arg:tt)*) => {
        if $crate::verbosity::verbose() {
            $crate::verbosity::emit(format_args!($($arg)*));
        }
    };
}
