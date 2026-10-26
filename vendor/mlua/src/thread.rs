use std::fmt;
use std::os::raw::{c_int, c_void};

use crate::error::{Error, Result};
use crate::function::Function;
use crate::state::RawLua;
use crate::traits::{FromLuaMulti, IntoLuaMulti};
use crate::types::{LuaType, ValueRef};
use crate::util::{check_stack, error_traceback_thread, pop_error, StackGuard};

#[cfg(not(feature = "luau"))]
use crate::{
    debug::{Debug, HookTriggers},
    types::HookKind,
};

#[cfg(feature = "async")]
use {
    futures_util::stream::Stream,
    std::{
        future::Future,
        marker::PhantomData,
        pin::Pin,
        ptr::NonNull,
        sync::{Arc, Weak, atomic::{AtomicBool, Ordering}},
        task::Wake,
        task::{Context, Poll, Waker},
    },
};

/// Status of a Lua thread (coroutine).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ThreadStatus {
    /// The thread was just created or is suspended (yielded).
    ///
    /// If a thread is in this state, it can be resumed by calling [`Thread::resume`].
    Resumable,
    /// The thread is currently running.
    Running,
    /// The thread has finished executing.
    Finished,
    /// The thread has raised a Lua error during execution.
    Error,
}

/// Internal representation of a Lua thread status.
///
/// The number in `New` and `Yielded` variants is the number of arguments pushed
/// to the thread stack.
#[derive(Clone, Copy)]
enum ThreadStatusInner {
    New(c_int),
    Running,
    Yielded(c_int),
    Finished,
    Error,
}

impl ThreadStatusInner {
    #[cfg(feature = "async")]
    #[inline(always)]
    fn is_resumable(self) -> bool {
        matches!(self, ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_))
    }

    #[cfg(feature = "async")]
    #[inline(always)]
    fn is_yielded(self) -> bool {
        matches!(self, ThreadStatusInner::Yielded(_))
    }
}

/// Handle to an internal Lua thread (coroutine).
#[derive(Clone)]
pub struct Thread(pub(crate) ValueRef, pub(crate) *mut ffi::lua_State);

#[cfg(feature = "send")]
unsafe impl Send for Thread {}
#[cfg(feature = "send")]
unsafe impl Sync for Thread {}

/// Thread (coroutine) representation as an async [`Future`] or [`Stream`].
///
/// [`Future`]: std::future::Future
/// [`Stream`]: futures_util::stream::Stream
#[cfg(feature = "async")]
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct AsyncThread<R> {
    thread: Thread,
    ret: PhantomData<fn() -> R>,
    recycle: bool,
    resume_nargs: Option<c_int>,
    wake_debt: Arc<AtomicBool>,
    wake_route: Weak<FutureWake>,
}

impl Thread {
    /// Returns reference to the Lua state that this thread is associated with.
    #[doc(hidden)]
    #[inline(always)]
    pub fn state(&self) -> *mut ffi::lua_State {
        self.1
    }

