//! The web globals OpenCode's Code Mode offers and Boa does not: `URL`,
//! `URLSearchParams`, `Headers`, `TextEncoder`/`TextDecoder`,
//! `structuredClone`, `crypto` (`getRandomValues`, `randomUUID`),
//! `atob`/`btoa`, `Buffer`, and `Uint8Array`'s base64 and hex methods.
//!
//! They are written in JavaScript (`web.js`), on a few natives from here:
//! WHATWG URL parsing and setters (the `url` crate), form encoding, and
//! random bytes. None of them reaches the network or the filesystem. The
//! script is compiled the first time a program reads one of its globals,
//! so programs that use none of them start as fast as before.
//!
//! Dynamic `import()` loads only the modules these globals make up (`url`,
//! `util`, `buffer`, `crypto`, with or without `node:`), as in OpenCode;
//! everything else, local files above all, fails to import.

use boa_engine::module::{ModuleLoader, Referrer, SyntheticModuleInitializer};
use boa_engine::object::builtins::JsArray;
use boa_engine::{
    js_string, Context, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue, Module,
    NativeFunction, Source,
};
use std::cell::RefCell;
use std::rc::Rc;

const SCRIPT: &str = include_str!("web.js");

/// The globals `web.js` defines, each compiled on first read.
const GLOBALS: &[&str] = &[
    "URL",
    "URLSearchParams",
    "Headers",
    "TextEncoder",
    "TextDecoder",
    "structuredClone",
    "crypto",
    "atob",
    "btoa",
    "Buffer",
];

fn text(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args
        .get(i)
        .cloned()
        .unwrap_or_default()
        .to_string(ctx)?
        .to_std_string_lossy())
}

fn strings(v: &[String], ctx: &mut Context) -> JsValue {
    JsArray::from_iter(
        v.iter().map(|s| JsValue::from(JsString::from(s.as_str()))),
        ctx,
    )
    .into()
}

fn parts(u: &url::Url, ctx: &mut Context) -> JsValue {
    use url::quirks as q;
    let v = [
        q::href(u),
        &q::origin(u),
        q::protocol(u),
        q::username(u),
        q::password(u),
        q::host(u),
        q::hostname(u),
        q::port(u),
        q::pathname(u),
        q::search(u),
        q::hash(u),
    ]
    .map(String::from);
    strings(&v, ctx)
}

/// `urlParse(input, base?)`: the URL's parts (href, origin, protocol,
/// username, password, host, hostname, port, pathname, search, hash), or
/// `null` when the input or the base does not parse.
fn url_parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let input = text(args, 0, ctx)?;
    let base = match args.get(1) {
        Some(b) if !b.is_undefined() => match url::Url::parse(&text(args, 1, ctx)?) {
            Ok(b) => Some(b),
            Err(_) => return Ok(JsValue::null()),
        },
        _ => None,
    };
    Ok(
        match url::Url::options().base_url(base.as_ref()).parse(&input) {
            Ok(u) => parts(&u, ctx),
            Err(_) => JsValue::null(),
        },
    )
}

/// `urlSet(href, name, value)`: the parts after the WHATWG setter for
/// `name`, or `null` when an `href` does not parse (other setters ignore
/// what they cannot use, as in browsers).
fn url_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use url::quirks as q;
    let (href, name, value) = (
        text(args, 0, ctx)?,
        text(args, 1, ctx)?,
        text(args, 2, ctx)?,
    );
    let Ok(mut u) = url::Url::parse(&href) else {
        return Ok(JsValue::null());
    };
    let ok = match name.as_str() {
        "href" => q::set_href(&mut u, &value).is_ok(),
        "protocol" => {
            let _ = q::set_protocol(&mut u, &value);
            true
        }
        "username" => {
            let _ = q::set_username(&mut u, &value);
            true
        }
        "password" => {
            let _ = q::set_password(&mut u, &value);
            true
        }
        "host" => {
            let _ = q::set_host(&mut u, &value);
            true
        }
        "hostname" => {
            let _ = q::set_hostname(&mut u, &value);
            true
        }
        "port" => {
            let _ = q::set_port(&mut u, &value);
            true
        }
        "pathname" => {
            q::set_pathname(&mut u, &value);
            true
        }
        "search" => {
            q::set_search(&mut u, &value);
            true
        }
        "hash" => {
            q::set_hash(&mut u, &value);
            true
        }
        _ => false,
    };
    Ok(if ok { parts(&u, ctx) } else { JsValue::null() })
}

