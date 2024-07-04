// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

use crate::locking::SpinLock;
use crate::serial::{Terminal, DEFAULT_SERIAL_PORT};
use crate::utils::immut_after_init::{ImmutAfterInitCell, ImmutAfterInitResult};
use core::fmt;

#[derive(Clone, Copy, Debug)]
struct Console {
    writer: &'static dyn Terminal,
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.bytes() {
            self.writer.put_byte(ch);
        }
        Ok(())
    }
}

static WRITER: SpinLock<Console> = SpinLock::new(Console {
    writer: &DEFAULT_SERIAL_PORT,
});
static CONSOLE_INITIALIZED: ImmutAfterInitCell<bool> = ImmutAfterInitCell::new(false);

pub fn init_console(writer: &'static dyn Terminal) -> ImmutAfterInitResult<()> {
    WRITER.lock().writer = writer;
    CONSOLE_INITIALIZED.reinit(&true)
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    use core::fmt::Write;
    if !*CONSOLE_INITIALIZED {
        return;
    }
    WRITER.lock().write_fmt(args).unwrap();
}

#[derive(Clone, Copy, Debug)]
struct ConsoleLogger {
    name: &'static str,
}

impl ConsoleLogger {
    const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

impl log::Log for ConsoleLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // The logger being uninitialized is impossible, as that would mean it
        // wouldn't have been registered with the log library.
        // Log format/detail depends on the level.
        match record.metadata().level() {
            log::Level::Error | log::Level::Warn => {
                _print(format_args!(
                    "[{}] {}: {}\n",
                    self.name,
                    record.metadata().level().as_str(),
                    record.args()
                ));
            }

            log::Level::Info => {
                _print(format_args!("[{}] {}\n", self.name, record.args()));
            }

            log::Level::Debug | log::Level::Trace => {
                _print(format_args!(
                    "[{}/{}] {} {}\n",
                    self.name,
                    record.metadata().target(),
                    record.metadata().level().as_str(),
                    record.args()
                ));
            }
        };
    }

    fn flush(&self) {}
}

static CONSOLE_LOGGER: ImmutAfterInitCell<ConsoleLogger> = ImmutAfterInitCell::uninit();

pub fn install_console_logger(component: &'static str) -> ImmutAfterInitResult<()> {
    CONSOLE_LOGGER.init(&ConsoleLogger::new(component))?;

    if let Err(e) = log::set_logger(&*CONSOLE_LOGGER) {
        // Failed to install the ConsoleLogger, presumably because something had
        // installed another logger before. No logs will appear at the console.
        // Print an error string.
        _print(format_args!(
            "[{}]: ERROR: failed to install console logger: {:?}",
            component, e,
        ));
    }

    // Log levels are to be configured via the log's library feature configuration.
    log::set_max_level(log::LevelFilter::Trace);
    Ok(())
}

#[macro_export]
macro_rules! println {
    () => (log::info!(""));
    ($($arg:tt)*) => (log::info!($($arg)*));
}