    /// Resumes execution of this thread.
    ///
    /// Equivalent to [`coroutine.resume`].
    ///
    /// Passes `args` as arguments to the thread. If the coroutine has called [`coroutine.yield`],
    /// it will return these arguments. Otherwise, the coroutine wasn't yet started, so the
    /// arguments are passed to its main function.
    ///
    /// If the thread is no longer resumable (meaning it has finished execution or encountered an
    /// error), this will return [`Error::CoroutineUnresumable`], otherwise will return `Ok` as
    /// follows:
    ///
    /// If the thread calls [`coroutine.yield`], returns the values passed to `yield`. If the thread
    /// `return`s values from its main function, returns those.
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Error, Lua, Result, Thread};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let thread: Thread = lua.load(r#"
    ///     coroutine.create(function(arg)
    ///         assert(arg == 42)
    ///         local yieldarg = coroutine.yield(123)
    ///         assert(yieldarg == 43)
    ///         return 987
    ///     end)
    /// "#).eval()?;
    ///
    /// assert_eq!(thread.resume::<u32>(42)?, 123);
    /// assert_eq!(thread.resume::<u32>(43)?, 987);
    ///
    /// // The coroutine has now returned, so `resume` will fail
    /// match thread.resume::<u32>(()) {
    ///     Err(Error::CoroutineUnresumable) => {},
    ///     unexpected => panic!("unexpected result {:?}", unexpected),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`coroutine.resume`]: https://www.lua.org/manual/5.4/manual.html#pdf-coroutine.resume
    /// [`coroutine.yield`]: https://www.lua.org/manual/5.4/manual.html#pdf-coroutine.yield
    pub fn resume<R>(&self, args: impl IntoLuaMulti) -> Result<R>
    where
        R: FromLuaMulti,
    {
        let lua = self.0.lua.lock();
        let mut pushed_nargs = match self.status_inner(&lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => return Err(Error::CoroutineUnresumable),
        };

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);

            let nargs = args.push_into_stack_multi(&lua)?;
            if nargs > 0 {
                check_stack(thread_state, nargs)?;
                ffi::lua_xmove(state, thread_state, nargs);
                pushed_nargs += nargs;
            }

            let _thread_sg = StackGuard::with_top(thread_state, 0);
            let (_, nresults) = self.resume_inner(&lua, pushed_nargs)?;
            check_stack(state, nresults + 1)?;
            ffi::lua_xmove(thread_state, state, nresults);

            R::from_stack_multi(nresults, &lua)
        }
    }

    /// Resumes execution of this thread, immediately raising an error.
    ///
    /// This is a Luau specific extension.
    #[cfg(feature = "luau")]
    #[cfg_attr(docsrs, doc(cfg(feature = "luau")))]
    pub fn resume_error<R>(&self, error: impl crate::IntoLua) -> Result<R>
    where
        R: FromLuaMulti,
    {
        let lua = self.0.lua.lock();
        match self.status_inner(&lua) {
            ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_) => {}
            _ => return Err(Error::CoroutineUnresumable),
        };

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);

            check_stack(state, 1)?;
            error.push_into_stack(&lua)?;
            ffi::lua_xmove(state, thread_state, 1);

            let _thread_sg = StackGuard::with_top(thread_state, 0);
            let (_, nresults) = self.resume_inner(&lua, ffi::LUA_RESUMEERROR)?;
            check_stack(state, nresults + 1)?;
            ffi::lua_xmove(thread_state, state, nresults);

            R::from_stack_multi(nresults, &lua)
        }
    }

    /// Resumes execution of this thread.
    ///
    /// It's similar to `resume()` but leaves `nresults` values on the thread stack.
    unsafe fn resume_inner(&self, lua: &RawLua, nargs: c_int) -> Result<(ThreadStatusInner, c_int)> {
        let state = lua.state();
        let thread_state = self.state();
        let mut nresults = 0;
        #[cfg(not(feature = "luau"))]
        let ret = ffi::lua_resume(thread_state, state, nargs, &mut nresults as *mut c_int);
        #[cfg(feature = "luau")]
        let ret = ffi::lua_resumex(thread_state, state, nargs, &mut nresults as *mut c_int);
        match ret {
            ffi::LUA_OK => Ok((ThreadStatusInner::Finished, nresults)),
            ffi::LUA_YIELD => Ok((ThreadStatusInner::Yielded(0), nresults)),
            ffi::LUA_ERRMEM => {
                // Don't call error handler for memory errors
                Err(pop_error(thread_state, ret))
            }
            _ => {
                check_stack(state, 3)?;
                protect_lua!(state, 0, 1, |state| error_traceback_thread(state, thread_state))?;
                Err(pop_error(state, ret))
            }
        }
    }

    /// Gets the status of the thread.
    pub fn status(&self) -> ThreadStatus {
        match self.status_inner(&self.0.lua.lock()) {
            ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_) => ThreadStatus::Resumable,
            ThreadStatusInner::Running => ThreadStatus::Running,
            ThreadStatusInner::Finished => ThreadStatus::Finished,
            ThreadStatusInner::Error => ThreadStatus::Error,
        }
    }

    /// Gets the status of the thread (internal implementation).
    fn status_inner(&self, lua: &RawLua) -> ThreadStatusInner {
        let thread_state = self.state();
        if thread_state == lua.state() {
            // The thread is currently running
            return ThreadStatusInner::Running;
        }
        let status = unsafe { ffi::lua_status(thread_state) };
        let top = unsafe { ffi::lua_gettop(thread_state) };
        match status {
            ffi::LUA_YIELD => ThreadStatusInner::Yielded(top),
            ffi::LUA_OK if top > 0 => ThreadStatusInner::New(top - 1),
            ffi::LUA_OK => ThreadStatusInner::Finished,
            _ => ThreadStatusInner::Error,
        }
    }

    /// Sets a hook function that will periodically be called as Lua code executes.
    ///
    /// This function is similar or [`Lua::set_hook`] except that it sets for the thread.
    /// You can have multiple hooks for different threads.
    ///
    /// To remove a hook call [`Thread::remove_hook`].
    ///
    /// [`Lua::set_hook`]: crate::Lua::set_hook
    #[cfg(not(feature = "luau"))]
    #[cfg_attr(docsrs, doc(cfg(not(feature = "luau"))))]
    pub fn set_hook<F>(&self, triggers: HookTriggers, callback: F) -> Result<()>
    where
        F: Fn(&crate::Lua, &Debug) -> Result<crate::VmState> + crate::MaybeSend + 'static,
    {
        let lua = self.0.lua.lock();
        unsafe {
            lua.set_thread_hook(
                self.state(),
                HookKind::Thread(triggers, crate::types::XRc::new(callback)),
            )
        }
    }

    /// Removes any hook function from this thread.
    #[cfg(not(feature = "luau"))]
    #[cfg_attr(docsrs, doc(cfg(not(feature = "luau"))))]
    pub fn remove_hook(&self) {
        let _lua = self.0.lua.lock();
        unsafe {
            ffi::lua_sethook(self.state(), None, 0, 0);
        }
    }

    /// Resets a thread
    ///
    /// In [Lua 5.4]: cleans its call stack and closes all pending to-be-closed variables.
    /// Returns a error in case of either the original error that stopped the thread or errors
    /// in closing methods.
    ///
    /// In Luau: resets to the initial state of a newly created Lua thread.
    /// Lua threads in arbitrary states (like yielded or errored) can be reset properly.
    ///
    /// Other Lua versions can reset only new or finished threads.
    ///
    /// Sets a Lua function for the thread afterwards.
    ///
    /// [Lua 5.4]: https://www.lua.org/manual/5.4/manual.html#lua_closethread
    pub fn reset(&self, func: Function) -> Result<()> {
        let lua = self.0.lua.lock();
        let thread_state = self.state();
        unsafe {
            let status = self.status_inner(&lua);
            self.reset_inner(status)?;

            // Push function to the top of the thread stack
            ffi::lua_xpush(lua.ref_thread(), thread_state, func.0.index);

            #[cfg(feature = "luau")]
            {
                // Inherit `LUA_GLOBALSINDEX` from the main thread
                ffi::lua_xpush(lua.main_state(), thread_state, ffi::LUA_GLOBALSINDEX);
                ffi::lua_replace(thread_state, ffi::LUA_GLOBALSINDEX);
            }

            Ok(())
        }
    }

    unsafe fn reset_inner(&self, status: ThreadStatusInner) -> Result<()> {
        match status {
            ThreadStatusInner::New(_) => {
                // The thread is new, so we can just set the top to 0
                ffi::lua_settop(self.state(), 0);
                Ok(())
            }
            ThreadStatusInner::Running => Err(Error::runtime("cannot reset a running thread")),
            ThreadStatusInner::Finished => Ok(()),
            #[cfg(not(any(feature = "lua55", feature = "lua54", feature = "luau")))]
            ThreadStatusInner::Yielded(_) | ThreadStatusInner::Error => {
                Err(Error::runtime("cannot reset non-finished thread"))
            }
            #[cfg(any(feature = "lua55", feature = "lua54", feature = "luau"))]
            ThreadStatusInner::Yielded(_) | ThreadStatusInner::Error => {
                let thread_state = self.state();

                #[cfg(all(feature = "lua54", not(feature = "vendored")))]
                let status = ffi::lua_resetthread(thread_state);
                #[cfg(any(feature = "lua55", all(feature = "lua54", feature = "vendored")))]
                let status = {
                    let lua = self.0.lua.lock();
                    ffi::lua_closethread(thread_state, lua.state())
                };
                #[cfg(any(feature = "lua55", feature = "lua54"))]
                if status != ffi::LUA_OK {
                    return Err(pop_error(thread_state, status));
                }
                #[cfg(feature = "luau")]
                ffi::lua_resetthread(thread_state);

                Ok(())
            }
        }
    }

    /// Converts [`Thread`] to an [`AsyncThread`] which implements [`Future`] and [`Stream`] traits.
    ///
    /// Only resumable threads can be converted to [`AsyncThread`].
    ///
    /// `args` are pushed to the thread stack and will be used when the thread is resumed.
    /// The object calls [`resume`] while polling and also allow to run Rust futures
    /// to completion using an executor.
    ///
    /// Using [`AsyncThread`] as a [`Stream`] allow to iterate through [`coroutine.yield`]
    /// values whereas [`Future`] version discards that values and poll until the final
    /// one (returned from the thread function).
    ///
    /// [`Future`]: std::future::Future
    /// [`Stream`]: futures_util::stream::Stream
    /// [`resume`]: https://www.lua.org/manual/5.4/manual.html#lua_resume
    /// [`coroutine.yield`]: https://www.lua.org/manual/5.4/manual.html#pdf-coroutine.yield
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Lua, Result, Thread};
    /// use futures_util::stream::TryStreamExt;
    /// # #[tokio::main]
    /// # async fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let thread: Thread = lua.load(r#"
    ///     coroutine.create(function (sum)
    ///         for i = 1,10 do
    ///             sum = sum + i
    ///             coroutine.yield(sum)
    ///         end
    ///         return sum
    ///     end)
    /// "#).eval()?;
    ///
    /// let mut stream = thread.into_async::<i64>(1)?;
    /// let mut sum = 0;
    /// while let Some(n) = stream.try_next().await? {
    ///     sum += n;
    /// }
    ///
    /// assert_eq!(sum, 286);
    ///
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub fn into_async<R>(self, args: impl IntoLuaMulti) -> Result<AsyncThread<R>>
    where
        R: FromLuaMulti,
    {
        let lua = self.0.lua.lock();
        if !self.status_inner(&lua).is_resumable() {
            return Err(Error::CoroutineUnresumable);
        }

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);

            let nargs = args.push_into_stack_multi(&lua)?;
            if nargs > 0 {
                check_stack(thread_state, nargs)?;
                ffi::lua_xmove(state, thread_state, nargs);
            }

            Ok(AsyncThread {
                thread: self,
                ret: PhantomData,
                recycle: false,
                resume_nargs: None,
                wake_debt: Arc::new(AtomicBool::new(false)),
                wake_route: Weak::new(),
            })
        }
    }

    /// Enables sandbox mode on this thread.
    ///
    /// Under the hood replaces the global environment table with a new table,
    /// that performs writes locally and proxies reads to caller's global environment.
    ///
    /// This mode ideally should be used together with the global sandbox mode [`Lua::sandbox`].
    ///
    /// Please note that Luau links environment table with chunk when loading it into Lua state.
    /// Therefore you need to load chunks into a thread to link with the thread environment.
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Lua, Result};
    /// # #[cfg(feature = "luau")]
    /// # fn main() -> Result<()> {
    /// let lua = Lua::new();
    /// let thread = lua.create_thread(lua.create_function(|lua2, ()| {
    ///     lua2.load("var = 123").exec()?;
    ///     assert_eq!(lua2.globals().get::<u32>("var")?, 123);
    ///     Ok(())
    /// })?)?;
    /// thread.sandbox()?;
    /// thread.resume::<()>(())?;
    ///
    /// // The global environment should be unchanged
    /// assert_eq!(lua.globals().get::<Option<u32>>("var")?, None);
    /// # Ok(())
    /// # }
    ///
    /// # #[cfg(not(feature = "luau"))]
    /// # fn main() { }
    /// ```
    #[cfg(any(feature = "luau", doc))]
    #[cfg_attr(docsrs, doc(cfg(feature = "luau")))]
    pub fn sandbox(&self) -> Result<()> {
        let lua = self.0.lua.lock();
        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            check_stack(thread_state, 3)?;
            check_stack(state, 3)?;
            protect_lua!(state, 0, 0, |_| ffi::luaL_sandboxthread(thread_state))
        }
    }

    /// Converts this thread to a generic C pointer.
    ///
    /// There is no way to convert the pointer back to its original value.
    ///
    /// Typically this function is used only for hashing and debug information.
    #[inline]
    pub fn to_pointer(&self) -> *const c_void {
        self.0.to_pointer()
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_tuple("Thread").field(&self.0).finish()
    }
}

