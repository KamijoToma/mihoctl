//! Runs Clash Verge / Clash Party style override scripts — `function main(config, profileName)`
//! returning the (possibly async) new config — inside an embedded QuickJS.
//!
//! The sandbox has only ECMAScript built-ins plus `console`: no filesystem, network, timers or
//! process access. Any exception, timeout, memory overrun or non-object result fails the build;
//! unlike Verge/Party there is no silent fallback to the unmodified config.

use anyhow::{Context as _, Result, anyhow, bail};
use rquickjs::{CatchResultExt, CaughtError, Context, Function, Runtime, Value};
use std::time::{Duration, Instant};

const MEMORY_LIMIT: usize = 128 << 20;
const PRELUDE: &str = r#"
(() => {
  const fmt = (v) => {
    if (typeof v === "string") return v;
    try { return JSON.stringify(v); } catch (_) { return String(v); }
  };
  const log = globalThis.__mihoctl_log;
  delete globalThis.__mihoctl_log;
  const make = (lvl) => (...args) => log(lvl, args.map(fmt).join(" "));
  globalThis.console = { log: make("log"), info: make("info"), warn: make("warn"), error: make("error"), debug: make("debug") };
})();
"#;

/// QuickJS leaves a `null` exception when it runs out of memory while allocating the "out of
/// memory" error itself; FreeBSD's jemalloc hits this every time.
fn describe(e: CaughtError) -> String {
    match &e {
        CaughtError::Value(v) if v.is_null() => {
            format!("null (most likely out of memory: the heap limit is {} MiB)", MEMORY_LIMIT >> 20)
        }
        _ => e.to_string(),
    }
}

pub fn run(name: &str, source: &str, config: &serde_json::Value, timeout: Duration) -> Result<serde_json::Value> {
    let rt = Runtime::new().context("creating QuickJS runtime")?;
    rt.set_memory_limit(MEMORY_LIMIT);
    let deadline = Instant::now() + timeout;
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let ctx = Context::full(&rt).context("creating QuickJS context")?;
    let input = serde_json::to_string(config)?;
    let script_name = name.to_string();

    let output: Result<String> = ctx.with(|ctx| {
        let log = Function::new(ctx.clone(), move |level: String, msg: String| {
            eprintln!("[{script_name}] {level}: {msg}");
        })
        .map_err(|e| anyhow!("{e}"))?;
        ctx.globals().set("__mihoctl_log", log).map_err(|e| anyhow!("{e}"))?;
        ctx.eval::<(), _>(PRELUDE).catch(&ctx).map_err(|e| anyhow!("prelude: {e}"))?;

        ctx.eval::<(), _>(source.as_bytes()).catch(&ctx).map_err(|e| anyhow!("{e}"))?;
        let main: Function = ctx
            .globals()
            .get("main")
            .catch(&ctx)
            .map_err(|_| anyhow!("script does not define a global `function main(config)`"))?;
        let arg = ctx.json_parse(input).catch(&ctx).map_err(|e| anyhow!("{e}"))?;
        let mut ret: Value = main.call((arg, "mihoctl")).catch(&ctx).map_err(|e| anyhow!("main threw: {}", describe(e)))?;

        if let Some(promise) = ret.as_promise().cloned() {
            while ctx.execute_pending_job() {}
            ret = promise
                .finish::<Value>()
                .catch(&ctx)
                .map_err(|e| anyhow!("main rejected or never settled: {}", describe(e)))?;
        }
        if !ret.is_object() || ret.is_array() {
            bail!("main must return the config object, got {}", ret.type_name());
        }
        let json = ctx.json_stringify(ret).catch(&ctx).map_err(|e| anyhow!("{e}"))?;
        json.context("main returned undefined")?.to_string().map_err(|e| anyhow!("{e}"))
    });

    let text = output.with_context(|| format!("override script {name}"))?;
    serde_json::from_str(&text).with_context(|| format!("override script {name}: result is not valid JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T: Duration = Duration::from_millis(500);

    #[test]
    fn verge_style_script_transforms_config() {
        let src = r#"
            function main(config, profileName) {
              config.dns = config.dns || {};
              config.dns.ipv6 = false;
              config.rules = ["DOMAIN-SUFFIX,ts.net,DIRECT", ...config.rules];
              config.profile = profileName;
              return config;
            }"#;
        let out = run("t", src, &json!({"rules": ["MATCH,P"]}), T).unwrap();
        assert_eq!(out, json!({"rules": ["DOMAIN-SUFFIX,ts.net,DIRECT", "MATCH,P"], "dns": {"ipv6": false}, "profile": "mihoctl"}));
    }

    #[test]
    fn async_main_is_awaited() {
        let src = "async function main(c) { await Promise.resolve(); c.mode = 'global'; return c; }";
        assert_eq!(run("t", src, &json!({"mode": "rule"}), T).unwrap(), json!({"mode": "global"}));
    }

    #[test]
    fn failures_are_errors_not_fallbacks() {
        let cases = [
            ("function main(c) { throw new Error('boom') }", "boom"),
            ("async function main(c) { await null; throw new Error('late') }", "late"),
            ("function main(c) { c.x = 1 }", "must return the config object"),
            ("function main(c) { return [c] }", "must return the config object"),
            ("var x = 1;", "does not define"),
            ("function main(c) { for (;;) {} }", "interrupted"),
            ("function main(c) { const a = []; for (;;) a.push(new Array(1e6).fill(1)) }", "out of memory"),
        ];
        for (src, needle) in cases {
            let err = format!("{:#}", run("t", src, &json!({}), T).unwrap_err());
            assert!(err.contains(needle), "{src}: {err}");
        }
    }

    #[test]
    fn sandbox_exposes_no_host_capabilities() {
        let src = "function main(c) { c.caps = [typeof require, typeof process, typeof fetch, typeof setTimeout, typeof std, typeof os]; return c; }";
        let out = run("t", src, &json!({}), T).unwrap();
        assert_eq!(out["caps"], json!(["undefined", "undefined", "undefined", "undefined", "undefined", "undefined"]));
    }
}
