#![cfg(not(feature = "luau"))]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use mlua::{DebugEvent, Error, HookTriggers, Lua, Result, ThreadStatus, Value, VmState};

#[test]
fn test_hook_triggers() {
    let trigger = HookTriggers::new().on_calls().on_returns()
        | HookTriggers::new().every_line().every_nth_instruction(5);

    assert!(trigger.on_calls);
    assert!(trigger.on_returns);
    assert!(trigger.every_line);
    assert_eq!(trigger.every_nth_instruction, Some(5));
}

#[test]
fn test_line_counts() -> Result<()> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let hook_output = output.clone();

    let lua = Lua::new();
    lua.set_hook(HookTriggers::EVERY_LINE, move |_lua, debug| {
        assert_eq!(debug.event(), DebugEvent::Line);
        hook_output.lock().unwrap().push(debug.current_line().unwrap());
        Ok(VmState::Continue)
    })?;
    lua.load(
        r#"
            local x = 2 + 3
            local y = x * 63
            local z = string.len(x..", "..y)
        "#,
    )
    .exec()?;

    lua.remove_hook();

    let output = output.lock().unwrap();
    if cfg!(feature = "luajit") && lua.load("jit.version_num").eval::<i64>()? >= 20100 {
        assert_eq!(*output, vec![2, 3, 4, 0, 4]);
    } else {
        assert_eq!(*output, vec![2, 3, 4]);
    }

    Ok(())
}

#[test]
fn test_function_calls() -> Result<()> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let hook_output = output.clone();

    let lua = Lua::new();
    lua.set_hook(HookTriggers::ON_CALLS, move |_lua, debug| {
        assert_eq!(debug.event(), DebugEvent::Call);
        let names = debug.names();
        let source = debug.source();
        let name = names.name.map(|s| s.into_owned());
        hook_output.lock().unwrap().push((name, source.what));
        Ok(VmState::Continue)
    })?;

    lua.load(
        r#"
            local v = string.len("Hello World")
        "#,
    )
    .exec()?;

    lua.remove_hook();

    let output = output.lock().unwrap();
    if cfg!(feature = "luajit") && lua.load("jit.version_num").eval::<i64>()? >= 20100 {
        #[cfg(not(force_memory_limit))]
        assert_eq!(*output, vec![(None, "main"), (Some("len".to_string()), "Lua")]);
        #[cfg(force_memory_limit)]
        assert_eq!(
            *output,
            vec![(None, "C"), (None, "main"), (Some("len".to_string()), "Lua")]
        );
    } else {
        #[cfg(not(force_memory_limit))]
        assert_eq!(*output, vec![(None, "main"), (Some("len".to_string()), "C")]);
        #[cfg(force_memory_limit)]
        assert_eq!(
            *output,
            vec![(None, "C"), (None, "main"), (Some("len".to_string()), "C")]
        );
    }

    Ok(())
}

#[test]
fn test_error_within_hook() -> Result<()> {
    let lua = Lua::new();

    lua.set_hook(HookTriggers::EVERY_LINE, |_lua, _debug| {
        Err(Error::runtime("Something happened in there!"))
    })?;

    let err = lua.load("x = 1").exec().expect_err("panic didn't propagate");
    match err {
        Error::RuntimeError(msg) => assert_eq!(msg, "Something happened in there!"),
        err => panic!("expected `RuntimeError` with a specific message, got {err:?}"),
    }

    Ok(())
}