impl PartialEq for Thread {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl LuaType for Thread {
    const TYPE_ID: c_int = ffi::LUA_TTHREAD;
}

#[cfg(feature = "async")]
impl<R> AsyncThread<R> {
    fn clear_wake_route(&self) {
        if let Some(route) = self.wake_route.upgrade() {
            let previous = route.caller.lock().take();
            drop(previous);
        }
    }

    #[inline(always)]
    pub(crate) fn set_recyclable(&mut self, recyclable: bool) {
        self.recycle = recyclable;
    }
}

#[cfg(feature = "async")]
impl<R> Drop for AsyncThread<R> {
    fn drop(&mut self) {
        self.clear_wake_route();
        if self.recycle {
            if let Some(lua) = self.thread.0.lua.try_lock() {
                unsafe {
                    let mut status = self.thread.status_inner(&lua);
                    if matches!(status, ThreadStatusInner::Yielded(0)) {
                        // The thread is dropped while yielded, resume it with the "terminate" signal
                        ffi::lua_pushlightuserdata(self.thread.1, crate::Lua::poll_terminate().0);
                        if let Ok((new_status, _)) = self.thread.resume_inner(&lua, 1) {
                            // `new_status` should always be `ThreadStatusInner::Yielded(0)`
                            status = new_status;
                        }
                    }

                    // For Lua 5.4 this also closes all pending to-be-closed variables
                    if self.thread.reset_inner(status).is_ok() {
                        lua.recycle_thread(&mut self.thread);
                    }
                }
            }
        }
    }
}

#[cfg(feature = "async")]
impl<R: FromLuaMulti> Stream for AsyncThread<R> {
    type Item = Result<R>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let lua = this.thread.0.lua.lock();
        let nargs = match this.thread.status_inner(&lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => { this.clear_wake_route(); return Poll::Ready(None); },
        };

