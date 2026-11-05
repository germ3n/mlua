use std::sync::Arc;

use mlua::state::{GcIncParams, GcMode};
use mlua::{Error, Function, Lua, MultiValue, Result, Table, UserData};

#[cfg(any(feature = "lua54", feature = "lua55"))]
use mlua::state::GcGenParams;

#[test]
fn test_memory_limit() -> Result<()> {
    let lua = Lua::new();
    #[cfg(feature = "luau")]
    lua.set_fuel(u64::MAX)?;

    let initial_memory = lua.used_memory();
    assert!(
        initial_memory > 0,
        "used_memory reporting is wrong, lua uses memory for stdlib"
    );

    let f = lua
        .load("local t = {}; for i = 1,10000 do t[i] = i end")
        .into_function()?;
    f.call::<()>(()).expect("should trigger no memory limit");

    if cfg!(feature = "luajit") && lua.set_memory_limit(0).is_err() {
        // seems this luajit version does not support memory limit
        return Ok(());
    }

    // Luau counts from zero where the limit is set; the others count the whole heap.
    let base = if cfg!(feature = "luau") { 0 } else { initial_memory };
    lua.set_memory_limit(base + 10000)?;
    match f.call::<()>(()) {
        Err(Error::MemoryError(_)) => {}
        something_else => panic!("did not trigger memory error: {:?}", something_else),
    };

    lua.set_memory_limit(0)?;
    f.call::<()>(()).expect("should trigger no memory limit");

    // Test memory limit during chunk loading: Luau's count starts at zero, so less is enough
    lua.set_memory_limit(if cfg!(feature = "luau") { 64 } else { 1024 })?;
    match lua.load(format!("return '{}'", "x".repeat(4096))).into_function() {
        Err(Error::MemoryError(_)) => {}
        _ => panic!("did not trigger memory error"),
    };

    // Callback cleanup must not panic if the reference stack cannot grow under a memory limit.
    for nrefs in 0..64 {
        let lua = Lua::new();
        let f = lua.create_function(|lua, value: i32| {
            lua.set_memory_limit(1)?;
            Ok(value)
        })?;
        let _refs: Vec<_> = (0..nrefs).map(|_| lua.globals()).collect();
        assert_eq!(f.call::<i32>(42)?, 42);
        lua.set_memory_limit(0)?;
    }

    Ok(())
}

#[test]
fn test_result_reference_exhaustion() -> Result<()> {
    let lua = Lua::new();
    if cfg!(feature = "luajit") && lua.set_memory_limit(0).is_err() {
        return Ok(());
    }
    let f: Function = lua
        .load("local t = {}; return function() return t, t end")
        .eval()?;
    f.call::<MultiValue>(())?; // Warm the call stack before limiting allocations.
    lua.gc_stop();
    lua.set_memory_limit(1)?;

    let mut refs = Vec::new();
    loop {
        match f.call::<Table>(()) {
            Ok(table) => refs.push(table),
            Err(Error::StackError) => break,
            Err(err) => return Err(err),
        }
    }

    // The first result can be retained, but the second must fail and release the first.
    drop(refs.pop().unwrap());
    assert!(matches!(f.call::<MultiValue>(()), Err(Error::StackError)));
    f.call::<Table>(())?;
    lua.set_memory_limit(0)?;
    assert_eq!(f.call::<MultiValue>(())?.len(), 2);
    Ok(())
}

// In the fork, fuel belongs to each Luau thread, and a new one has none.
#[cfg(not(feature = "luau"))]
#[test]
fn test_memory_limit_thread() -> Result<()> {
    let lua = Lua::new();

    let f = lua
        .load("local t = {}; for i = 1,10000 do t[i] = i end")
        .into_function()?;

    if cfg!(feature = "luajit") && lua.set_memory_limit(0).is_err() {
        // seems this luajit version does not support memory limit
        return Ok(());
    }

    let thread = lua.create_thread(f)?;
    let base = if cfg!(feature = "luau") { 0 } else { lua.used_memory() };
    lua.set_memory_limit(base + 10000)?;
    match thread.resume::<()>(()) {
        Err(Error::MemoryError(_)) => {}
        something_else => panic!("did not trigger memory error: {:?}", something_else),
    };

    Ok(())
}