/// `formParse(query)`: `application/x-www-form-urlencoded` pairs, flat
/// (`[name, value, name, value, ...]`).
fn form_parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = text(args, 0, ctx)?;
    let flat: Vec<String> = url::form_urlencoded::parse(s.as_bytes())
        .flat_map(|(k, v)| [k.into_owned(), v.into_owned()])
        .collect();
    Ok(strings(&flat, ctx))
}

/// `formSerialize([name, value, ...])`: the pairs, form-encoded.
fn form_serialize(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let list = args
        .first()
        .and_then(JsValue::as_object)
        .ok_or_else(|| JsNativeError::typ().with_message("formSerialize expects an array"))?;
    let n = list.get(js_string!("length"), ctx)?.to_length(ctx)?;
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    let mut i = 0;
    while i + 1 < n {
        let k = list.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy();
        let v = list.get(i + 1, ctx)?.to_string(ctx)?.to_std_string_lossy();
        out.append_pair(&k, &v);
        i += 2;
    }
    Ok(JsValue::from(JsString::from(out.finish().as_str())))
}

/// `random(n)`: `n` random bytes (at most 65536), as an array of numbers.
fn random(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use rand::RngCore;
    let n = args
        .first()
        .cloned()
        .unwrap_or_default()
        .to_length(ctx)?
        .min(65536) as usize;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    Ok(JsArray::from_iter(bytes.into_iter().map(JsValue::from), ctx).into())
}

type Native = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

/// Compile `web.js` (once) and define its globals.
fn load(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let natives = JsObject::with_null_proto();
    let fns: [(&str, Native); 5] = [
        ("urlParse", url_parse),
        ("urlSet", url_set),
        ("formParse", form_parse),
        ("formSerialize", form_serialize),
        ("random", random),
    ];
    for (name, f) in fns {
        let f = boa_engine::object::FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_fn_ptr(f),
        )
        .name(js_string!(name))
        .build();
        natives.set(JsString::from(name), f, false, ctx)?;
    }
    let exports = args.first().cloned().unwrap_or_default();
    let install = ctx.eval(Source::from_bytes(SCRIPT))?;
    let install = install
        .as_callable()
        .ok_or_else(|| JsNativeError::typ().with_message("web.js did not load"))?;
    install.call(&JsValue::undefined(), &[natives.into(), exports], ctx)?;
    Ok(JsValue::undefined())
}