        // A retained hook-suspended frame is not a list of resume arguments.
        let nargs = this.resume_nargs.take().unwrap_or(nargs);
        let state = lua.state();
        let thread_state = this.thread.state();
        unsafe {
            let _sg = StackGuard::new(state);
            let mut thread_sg = StackGuard::with_top(thread_state, 0);
            let route = match this.wake_route.upgrade() {
                Some(route) => route,
                None => {
                    let route = Arc::new(FutureWake {
                        caller: parking_lot::Mutex::new(Some(Arc::new(cx.waker().clone()))),
                        debt: this.wake_debt.clone(),
                    });
                    this.wake_route = Arc::downgrade(&route);
                    route
                }
            };
            let caller = Arc::new(cx.waker().clone());
            let previous = std::mem::replace(&mut *route.caller.lock(), Some(caller));
            drop(previous);
            let proxy = Waker::from(route);
            let _wg = WakerGuard::new(&lua, &proxy, this.wake_debt.clone());

            let (status, nresults) = (this.thread).resume_inner(&lua, nargs).map_err(|error| { this.clear_wake_route(); error })?;

            if status.is_yielded() {
                this.resume_nargs = Some(0);
                if nresults == 0 {
                    thread_sg.keep(ffi::lua_gettop(thread_state));
                }
                if nresults == 1 && is_poll_pending(thread_state) {
                    if this.wake_debt.load(Ordering::SeqCst) {
                        cx.waker().wake_by_ref();
                    }
                    return Poll::Pending;
                }
                // Continue polling
                cx.waker().wake_by_ref();
            }

            if !status.is_yielded() {
                this.clear_wake_route();
            }
            check_stack(state, nresults + 1).map_err(|error| { this.clear_wake_route(); error })?;
            ffi::lua_xmove(thread_state, state, nresults);

            let result = R::from_stack_multi(nresults, &lua);
            if result.is_err() { this.clear_wake_route(); }
            Poll::Ready(Some(result))
        }
    }
}

