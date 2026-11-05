use std::sync::Arc;

use mlua::{Error, GCMode, Lua, Result, UserData};

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
    match lua
        .load("local t = {}; for i = 1,10000 do t[i] = i end")
        .into_function()
    {
        Err(Error::MemoryError(_)) => {}
        _ => panic!("did not trigger memory error"),
    };

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

    let base = if cfg!(feature = "luau") { 0 } else { lua.used_memory() };
    lua.set_memory_limit(base + 10000)?;
    let thread = lua.create_thread(f)?;
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

    #[cfg(feature = "lua54")]
    {
        assert_eq!(lua.gc_gen(0, 0), GCMode::Incremental);
        assert_eq!(lua.gc_inc(0, 0, 0), GCMode::Generational);
    }

    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52", feature = "luau"))]
    {
        assert!(lua.gc_is_running());
        lua.gc_stop();
        assert!(!lua.gc_is_running());
        lua.gc_restart();
        assert!(lua.gc_is_running());
    }

    assert_eq!(lua.gc_inc(200, 100, 13), GCMode::Incremental);

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