/// Define the web globals on `ctx`, as accessors that compile `web.js` the
/// first time any of them is read.
pub fn install(ctx: &mut Context) -> JsResult<()> {
    let loader = boa_engine::object::FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_fn_ptr(load),
    )
    .build();
    ctx.global_object()
        .set(js_string!("__webLoad"), loader, false, ctx)?;
    let names = GLOBALS
        .iter()
        .map(|n| format!("{n:?}"))
        .collect::<Vec<_>>()
        .join(",");
    // The globals stay accessors: Boa caches global bindings, so they are
    // never deleted or redefined once programs could have seen them.
    let lazy = format!(
        r#"(() => {{
  const load = globalThis.__webLoad;
  delete globalThis.__webLoad;
  const exports = Object.create(null);
  let loaded = false;
  const web = () => {{
    if (!loaded) {{ loaded = true; load(exports); }}
    return exports;
  }};
  // Uint8Array's base64 and hex methods, where Boa lacks them.
  const U8 = globalThis.Uint8Array;
  const bytes = {{
    fromBase64(string) {{ return web().bytes.fromBase64.apply(this, arguments); }},
    fromHex(string) {{ return web().bytes.fromHex.apply(this, arguments); }},
  }};
  const proto = {{
    toBase64() {{ return web().bytes.toBase64.apply(this, arguments); }},
    toHex() {{ return web().bytes.toHex.apply(this, arguments); }},
  }};
  for (const [target, table] of [[U8, bytes], [U8.prototype, proto]]) {{
    for (const k of Object.keys(table)) {{
      if (typeof target[k] !== "function") Object.defineProperty(target, k, {{ value: table[k], writable: true, configurable: true }});
    }}
  }}
  for (const n of [{names}]) {{
    let own = false, value;
    Object.defineProperty(globalThis, n, {{
      configurable: true,
      enumerable: false,
      get: {{ [n]() {{
        return own ? value : web()[n];
      }} }}[n],
      set: {{ [n](v) {{ own = true; value = v; }} }}[n],
    }});
  }}
}})();"#
    );
    ctx.eval(Source::from_bytes(lazy.as_bytes()))?;
    Ok(())
}

/// The context Code Mode programs run in: Boa's, with module loading
/// limited to [`Loader`]'s built-in modules.
pub fn context() -> Context {
    Context::builder()
        .module_loader(Rc::new(Loader))
        .build()
        .unwrap_or_default()
}

/// Node's own module names (with or without `node:`); importing one of
/// these that Code Mode does not provide says so.
const CORE: &str = "assert async_hooks buffer child_process cluster console constants crypto dgram diagnostics_channel dns domain events fs http http2 https inspector module net os path perf_hooks process punycode querystring readline repl stream string_decoder sys timers tls trace_events tty url util v8 vm wasi worker_threads zlib";

/// The globals a module name provides, as OpenCode resolves them.
fn provided(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "url" => &["URL", "URLSearchParams"],
        "util" => &["TextEncoder", "TextDecoder"],
        "buffer" => &["Buffer", "atob", "btoa"],
        "crypto" => &["randomUUID", "getRandomValues"],
        _ => return None,
    })
}

fn import_error(specifier: &str) -> JsError {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    let why = if specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with("file:")
    {
        "local files can't be imported."
    } else if specifier.starts_with("node:")
        || specifier == "bun"
        || specifier.starts_with("bun:")
        || CORE
            .split(' ')
            .any(|c| c == name.split('/').next().unwrap_or(name))
    {
        "this runtime doesn't provide it."
    } else {
        "packages can't be installed here."
    };
    JsNativeError::error()
        .with_message(format!("Cannot import {specifier:?}: {why}"))
        .into()
}

/// Loads only Code Mode's built-in modules: no files, no packages.
pub struct Loader;