#[cfg(feature = "async")]
impl<R: FromLuaMulti> Future for AsyncThread<R> {
    type Output = Result<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let lua = this.thread.0.lua.lock();
        let nargs = match this.thread.status_inner(&lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => { this.clear_wake_route(); return Poll::Ready(Err(Error::CoroutineUnresumable)); },
        };

        // A retained hook-suspended frame is not a list of resume arguments.
        let nargs = this.resume_nargs.take().unwrap_or(nargs);
        let state = lua.state();
        let thread_state = this.thread.state();
        unsafe {
            let _sg = StackGuard::new(state);
            let mut thread_sg = StackGuard::with_top(thread_state, 0);
            let route = match this.wake_route.upgrade() {
                Some(route) => route,
                None => {
                    let route = Arc::new(FutureWake {
                        caller: parking_lot::Mutex::new(Some(Arc::new(cx.waker().clone()))),
                        debt: this.wake_debt.clone(),
                    });
                    this.wake_route = Arc::downgrade(&route);
                    route
                }
            };
            let caller = Arc::new(cx.waker().clone());
            let previous = std::mem::replace(&mut *route.caller.lock(), Some(caller));
            drop(previous);
            let proxy = Waker::from(route);
            let _wg = WakerGuard::new(&lua, &proxy, this.wake_debt.clone());

            let (status, nresults) = this.thread.resume_inner(&lua, nargs).map_err(|error| { this.clear_wake_route(); error })?;

            if status.is_yielded() {
                this.resume_nargs = Some(0);
                if nresults == 0 {
                    // A count-hook suspension exposes the current Lua frame, not
                    // returned yield values. Keep those locals until resume.
                    thread_sg.keep(ffi::lua_gettop(thread_state));
                }
                if nresults == 1 && is_poll_pending(thread_state) {
                    if this.wake_debt.load(Ordering::SeqCst) {
                        cx.waker().wake_by_ref();
                    }
                } else {
                    // Ignore values returned via yield()
                    cx.waker().wake_by_ref();
                }
                return Poll::Pending;
            }

            if !status.is_yielded() {
                this.clear_wake_route();
            }
            check_stack(state, nresults + 1).map_err(|error| { this.clear_wake_route(); error })?;
            ffi::lua_xmove(thread_state, state, nresults);

            Poll::Ready(R::from_stack_multi(nresults, &lua))
        }
    }
}