// Luau counts the memory limit by the sizes it asks for, from where the limit is set, so the same
// code counts the same bytes and runs out at the same point in any state, and garbage the
// collector frees comes off.
#[cfg(feature = "luau")]
#[test]
fn test_memory_counted() -> Result<()> {
    let counted = || -> Result<(i64, String)> {
        let lua = Lua::new();
        lua.set_fuel(u64::MAX)?;
        lua.set_memory_limit(1 << 20)?;
        assert_eq!(lua.memory_counted(), 0);

        let kept = lua.load("local t = {} for i = 1, 1000 do t[i] = { i } end return t").eval::<mlua::Table>()?;
        let made = lua.memory_counted();
        assert!(made > 1000 * 16, "{made}");

        // Far more garbage than the limit, which the collector frees as it goes.
        lua.load("for i = 1, 20000 do local s = string.rep('x', 1000) .. i end").exec()?;
        drop(kept);

        let failed = lua.load("local t = {} local n = 0 while true do n += 1 t[n] = string.rep('y', 1000) .. n end").exec();
        match failed {
            Err(Error::MemoryError(_)) => {}
            other => panic!("did not trigger memory error: {other:?}"),
        }

        Ok((made, format!("{}", lua.memory_counted())))
    };

    assert_eq!(counted()?, counted()?);

    Ok(())
}

#[test]
fn test_gc_control() -> Result<()> {
    let lua = Lua::new();
    let globals = lua.globals();

    #[cfg(any(feature = "lua55", feature = "lua54"))]
    {
        assert!(matches!(
            lua.gc_set_mode(GcMode::Generational(GcGenParams::default())),
            GcMode::Incremental(_)
        ));
        assert!(matches!(
            lua.gc_set_mode(GcMode::Incremental(GcIncParams::default())),
            GcMode::Generational(_)
        ));
    }

    #[cfg(any(
        feature = "lua55",
        feature = "lua54",
        feature = "lua53",
        feature = "lua52",
        feature = "luau"
    ))]
    {
        assert!(lua.gc_is_running());
        lua.gc_stop();
        assert!(!lua.gc_is_running());
        lua.gc_restart();
        assert!(lua.gc_is_running());
    }

    assert!(matches!(
        lua.gc_set_mode(GcMode::Incremental({
            let p = GcIncParams::default().step_multiplier(100);
            #[cfg(not(feature = "luau"))]
            let p = p.pause(200);
            #[cfg(feature = "luau")]
            let p = p.goal(200);
            p
        })),
        GcMode::Incremental(_)
    ));

    struct MyUserdata(#[allow(unused)] Arc<()>);
    impl UserData for MyUserdata {}

    let rc = Arc::new(());
    globals.set("userdata", lua.create_userdata(MyUserdata(rc.clone()))?)?;
    globals.raw_remove("userdata")?;

    assert_eq!(Arc::strong_count(&rc), 2);
    lua.gc_collect()?;
    lua.gc_collect()?;
    assert_eq!(Arc::strong_count(&rc), 1);

    Ok(())
}

#[cfg(any(feature = "lua54", feature = "lua55"))]
#[test]
fn test_gc_set_mode_in_finalizer() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let lua = Lua::new();

    // While finalizers are running, the GC is internally stopped and `lua_gc` rejects all
    // options
    let switched = Arc::new(AtomicBool::new(false));
    let switched2 = switched.clone();
    let finalizer = lua.create_function(move |lua, ()| {
        let mode = lua.gc_set_mode(GcMode::Generational(GcGenParams::default()));
        switched2.store(matches!(mode, GcMode::Generational(_)), Ordering::Relaxed);
        Ok(())
    })?;
    lua.globals().set("finalizer", finalizer)?;
    lua.load("setmetatable({}, { __gc = finalizer })").exec()?;
    lua.globals().raw_remove("finalizer")?;

    lua.gc_collect()?;
    lua.gc_collect()?;
    assert!(
        switched.load(Ordering::Relaxed),
        "gc_set_mode did not complete in finalizer"
    );

    Ok(())
}

#[cfg(any(feature = "lua53", feature = "lua52"))]
#[test]
fn test_gc_error() {
    use mlua::Error;

    let lua = Lua::new();
    match lua
        .load(
            r#"
            val = nil
            table = {}
            setmetatable(table, {
                __gc = function()
                    error("gcwascalled")
                end
            })
            table = nil
            collectgarbage("collect")
    "#,
        )
        .exec()
    {
        Err(Error::GarbageCollectorError(_)) => {}
        Err(e) => panic!("__gc error did not result in correct error, instead: {}", e),
        Ok(()) => panic!("__gc error did not result in error"),
    }
}