#[test]
fn test_limit_execution_instructions() -> Result<()> {
    let lua = Lua::new();

    // For LuaJIT disable JIT, as compiled code does not trigger hooks
    #[cfg(feature = "luajit")]
    lua.load("jit.off()").exec()?;

    let max_instructions = AtomicI64::new(10000);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(30),
        move |_lua, debug| {
            assert_eq!(debug.event(), DebugEvent::Count);
            if max_instructions.fetch_sub(30, Ordering::Relaxed) <= 30 {
                Err(Error::runtime("time's up"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )?;

    lua.globals().set("x", Value::Integer(0))?;
    let _ = lua
        .load(
            r#"
                for i = 1, 10000 do
                    x = x + 1
                end
            "#,
        )
        .exec()
        .expect_err("instruction limit didn't occur");

    Ok(())
}

#[test]
fn test_hook_removal() -> Result<()> {
    let lua = Lua::new();

    lua.set_hook(HookTriggers::new().every_nth_instruction(1), |_lua, _debug| {
        Err(Error::runtime("this hook should've been removed by this time"))
    })?;

    assert!(lua.load("local x = 1").exec().is_err());
    lua.remove_hook();
    assert!(lua.load("local x = 1").exec().is_ok());

    Ok(())
}

// Having the code compiled (even not run) on macos and luajit causes a memory reference issue
// See https://github.com/LuaJIT/LuaJIT/issues/1099
#[cfg(not(all(feature = "luajit", target_os = "macos")))]
#[test]
fn test_hook_swap_within_hook() -> Result<()> {
    thread_local! {
        static TL_LUA: std::cell::RefCell<Option<Lua>> = Default::default();
    }

    TL_LUA.with(|tl| {
        *tl.borrow_mut() = Some(Lua::new());
    });

    TL_LUA.with(|tl| {
        tl.borrow()
            .as_ref()
            .unwrap()
            .set_hook(HookTriggers::EVERY_LINE, move |lua, _debug| {
                lua.globals().set("ok", 1i64)?;
                TL_LUA.with(|tl| {
                    tl.borrow()
                        .as_ref()
                        .unwrap()
                        .set_hook(HookTriggers::EVERY_LINE, move |lua, _debug| {
                            lua.load(
                                r#"
                                    if ok ~= nil then
                                        ok = ok + 1
                                    end
                                "#,
                            )
                            .exec()
                            .expect("exec failure within hook");
                            TL_LUA.with(|tl| {
                                tl.borrow().as_ref().unwrap().remove_hook();
                            });
                            Ok(VmState::Continue)
                        })
                })?;
                Ok(VmState::Continue)
            })
    })?;

    TL_LUA.with(|tl| {
        let tl = tl.borrow();
        let lua = tl.as_ref().unwrap();
        lua.load(
            r#"
                local x = 1
                x = 2
                local y = 3
            "#,
        )
        .exec()?;
        assert_eq!(lua.globals().get::<i64>("ok")?, 2);
        Ok(())
    })
}

#[test]
fn test_hook_threads() -> Result<()> {
    let lua = Lua::new();

    let func = lua
        .load(
            r#"
            local x = 2 + 3
            local y = x * 63
            local z = string.len(x..", "..y)
        "#,
        )
        .into_function()?;
    let co = lua.create_thread(func)?;

    let output = Arc::new(Mutex::new(Vec::new()));
    let hook_output = output.clone();
    co.set_hook(HookTriggers::EVERY_LINE, move |_lua, debug| {
        assert_eq!(debug.event(), DebugEvent::Line);
        hook_output.lock().unwrap().push(debug.current_line().unwrap());
        Ok(VmState::Continue)
    })?;

    co.resume::<()>(())?;
    lua.remove_hook();

    let output = output.lock().unwrap();
    if cfg!(feature = "luajit") && lua.load("jit.version_num").eval::<i64>()? >= 20100 {
        assert_eq!(*output, vec![2, 3, 4, 0, 4]);
    } else {
        assert_eq!(*output, vec![2, 3, 4]);
    }

    Ok(())
}

#[test]
fn test_hook_yield() -> Result<()> {
    let lua = Lua::new();

    let func = lua
        .load(
            r#"
            local x = 2 + 3
            local y = x * 63
            local z = string.len(x..", "..y)
        "#,
        )
        .into_function()?;
    let co = lua.create_thread(func)?;

    co.set_hook(HookTriggers::EVERY_LINE, move |_lua, _debug| Ok(VmState::Yield))?;

    #[cfg(any(feature = "lua55", feature = "lua54", feature = "lua53"))]
    {
        assert!(co.resume::<()>(()).is_ok());
        assert!(co.resume::<()>(()).is_ok());
        assert!(co.resume::<()>(()).is_ok());
        assert!(co.resume::<()>(()).is_ok());
        assert!(co.status() == ThreadStatus::Finished);
    }
    #[cfg(any(feature = "lua51", feature = "lua52", feature = "luajit"))]
    {
        assert!(
            matches!(co.resume::<()>(()), Err(Error::RuntimeError(err)) if err.contains("attempt to yield from a hook"))
        );
        assert!(co.status() == ThreadStatus::Error);
    }

    Ok(())
}

#[test]
fn test_global_hook() -> Result<()> {
    let lua = Lua::new();

    let counter = Arc::new(AtomicI64::new(0));
    let hook_counter = counter.clone();
    lua.set_global_hook(HookTriggers::EVERY_LINE, move |_lua, debug| {
        assert_eq!(debug.event(), DebugEvent::Line);
        hook_counter.fetch_add(1, Ordering::Relaxed);
        Ok(VmState::Continue)
    })?;

    let thread = lua.create_thread(
        lua.load(
            r#"
            local x = 2 + 3
            local y = x * 63
            coroutine.yield()
            local z = string.len(x..", "..y)
        "#,
        )
        .into_function()?,
    )?;

    thread.resume::<()>(()).unwrap();
    lua.remove_global_hook();
    thread.resume::<()>(()).unwrap();
    assert_eq!(thread.status(), ThreadStatus::Finished);
    assert_eq!(counter.load(Ordering::Relaxed), 3);

    Ok(())
}

// Sweep each instruction boundary rather than relying on scheduler timing.
#[cfg(all(feature = "async", any(feature = "lua54", feature = "lua55")))]
#[tokio::test]
async fn zero_result_hook_yields_preserve_concat_arguments_and_frame_locals() -> Result<()> {
    use std::sync::atomic::AtomicUsize;
    use futures_util::StreamExt;
    const SCRIPT: &str = r#"
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
    "#;
    for stream in [false, true] {
        for boundary in 1..=256 {
            let started = std::time::Instant::now();
            eprintln!("hook-arity start stream={stream} boundary={boundary}");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let lua = Lua::new();
            lua.globals().set("await_tick", lua.create_async_function(|_, ()| async {
                eprintln!("hook-arity async-C enter");
                tokio::task::yield_now().await;
                eprintln!("hook-arity async-C return");
                Ok(17)
            })?)?;
            let instruction = Arc::new(AtomicUsize::new(0));
            let observed = instruction.clone();
            lua.set_global_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
                let current = observed.fetch_add(1, Ordering::Relaxed) + 1;
                if current > 100_000 {
                    return Err(Error::RuntimeError(format!("instruction non-progress stream={stream} boundary={boundary} count={current}")));
                }
                Ok(if current == boundary { VmState::Yield } else { VmState::Continue })
            })?;
            let function = lua.load(SCRIPT).into_function()?;
            let thread = lua.create_thread(function)?;
            if stream {
                let mut execution = thread.into_async::<mlua::MultiValue>(())?;
                let mut final_text = None;
                while let Some(row) = execution.next().await {
                    let row = row?;
                    if let Some(Value::String(text)) = row.front() {
                        final_text = Some(text.to_str()?.to_string());
                    } else {
                        assert!(row.is_empty(), "unexpected hook/async yield payload");
                    }
                }
                assert_eq!(final_text.as_deref(), Some("alphabetagammadelta:271828"), "stream boundary {boundary}");
            } else {
                let result: String = thread.into_async(())?.await?;
                assert_eq!(result, "alphabetagammadelta:271828", "future boundary {boundary}");
            }
            Ok::<(), Error>(())
            }).await.unwrap_or_else(|_| panic!("hook-arity deadline stream={stream} boundary={boundary}"))?;
            assert!(started.elapsed() < std::time::Duration::from_secs(1), "timer-rescued hook boundary stream={stream} boundary={boundary}");
            eprintln!("hook-arity complete stream={stream} boundary={boundary}");
        }
    }
    Ok(())
}

