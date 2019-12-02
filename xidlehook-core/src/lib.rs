#![warn(
    // Harden built-in lints
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    unreachable_pub,

    // Harden clippy lints
    clippy::cargo_common_metadata,
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro,
    clippy::decimal_literal_representation,
    clippy::float_cmp_const,
    clippy::get_unwrap,
    clippy::integer_arithmetic,
    clippy::integer_division,
    clippy::pedantic,
    clippy::print_stdout,
)]

//! Instead of implementing your extension as something that
//! communicates with xidlehook, what about implementing your
//! extension as something that *is* xidlehook?
//!
//! This library lets you create your own xidlehook front-end using a
//! powerful timer and module system.

use std::{cmp, convert::TryInto, fmt, ptr, time::Duration};

use log::trace;
use nix::libc;

/// The default error type for xidlehook. Unfortunately, it's a
/// dynamic type for now.
pub type Error = Box<dyn std::error::Error>;
/// An alias to Result which overrides the default Error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub mod modules;
pub mod timers;

pub use self::{
    modules::{Module, Progress},
    timers::Timer,
};

/// An identifier for a timer, based on the index in the timer list
/// and its length.
#[derive(Clone, Copy, Debug)]
pub struct TimerInfo {
    /// The index of this timer in the timer list
    pub index: usize,
    /// The length of the timer list
    pub length: usize,
}

/// The main xidlehook instance that allows you to schedule things
pub struct Xidlehook<T: Timer, M: Module>
where
    T: Timer,
    M: Module,
{
    module: M,

    timers: Vec<T>,
    next_index: usize,
    /// The base idle time: the absolute idle time when the last timer
    /// was called, used to retrieve the relative idle time since it.
    base_idle_time: Duration,
    /// The previous idle time, used for comparing whether or not the
    /// user has moved.
    previous_idle_time: Duration,
    /// If a chain is aborted during the process, store this here as
    /// to not make any more attempts to continue it.
    aborted: bool,
}
impl<T: Timer> Xidlehook<T, ()> {
    /// An empty instance without any modules
    pub fn new(timers: Vec<T>) -> Self {
        Self {
            module: (),

            timers,
            next_index: 0,
            base_idle_time: Duration::default(),
            previous_idle_time: Duration::default(),
            aborted: false,
        }
    }
}

macro_rules! with_module {
    ($self:expr, $module:expr) => {
        Xidlehook {
            module: $module,
            timers: $self.timers,
            next_index: $self.next_index,
            base_idle_time: $self.base_idle_time,
            previous_idle_time: $self.previous_idle_time,
            aborted: $self.aborted,
        }
    };
}