#[cfg(feature = "async")]
#[inline(always)]
unsafe fn is_poll_pending(state: *mut ffi::lua_State) -> bool {
    ffi::lua_tolightuserdata(state, -1) == crate::Lua::poll_pending().0
}

#[cfg(feature = "async")]
struct FutureWake {
    caller: parking_lot::Mutex<Option<Arc<Waker>>>,
    debt: Arc<AtomicBool>,
}

#[cfg(feature = "async")]
impl Wake for FutureWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.debt.store(true, Ordering::SeqCst);
        let caller = self.caller.lock().clone();
        if let Some(caller) = caller {
            caller.wake_by_ref();
        }
    }
}

#[cfg(feature = "async")]
struct WakerGuard<'lua, 'a> {
    lua: &'lua RawLua,
    prev: NonNull<Waker>,
    previous_debt: Option<Arc<AtomicBool>>,
    _phantom: PhantomData<&'a ()>,
}

#[cfg(feature = "async")]
impl<'lua, 'a> WakerGuard<'lua, 'a> {
    #[inline]
    pub fn new(lua: &'lua RawLua, waker: &'a Waker, debt: Arc<AtomicBool>) -> Result<WakerGuard<'lua, 'a>> {
        let prev = lua.set_waker(NonNull::from(waker));
        let previous_debt = lua.set_wake_debt(Some(debt));
        Ok(WakerGuard {
            lua,
            prev,
            previous_debt,
            _phantom: PhantomData,
        })
    }
}

#[cfg(feature = "async")]
impl Drop for WakerGuard<'_, '_> {
    fn drop(&mut self) {
        self.lua.set_waker(self.prev);
        self.lua.set_wake_debt(self.previous_debt.take());
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    #[cfg(not(feature = "send"))]
    static_assertions::assert_not_impl_any!(Thread: Send);
    #[cfg(feature = "send")]
    static_assertions::assert_impl_all!(Thread: Send, Sync);
    #[cfg(all(feature = "async", not(feature = "send")))]
    static_assertions::assert_not_impl_any!(AsyncThread<()>: Send);
    #[cfg(all(feature = "async", feature = "send"))]
    static_assertions::assert_impl_all!(AsyncThread<()>: Send, Sync);
}

#[cfg(all(test, feature = "async", any(feature = "lua54", feature = "lua55")))]
mod wake_debt_tests {
    use super::*;
    use crate::Lua;
    use std::sync::atomic::AtomicUsize;