impl ModuleLoader for Loader {
    async fn load_imported_module(
        self: Rc<Self>,
        _referrer: Referrer,
        request: boa_engine::module::ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> JsResult<Module> {
        let specifier = request.specifier().to_std_string_lossy();
        let name = specifier.strip_prefix("node:").unwrap_or(&specifier);
        let Some(names) = provided(name) else {
            return Err(import_error(&specifier));
        };
        let ctx = &mut *context.borrow_mut();
        let module = JsObject::with_null_proto();
        let source = if name == "crypto" {
            ctx.global_object().get(js_string!("crypto"), ctx)?
        } else {
            ctx.global_object().into()
        };
        let source = source
            .as_object()
            .ok_or_else(|| import_error(&specifier))?
            .clone();
        for n in names {
            let v = source.get(JsString::from(*n), ctx)?;
            module.set(JsString::from(*n), v, false, ctx)?;
        }
        let mut exports: Vec<JsString> = names.iter().map(|n| JsString::from(*n)).collect();
        exports.push(js_string!("default"));
        Ok(Module::synthetic(
            &exports,
            SyntheticModuleInitializer::from_copy_closure_with_captures(
                |m, module: &JsObject, ctx| {
                    for key in module.own_property_keys(ctx)? {
                        if let boa_engine::property::PropertyKey::String(k) = key {
                            let v = module.get(k.clone(), ctx)?;
                            m.set_export(&k, v)?;
                        }
                    }
                    m.set_export(&js_string!("default"), module.clone().into())?;
                    Ok(())
                },
                module,
            ),
            None,
            None,
            ctx,
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::time::Duration;

    async fn run(code: &str) -> Result<serde_json::Value, String> {
        super::super::execute_with(None, code, &[], 5, Duration::from_secs(10), |_, _| async {
            Err::<String, String>("no tools".into())
        })
        .await
        .result
    }

    #[tokio::test]
    async fn import_loads_only_the_built_in_modules() {
        let v = run(r#"
            const fail = (s) => import(s).then(() => "loaded", (e) => e.message);
            const { Buffer: B, default: d } = await import("node:buffer");
            const url = await import("url");
            return [B === Buffer, d.atob === atob, url.URL === URL, (await import("node:crypto")).randomUUID().length,
              await fail("./Cargo.toml"), await fail("/etc/hosts"), await fail("fs"), await fail("yaml")]"#)
        .await
        .unwrap();
        assert_eq!(
            v,
            json!([
                true,
                true,
                true,
                36,
                "Cannot import \"./Cargo.toml\": local files can't be imported.",
                "Cannot import \"/etc/hosts\": local files can't be imported.",
                "Cannot import \"fs\": this runtime doesn't provide it.",
                "Cannot import \"yaml\": packages can't be installed here."
            ])
        );
    }

    #[tokio::test]
    async fn the_globals_load_on_first_use_and_can_be_replaced() {
        let v = run(r#"
            const listed = Object.keys(globalThis).filter((k) => ["URL", "Buffer", "atob"].includes(k));
            URL = 1;
            return [listed, URL, typeof Headers, new Uint8Array([1, 255]).toHex(), btoa("hi")]"#)
        .await
        .unwrap();
        assert_eq!(v, json!([[], 1, "function", "01ff", "aGk="]));
    }

    #[tokio::test]
    async fn buffer_reads_writes_and_encodes_like_node() {
        let v = run(r#"
            const b = Buffer.alloc(8);
            b.writeUInt32LE(0xdeadbeef, 0);
            b.writeInt16BE(-2, 4);
            let range;
            try { b.readUInt32LE(6) } catch (e) { range = [e.name, e.code] }
            return [b.toString("hex"), b.readUInt32LE(0), b.readInt16BE(4), range,
              Buffer.from("aGk", "base64").toString(), Buffer.from("hi").toString("base64url"),
              Buffer.from("é", "latin1").toString("hex"), Buffer.from("hi", "utf16le").toString("hex"),
              Buffer.from([1, 2, 3]).map((x) => x * 2) instanceof Buffer, [...Buffer.from("abc").subarray(-2)]]"#)
        .await
        .unwrap();
        assert_eq!(
            v,
            json!([
                "efbeaddefffe0000",
                3735928559.0,
                -2.0,
                ["RangeError", "ERR_OUT_OF_RANGE"],
                "hi",
                "aGk",
                "e9",
                "68006900",
                true,
                [98, 99]
            ])
        );
    }

    #[tokio::test]
    async fn url_setters_follow_the_whatwg_rules() {
        let v = run(r#"
            const u = new URL("https://example.test:8080/a?x=1#h");
            u.port = "443"; u.hostname = "other.test"; u.protocol = "http"; u.search = "?y=2";
            const ignored = new URL("https://a.test/"); ignored.port = "nope";
            let bad;
            try { u.href = "not a url" } catch (e) { bad = e.message }
            return [u.href, u.searchParams.get("y"), ignored.href, bad, URL.parse("x") === null]"#)
        .await
        .unwrap();
        assert_eq!(
            v,
            json!([
                "http://other.test/a?y=2#h",
                "2",
                "https://a.test/",
                "URL.href received an invalid value.",
                true
            ])
        );
    }
}