// There are some false positive with Self and generics.
#[allow(clippy::use_self)]
impl<T, M> Xidlehook<T, M>
where
    T: Timer,
    M: Module,
{
    /// Return this xidlehook instance but with this module replaced.
    pub fn with_module<N: Module>(self, other: N) -> Xidlehook<T, N> {
        with_module!(self, other)
    }

    /// Return this xidlehook instance but with an additional module
    /// activated. This works using the timer impl for `(A, B)` to get
    /// a fixed-size list of modules at compile time.
    pub fn register<N: Module>(self, other: N) -> Xidlehook<T, (M, N)> {
        // Sadly cannot use `self.with_module` safely due to use of
        // `self.module` - Rust isn't intelligent enough to realize
        // the function isn't using that field. This is one of the few
        // shortcomings of Rust IMO.
        with_module!(self, (self.module, other))
    }

    /// Returns an immutable list of all timers
    pub fn timers(&self) -> &Vec<T> {
        &self.timers
    }

    /// Returns a mutable list of all timers. Use this to add or
    /// remove timers as you wish. This will abort the idle chain as
    /// that may otherwise panic.
    pub fn timers_mut(&mut self) -> Result<&mut Vec<T>> {
        self.abort()?;
        Ok(&mut self.timers)
    }

    /// Returns the previous timer that was activated (but not
    /// deactivated)
    fn previous(&mut self) -> Option<&mut T> {
        self.next_index
            .checked_sub(1)
            .map(move |i| &mut self.timers[i])
    }

    /// Calls the abortion function on the current timer and stops pursuing the chain
    fn abort(&mut self) -> Result<()> {
        if self.aborted {
            return Ok(());
        }

        self.aborted = true;
        if let Some(prev) = self.previous() {
            prev.abort()?;
        }
        Ok(())
    }

    /// Calls the abortion functions on the current timer and restarts
    /// from index zero. Just like `poll` is continued usage after an
    /// error discouraged.
    fn reset(&mut self) -> Result<()> {
        self.abort()?;

        if self.next_index > 0 {
            if let Err(err) = self.module.reset() {
                self.module.warning(&err)?;
            }
            self.next_index = 0;
        }

        self.base_idle_time = Duration::default();
        self.previous_idle_time = Duration::default();
        self.aborted = false;

        Ok(())
    }

    /// Skip ahead to the selected timer. Timers leading up to this
    /// point will not be ran. If you pass `force`, modules will not
    /// even be able to prevent this from happening (all requests
    /// pre-timer would be ignored). Post-timer requests are fully
    /// complied with.
    ///
    /// Whatever the return value is, it's already been handled. If
    /// the return value is `Err(...)`, that means this function
    /// invoked the module's `warning` function and that still wanted
    /// to propagate the error. If the return value is
    /// `Ok(Progress::Abort)`, never mind it. The `self.abort()`
    /// function has already been invoked - it's all cool.
    ///
    /// # Panics
    ///
    /// - If the index is out of bounds
    pub fn trigger(
        &mut self,
        index: usize,
        absolute_time: Duration,
        force: bool,
    ) -> Result<Progress> {
        trace!("Activating timer {}", index);

        let timer_info = TimerInfo {
            index,
            length: self.timers.len(),
        };

        let next = &mut self.timers[index];

        match self.module.pre_timer(timer_info) {
            Ok(_) if force => (),

            Ok(Progress::Continue) => (),
            Ok(Progress::Abort) => {
                trace!("Module requested abort of chain.");
                self.abort()?;
                return Ok(Progress::Abort);
            },
            Ok(Progress::Stop) => return Ok(Progress::Stop),

            Err(err) => {
                self.module.warning(&err)?;
            },
        }

        next.activate()?;
        if let Some(previous) = self.previous() {
            previous.deactivate()?;
        }

        self.base_idle_time = absolute_time;

        match self.module.post_timer(timer_info) {
            Ok(Progress::Continue) => (),
            Ok(Progress::Abort) => {
                trace!("Module requested abort of chain.");
                self.abort()?;
                return Ok(Progress::Abort);
            },
            Ok(Progress::Stop) => return Ok(Progress::Stop),

            Err(err) => {
                self.module.warning(&err)?;
            },
        }

        Ok(Progress::Continue)
    }

    /// Polls the scheduler for any activated timers. On success,
    /// returns the max amount of time a program can sleep for. Only
    /// fatal errors cause this function to return, and at that point,
    /// the state of xidlehook is undefined so it should not be used.
    ///
    /// # Panics
    ///
    /// Panics when there are no timers added - there must always be
    /// at least one.
    pub fn poll(&mut self, absolute_time: Duration) -> Result<Option<Duration>> {
        if absolute_time < self.previous_idle_time {
            // If the idle time has decreased, the only reasonable
            // explanation is that the user briefly wasn't idle.
            self.reset()?;
        }

        self.previous_idle_time = absolute_time;

        let mut max_sleep = self.timers[0]
            .time_left(Duration::default())?
            .unwrap_or_default();
        trace!(
            "Taking the first timer into account. Remaining: {:?}",
            max_sleep
        );

        if self.aborted {
            // This chain was aborted, so don't pursue it
            return Ok(Some(max_sleep));
        }

        let relative_time = absolute_time - self.base_idle_time;
        trace!("Relative time: {:?}", relative_time);

        // When there's a next timer available, get the time until that activates
        if let Some(next) = self.timers.get_mut(self.next_index) {
            if let Some(remaining) = next.time_left(relative_time)? {
                trace!("Taking next timer into account. Remaining: {:?}", remaining);
                max_sleep = cmp::min(max_sleep, remaining);
            } else {
                // Oh! It's already been activated - let's trigger it.

                match self.trigger(self.next_index, absolute_time, false)? {
                    Progress::Continue => (),
                    Progress::Abort => return Ok(Some(max_sleep)),
                    Progress::Stop => return Ok(None),
                }
                // From now on, `relative_time` is invalid. Don't use it.

                loop {
                    self.next_index += 1;

                    if let Some(next) = self.timers.get_mut(self.next_index) {
                        if next.disabled() {
                            trace!("Timer was disabled, going to next...");
                            continue;
                        }
                        if let Some(remaining) = next.time_left(Duration::default())? {
                            trace!(
                                "Taking next-next timer into account. Remaining: {:?}",
                                remaining
                            );
                            max_sleep = cmp::min(max_sleep, remaining);
                        }
                    }
                    break;
                }
            }
        }

        // When there's a previous timer, respect that timer's abort
        // urgency (see `Timer::abort_urgency()`)
        if let Some(abort) = self.previous() {
            if let Some(urgency) = abort.abort_urgency() {
                trace!(
                    "Taking abort urgency into account. Remaining: {:?}",
                    urgency
                );
                max_sleep = cmp::min(max_sleep, urgency);
            }
        }

        Ok(Some(max_sleep))
    }

    /// Runs a standard poll-sleep-repeat loop.
    /// ```rust
    /// # if std::env::var("DISPLAY").is_err() {
    /// #     // Don't fail on CI.
    /// #     return Ok::<(), xidlehook_core::Error>(());
    /// # }
    /// # use std::{
    /// #     sync::atomic::{AtomicBool, Ordering},
    /// #     time::Duration,
    /// # };
    /// #
    /// # use nix::{
    /// #     libc,
    /// #     sys::{signal, wait},
    /// # };
    /// # use xidlehook_core::{
    /// #     modules::{StopAt, Xcb},
    /// #     timers::CallbackTimer,
    /// #     Xidlehook,
    /// # };
    /// #
    /// # let timers = vec![
    /// #     CallbackTimer::new(Duration::from_millis(50), || println!("50ms passed!")),
    /// # ];
    /// # let mut xidlehook = Xidlehook::new(timers)
    /// #     .register(StopAt::completion());
    /// # let xcb = Xcb::new()?;
    /// static EXITED: AtomicBool = AtomicBool::new(false);
    ///
    /// extern "C" fn exit_handler(_signo: libc::c_int) {
    ///     EXITED.store(true, Ordering::SeqCst);
    /// }
    ///
    /// unsafe {
    ///     signal::sigaction(
    ///         signal::Signal::SIGINT,
    ///         &signal::SigAction::new(
    ///             signal::SigHandler::Handler(exit_handler),
    ///             signal::SaFlags::empty(),
    ///             signal::SigSet::empty(),
    ///         ),
    ///     )?;
    /// }
    /// xidlehook.main_sync(&xcb, || EXITED.load(Ordering::SeqCst));
    /// # Ok::<(), xidlehook_core::Error>(())
    /// ```
    pub fn main_sync<F>(mut self, xcb: &self::modules::Xcb, mut callback: F) -> Result<()>
    where
        F: FnMut() -> bool,
    {
        loop {
            let idle = xcb.get_idle()?;
            let delay = match self.poll(idle)? {
                Some(delay) => delay,
                None => break,
            };

            trace!("Sleeping for {:?}", delay);

            // This sleep, unlike `thread::sleep`, will stop for signals.
            unsafe {
                libc::nanosleep(
                    &libc::timespec {
                        tv_sec: delay
                            .as_secs()
                            .try_into()
                            .expect("woah that's one large number"),
                        tv_nsec: delay.subsec_nanos().into(),
                    },
                    ptr::null_mut(),
                );
            }

            if callback() {
                // Oh look, the callback wants us to exit
                break;
            }
        }
        Ok(())
    }

    /// Runs a standard poll-sleep-repeat loop... asynchronously.
    #[cfg(feature = "async-std")]
    pub async fn main_async(&mut self, xcb: &self::modules::Xcb) -> Result<()> {
        loop {
            let idle = xcb.get_idle()?;
            let delay = match self.poll(idle)? {
                Some(delay) => delay,
                None => break,
            };

            trace!("Sleeping for {:?}", delay);
            async_std::task::sleep(delay).await;
        }
        Ok(())
    }
}

impl<T, M> fmt::Debug for Xidlehook<T, M>
where
    T: Timer,
    M: Module + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Modules: {:?}", self.module)
    }
}