    struct CountWake(AtomicUsize);
    impl Wake for CountWake {
        fn wake(self: Arc<Self>) { self.wake_by_ref(); }
        fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    #[test]
    fn route_migration_forwards_to_latest_parent_and_clears_on_drop() {
        let first = Arc::new(CountWake(AtomicUsize::new(0)));
        let second = Arc::new(CountWake(AtomicUsize::new(0)));
        let debt = Arc::new(AtomicBool::new(false));
        let route = Arc::new(FutureWake { caller: parking_lot::Mutex::new(Some(Arc::new(Waker::from(first.clone())))), debt: debt.clone() });
        let old_proxy = Waker::from(route.clone());
        let parent = Arc::new(Waker::from(second.clone()));
        let previous = std::mem::replace(&mut *route.caller.lock(), Some(parent));
        drop(previous);
        old_proxy.wake_by_ref();
        assert!(debt.load(Ordering::SeqCst));
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
        let previous = route.caller.lock().take();
        drop(previous);
        old_proxy.wake_by_ref();
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn real_wake_debt_before_during_after_ack_is_not_consumed_by_forwarding() {
        let parent = Arc::new(CountWake(AtomicUsize::new(0)));
        let debt = Arc::new(AtomicBool::new(false));
        let route = Arc::new(FutureWake { caller: parking_lot::Mutex::new(Some(Arc::new(Waker::from(parent.clone())))), debt: debt.clone() });
        let wake = Waker::from(route);
        wake.wake_by_ref();
        assert!(debt.load(Ordering::SeqCst));
        debt.store(false, Ordering::SeqCst); // Actual Rust poll acknowledges prior wake.
        assert!(!debt.load(Ordering::SeqCst));
        let concurrent = wake.clone();
        std::thread::spawn(move || concurrent.wake()).join().unwrap();
        assert!(debt.load(Ordering::SeqCst));
        assert_eq!(parent.0.load(Ordering::SeqCst), 2);
        assert!(debt.load(Ordering::SeqCst)); // A conditional handoff does not acknowledge.
        debt.store(false, Ordering::SeqCst);
        wake.wake_by_ref();
        assert!(debt.load(Ordering::SeqCst));
        assert_eq!(parent.0.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn nested_waker_guards_restore_both_bindings() -> crate::Result<()> {
        let lua = Lua::new();
        let raw = lua.lock();
        let initial = raw.waker() as *const Waker;
        let first = Waker::from(Arc::new(CountWake(AtomicUsize::new(0))));
        let second = Waker::from(Arc::new(CountWake(AtomicUsize::new(0))));
        let first_debt = Arc::new(AtomicBool::new(false));
        let second_debt = Arc::new(AtomicBool::new(false));
        {
            let _outer = WakerGuard::new(&raw, &first, first_debt.clone())?;
            assert!(std::ptr::eq(raw.waker(), &first));
            {
                let _inner = WakerGuard::new(&raw, &second, second_debt.clone())?;
                assert!(std::ptr::eq(raw.waker(), &second));
                let bound = raw.set_wake_debt(None);
                assert!(Arc::ptr_eq(bound.as_ref().unwrap(), &second_debt));
                raw.set_wake_debt(bound);
            }
            assert!(std::ptr::eq(raw.waker(), &first));
            let bound = raw.set_wake_debt(None);
            assert!(Arc::ptr_eq(bound.as_ref().unwrap(), &first_debt));
            raw.set_wake_debt(bound);
        }
        assert_eq!(raw.waker() as *const Waker, initial);
        assert!(raw.set_wake_debt(None).is_none());
        Ok(())
    }

    #[test]
    fn never_ready_no_wake_future_does_not_schedule_marker_spin() -> crate::Result<()> {
        let lua = Lua::new();
        lua.globals().set("wait_forever", lua.create_async_function(|_, ()| async {
            std::future::pending::<()>().await;
            Ok(())
        })?)?;
        let function = lua.load("wait_forever()").into_function()?;
        let mut execution = lua.create_thread(function)?.into_async::<()>(())?;
        let parent = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(parent.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut execution).poll(&mut cx).is_pending());
        assert_eq!(parent.0.load(Ordering::SeqCst), 0);
        assert!(!execution.wake_debt.load(Ordering::SeqCst));
        drop(execution);
        assert_eq!(parent.0.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn async_thread_migration_routes_registered_proxy_without_rust_repoll() -> crate::Result<()> {
        struct PendingProbe {
            polls: Arc<AtomicUsize>,
            registered: Arc<parking_lot::Mutex<Option<Waker>>>,
        }
        impl Future for PendingProbe {
            type Output = crate::Result<i64>;
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                self.polls.fetch_add(1, Ordering::SeqCst);
                *self.registered.lock() = Some(cx.waker().clone());
                Poll::Pending
            }
        }
        let lua = Lua::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let registered = Arc::new(parking_lot::Mutex::new(None));
        let callback_polls = polls.clone();
        let callback_registered = registered.clone();
        lua.globals().set("await_tick", lua.create_async_function(move |_, ()| PendingProbe {
            polls: callback_polls.clone(), registered: callback_registered.clone(),
        })?)?;
        let instructions = Arc::new(AtomicUsize::new(0));
        let counted = instructions.clone();
        lua.set_global_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
            let instruction = counted.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(instruction < 100_000);
            Ok(if instruction == 49 { crate::VmState::Yield } else { crate::VmState::Continue })
        })?;
        let function = lua.load(r#"
            local run = {"alpha", "beta", "gamma", "delta"}
            local scalar = 271828
            local function add(term)
                assert(type(term) == "string", "concat result lost across hook yield")
                return term
            end
            local joined = add(table.concat(run))
            assert(joined == "alphabetagammadelta")
            assert(scalar == 271828 and run[3] == "gamma")
            assert(await_tick() == 17)
            assert(table.concat(run) == joined)
            return joined .. ":" .. scalar
        "#).into_function()?;
        let mut execution = lua.create_thread(function)?.into_async::<String>(())?;
        let first = Arc::new(CountWake(AtomicUsize::new(0)));
        let second = Arc::new(CountWake(AtomicUsize::new(0)));
        let first_waker = Waker::from(first.clone());
        let second_waker = Waker::from(second.clone());
        assert!(Pin::new(&mut execution).poll(&mut Context::from_waker(&first_waker)).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 1, "hook must interrupt after Rust registration");
        let old_proxy = registered.lock().as_ref().unwrap().clone();
        assert!(Pin::new(&mut execution).poll(&mut Context::from_waker(&second_waker)).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 1, "migration poll must only advance hook handoff");
        let before_first = first.0.load(Ordering::SeqCst);
        let before_second = second.0.load(Ordering::SeqCst);
        old_proxy.wake_by_ref();
        assert_eq!(first.0.load(Ordering::SeqCst), before_first);
        assert_eq!(second.0.load(Ordering::SeqCst), before_second + 1);
        // Drain only actual notifications, never manually spin a Pending future.
        let mut consumed = before_second;
        let mut drains = 0;
        while second.0.load(Ordering::SeqCst) > consumed {
            consumed = second.0.load(Ordering::SeqCst);
            drains += 1;
            assert!(drains < 16, "never-ready future scheduled indefinitely");
            assert!(Pin::new(&mut execution).poll(&mut Context::from_waker(&second_waker)).is_pending());
        }
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert!(!execution.wake_debt.load(Ordering::SeqCst));
        drop(execution);
        let after_drop = second.0.load(Ordering::SeqCst);
        old_proxy.wake_by_ref();
        assert_eq!(second.0.load(Ordering::SeqCst), after_drop);
        Ok(())
    }

    #[test]
    fn concurrent_route_registration_and_real_wake_forward_once() {
        for _ in 0..64 {
            let first = Arc::new(CountWake(AtomicUsize::new(0)));
            let second = Arc::new(CountWake(AtomicUsize::new(0)));
            let debt = Arc::new(AtomicBool::new(false));
            let route = Arc::new(FutureWake { caller: parking_lot::Mutex::new(Some(Arc::new(Waker::from(first.clone())))), debt: debt.clone() });
            let gate = Arc::new(std::sync::Barrier::new(2));
            let notifying_route = route.clone();
            let notifying_gate = gate.clone();
            let notifier = std::thread::spawn(move || {
                notifying_gate.wait();
                Waker::from(notifying_route).wake();
            });
            let latest = Arc::new(Waker::from(second.clone()));
            gate.wait();
            let previous = std::mem::replace(&mut *route.caller.lock(), Some(latest));
            drop(previous);
            notifier.join().unwrap();
            assert!(debt.load(Ordering::SeqCst));
            assert_eq!(first.0.load(Ordering::SeqCst) + second.0.load(Ordering::SeqCst), 1);
            // Once registration finished, every old proxy must reach latest parent.
            let before = second.0.load(Ordering::SeqCst);
            Waker::from(route).wake();
            assert_eq!(second.0.load(Ordering::SeqCst), before + 1);
        }
    }

    #[test]
    fn actual_rust_poll_ack_preserves_concurrent_notification() -> crate::Result<()> {
        struct RacingPending {
            polls: Arc<AtomicUsize>,
            registered: Arc<parking_lot::Mutex<Option<Waker>>>,
        }
        impl Future for RacingPending {
            type Output = crate::Result<()>;
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let poll = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
                *self.registered.lock() = Some(cx.waker().clone());
                if poll == 1 {
                    // raw.rs has acknowledged debt before entering this actual poll.
                    let gate = Arc::new(std::sync::Barrier::new(2));
                    let notify_gate = gate.clone();
                    let notify = cx.waker().clone();
                    let notifier = std::thread::spawn(move || {
                        notify_gate.wait();
                        notify.wake();
                    });
                    gate.wait();
                    notifier.join().unwrap();
                }
                Poll::Pending
            }
        }
        let lua = Lua::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let registered = Arc::new(parking_lot::Mutex::new(None));
        let callback_polls = polls.clone();
        let callback_registered = registered.clone();
        lua.globals().set("racing_pending", lua.create_async_function(move |_, ()| RacingPending {
            polls: callback_polls.clone(), registered: callback_registered.clone(),
        })?)?;
        let function = lua.load("racing_pending()").into_function()?;
        let mut execution = lua.create_thread(function)?.into_async::<()>(())?;
        let parent = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(parent.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut execution).poll(&mut cx).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert!(execution.wake_debt.load(Ordering::SeqCst), "wake during actual poll must survive raw ack");
        assert!(parent.0.load(Ordering::SeqCst) >= 1);
        // Consume the real notification: actual Rust poll now acknowledges it.
        assert!(Pin::new(&mut execution).poll(&mut cx).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert!(!execution.wake_debt.load(Ordering::SeqCst));
        let count = parent.0.load(Ordering::SeqCst);
        // An after-poll wake is retained until the next actual poll.
        let registered = registered.lock().as_ref().unwrap().clone();
        registered.wake_by_ref();
        assert_eq!(parent.0.load(Ordering::SeqCst), count + 1);
        assert!(execution.wake_debt.load(Ordering::SeqCst));
        assert!(Pin::new(&mut execution).poll(&mut cx).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        assert!(!execution.wake_debt.load(Ordering::SeqCst));
        Ok(())
    }
}