#[cfg(all(feature = "async", any(feature = "lua54", feature = "lua55")))]
#[tokio::test]
async fn async_stream_keeps_ordinary_yield_payloads_and_initial_arguments() -> Result<()> {
    use futures_util::StreamExt;
    let lua = Lua::new();
    let function = lua.load(r#"
        local initial = ...
        assert(initial == 42)
        local resumed = coroutine.yield("first", initial)
        assert(resumed == nil)
        coroutine.yield()
        return "last", initial + 1
    "#).into_function()?;
    let mut stream = lua.create_thread(function)?.into_async::<mlua::MultiValue>(42)?;
    let first = stream.next().await.unwrap()?;
    assert_eq!(first.len(), 2);
    assert!(matches!(first.get(1), Some(Value::Integer(42))));
    assert!(stream.next().await.unwrap()?.is_empty());
    let last = stream.next().await.unwrap()?;
    assert_eq!(last.len(), 2);
    assert!(matches!(last.get(1), Some(Value::Integer(43))));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[cfg(all(feature = "async", any(feature = "lua54", feature = "lua55")))]
#[tokio::test]
async fn async_c_yield_preserves_initial_arguments_and_return_values() -> Result<()> {
    let lua = Lua::new();
    lua.globals().set("await_echo", lua.create_async_function(|_, value: i64| async move {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        Ok((value, "echo"))
    })?)?;
    let function = lua.load(r#"
        local initial = ...
        assert(initial == 42)
        local number, text = await_echo(initial)
        assert(number == initial and text == "echo")
        return number + 1
    "#).into_function()?;
    let answer: i64 = lua.create_thread(function)?.into_async(42)?.await?;
    assert_eq!(answer, 43);
    Ok(())
}

#[cfg(all(feature = "async", any(feature = "lua54", feature = "lua55")))]
#[tokio::test]
async fn counted_hook_async_handoff_timeline() -> Result<()> {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll, Waker};
    use futures_util::task::{ArcWake, waker_ref};
    struct TraceWake { target: Waker, generation: Arc<AtomicUsize> }
    impl ArcWake for TraceWake {
        fn wake_by_ref(this: &Arc<Self>) {
            let generation = this.generation.fetch_add(1, Ordering::SeqCst) + 1;
            eprintln!("timeline WAKE generation={generation}");
            this.target.wake_by_ref();
        }
    }
    struct OnePending { polled: bool, count: Arc<AtomicUsize> }
    impl Future for OnePending {
        type Output = Result<i64>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<i64>> {
            let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
            eprintln!("timeline C_POLL count={count}");
            if self.polled { return Poll::Ready(Ok(17)); }
            self.polled = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
    for boundary in [49usize, 50, 51] {
        let started = std::time::Instant::now();
        eprintln!("timeline START boundary={boundary}");
        let lua = Lua::new();
        let c_polls = Arc::new(AtomicUsize::new(0));
        let callback_polls = c_polls.clone();
        lua.globals().set("await_tick", lua.create_async_function(move |_, ()| {
            OnePending { polled: false, count: callback_polls.clone() }
        })?)?;
        let instructions = Arc::new(AtomicUsize::new(0));
        let counted = instructions.clone();
        lua.set_global_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
            let instruction = counted.fetch_add(1, Ordering::SeqCst) + 1;
            if instruction > 100_000 { return Err(Error::RuntimeError("timeline instruction cap".into())); }
            if instruction == boundary {
                eprintln!("timeline HOOK_YIELD boundary={boundary}");
                return Ok(VmState::Yield);
            }
            Ok(VmState::Continue)
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
        let execution = lua.create_thread(function)?.into_async::<String>(())?;
        let deadline = tokio::time::timeout(std::time::Duration::from_secs(3), execution);
        tokio::pin!(deadline);
        let wakes = Arc::new(AtomicUsize::new(0));
        let mut polls = 0usize;
        let result = futures_util::future::poll_fn(|cx| {
            polls += 1;
            let proxy = Arc::new(TraceWake { target: cx.waker().clone(), generation: wakes.clone() });
            let wake = waker_ref(&proxy);
            let mut traced = Context::from_waker(&wake);
            eprintln!("timeline OUTER_ENTER boundary={boundary} poll={polls} wakes={} c_polls={}", wakes.load(Ordering::SeqCst), c_polls.load(Ordering::SeqCst));
            let result = deadline.as_mut().poll(&mut traced);
            eprintln!("timeline OUTER_RETURN boundary={boundary} poll={polls} ready={} wakes={} c_polls={}", result.is_ready(), wakes.load(Ordering::SeqCst), c_polls.load(Ordering::SeqCst));
            result
        }).await;
        let answer = result.unwrap_or_else(|_| panic!("timeline DEADLINE boundary={boundary} polls={polls} c_polls={}", c_polls.load(Ordering::SeqCst)))?;
        assert_eq!(answer, "alphabetagammadelta:271828");
        assert_eq!(c_polls.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "timer-rescued counted boundary={boundary}");
        eprintln!("timeline COMPLETE boundary={boundary}");
    }
    Ok(())
}

#[cfg(all(feature = "async", any(feature = "lua54", feature = "lua55")))]
#[tokio::test]
async fn counted_hook_timer_control() {
    eprintln!("timeline TIMER_CONTROL enter");
    let deadline = tokio::time::timeout(std::time::Duration::from_millis(100), std::future::pending::<()>());
    assert!(deadline.await.is_err());
    eprintln!("timeline TIMER_CONTROL complete");
}
