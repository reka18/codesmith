// Code Mode's web globals, as OpenCode's Code Mode provides them (see
// web.rs). Evaluates to a function that installs them, given the natives.
(function install(native, exports) {
  "use strict";
  const G = globalThis;
  const { urlParse, urlSet, formParse, formSerialize, random } = native;
  const U8 = G.Uint8Array;
  const TypedArray = Object.getPrototypeOf(Int8Array.prototype).constructor;
  const typedArrayTag = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Int8Array.prototype), Symbol.toStringTag).get;
  const isTyped = (v) => v !== null && typeof v === "object" && typedArrayTag.call(v) !== undefined;
  const isBytes = (v) => isTyped(v) && typedArrayTag.call(v) === "Uint8Array";
  const IteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));

  const hidden = (o, k, v) => Object.defineProperty(o, k, { value: v, writable: true, configurable: true, enumerable: false });
  const frozen = (o, k, v) => Object.defineProperty(o, k, { value: v, writable: false, configurable: false, enumerable: false });
  // Methods from an object literal keep their names and lengths, and cannot be constructed.
  const methods = (target, table) => {
    for (const key of Reflect.ownKeys(table)) {
      const d = Object.getOwnPropertyDescriptor(table, key);
      if ("value" in d) hidden(target, key, d.value);
      else Object.defineProperty(target, key, { get: d.get, set: d.set, configurable: true, enumerable: false });
    }
  };
  const global = (name, value) => {
    exports[name] = value;
  };
  const describe = (v) => {
    if (v === null || v === undefined) return String(v);
    if (typeof v === "function") return "a function";
    if (typeof v !== "object") return "a " + typeof v;
    if (Array.isArray(v)) return "an array";
    if (isTyped(v)) return "a " + typedArrayTag.call(v);
    return "an object";
  };
  const requiresNew = (name) => new TypeError(`Constructor ${name} requires 'new'.`);
  const incompatible = (method, v) => new TypeError(`${method} called on incompatible receiver ${describe(v)}.`);
  const callback = (f, name) => {
    if (typeof f !== "function") throw new TypeError(`${name} expects a function callback.`);
    return f;
  };
  const needs = (type, name, args, count) => {
    if (args.length < count) throw new TypeError(`${type}.${name} requires ${count} argument${count === 1 ? "" : "s"}.`);
  };
  const usv = (v) => String(v).toWellFormed();
  const isIterable = (v) => v !== null && (typeof v === "object" || typeof v === "function") && typeof v[Symbol.iterator] === "function";
  const ctor = (fn, proto, length) => {
    Object.defineProperty(fn, "prototype", { value: proto, writable: false, enumerable: false, configurable: false });
    hidden(proto, "constructor", fn);
    if (length !== undefined) Object.defineProperty(fn, "length", { value: length, configurable: true });
    return fn;
  };

  // ----- iterators over a live list ---------------------------------------
  // Each step reads the list again, so items added during iteration are reached.
  const iterState = new WeakMap();
  const makeIteratorProto = (tag) => {
    const proto = Object.create(IteratorPrototype);
    methods(proto, {
      next() {
        const s = iterState.get(this);
        if (s === undefined) throw incompatible(`${tag}.next`, this);
        const list = s.list();
        if (s.done || s.index >= list.length) {
          s.done = true;
          return { value: undefined, done: true };
        }
        const [k, v] = list[s.index++];
        return { value: s.kind === "keys" ? k : s.kind === "values" ? v : [k, v], done: false };
      },
    });
    Object.defineProperty(proto, Symbol.toStringTag, { value: tag, configurable: true });
    return proto;
  };
  const iterate = (proto, list, kind) => {
    const it = Object.create(proto);
    iterState.set(it, { list, kind, index: 0, done: false });
    return it;
  };
  const forEachPair = (target, pairs, f, thisArg) => {
    for (let i = 0; i < pairs.length; i++) f.call(thisArg, pairs[i][1], pairs[i][0], target);
  };
  // A synchronous iterable of [name, value] pairs, read in full before any length is checked; or null.
  const readPairs = (init, label) => {
    if (!isIterable(init)) return null;
    const pairs = [];
    for (const pair of init) {
      if (!isIterable(pair)) throw new TypeError(label + " expects iterable [name, value] pairs.");
      const strings = [];
      for (const item of pair) strings.push(String(item));
      pairs.push(strings);
    }
    if (pairs.some((p) => p.length !== 2)) throw new TypeError(`${label} expects iterable [name, value] pairs.`);
    return pairs;
  };
  const record = (init) => Object.keys(init).map((k) => [k, init[k]]);

  // ----- URLSearchParams ----------------------------------------------------
  const params = new WeakMap(); // instance -> { list, url }
  const parseQuery = (text) => {
    const flat = formParse(text.startsWith("?") ? text.slice(1) : text);
    const list = [];
    for (let i = 0; i < flat.length; i += 2) list.push([flat[i], flat[i + 1]]);
    return list;
  };
  const serialize = (list) => formSerialize(list.flat());
  const paramsIterator = makeIteratorProto("URLSearchParams Iterator");
  function URLSearchParams(init) {
    if (new.target === undefined) throw requiresNew("URLSearchParams");
    let list;
    if (init === undefined) list = [];
    else if (params.has(init)) list = params.get(init).list.map((p) => [p[0], p[1]]);
    else if (typeof init === "string") list = parseQuery(init);
    else if (init === null || typeof init === "number" || typeof init === "boolean" || typeof init === "bigint") list = parseQuery(String(init));
    else if (typeof init === "symbol") throw new TypeError("new URLSearchParams(...) expects a query string, data object, iterable pairs, or URLSearchParams.");
    else {
      const pairs = readPairs(init, "new URLSearchParams(...)");
      list = (pairs ?? record(init)).map(([k, v]) => [usv(k), usv(v)]);
    }
    params.set(this, { list, url: null });
  }
  const sp = (self, name) => {
    const s = params.get(self);
    if (s === undefined) throw incompatible(`URLSearchParams.prototype.${name}`, self);
    return s;
  };
  const updateURL = (s) => {
    if (s.url !== null) s.url.query(serialize(s.list));
  };
  ctor(URLSearchParams, {});
  methods(URLSearchParams.prototype, {
    get size() {
      return sp(this, "size").list.length;
    },
    append(name, value) {
      const s = sp(this, "append");
      needs("URLSearchParams", "append", arguments, 2);
      s.list.push([usv(name), usv(value)]);
      updateURL(s);
    },
    delete(name) {
      const s = sp(this, "delete");
      needs("URLSearchParams", "delete", arguments, 1);
      const n = usv(name);
      const value = arguments[1] === undefined ? undefined : usv(arguments[1]);
      const kept = s.list.filter((p) => !(p[0] === n && (value === undefined || p[1] === value)));
      s.list.length = 0;
      s.list.push(...kept);
      updateURL(s);
    },
    get(name) {
      const s = sp(this, "get");
      needs("URLSearchParams", "get", arguments, 1);
      const n = usv(name);
      const found = s.list.find((p) => p[0] === n);
      return found === undefined ? null : found[1];
    },
    getAll(name) {
      const s = sp(this, "getAll");
      needs("URLSearchParams", "getAll", arguments, 1);
      const n = usv(name);
      return s.list.filter((p) => p[0] === n).map((p) => p[1]);
    },
    has(name) {
      const s = sp(this, "has");
      needs("URLSearchParams", "has", arguments, 1);
      const n = usv(name);
      const value = arguments[1] === undefined ? undefined : usv(arguments[1]);
      return s.list.some((p) => p[0] === n && (value === undefined || p[1] === value));
    },
    set(name, value) {
      const s = sp(this, "set");
      needs("URLSearchParams", "set", arguments, 2);
      const n = usv(name);
      const v = usv(value);
      const i = s.list.findIndex((p) => p[0] === n);
      if (i === -1) s.list.push([n, v]);
      else {
        s.list[i] = [n, v];
        const kept = s.list.filter((p, j) => j <= i || p[0] !== n);
        s.list.length = 0;
        s.list.push(...kept);
      }
      updateURL(s);
    },
    sort() {
      const s = sp(this, "sort");
      // Stable, by UTF-16 code units of the names.
      const sorted = s.list
        .map((p, i) => [p, i])
        .sort((a, b) => (a[0][0] < b[0][0] ? -1 : a[0][0] > b[0][0] ? 1 : a[1] - b[1]))
        .map((x) => x[0]);
      s.list.length = 0;
      s.list.push(...sorted);
      updateURL(s);
    },
    forEach(f) {
      const s = sp(this, "forEach");
      needs("URLSearchParams", "forEach", arguments, 1);
      callback(f, "URLSearchParams.forEach");
      forEachPair(this, s.list.map((p) => [p[0], p[1]]), f, arguments[1]);
    },
    keys() {
      const s = sp(this, "keys");
      return iterate(paramsIterator, () => s.list, "keys");
    },
    values() {
      const s = sp(this, "values");
      return iterate(paramsIterator, () => s.list, "values");
    },
    entries() {
      const s = sp(this, "entries");
      return iterate(paramsIterator, () => s.list, "entries");
    },
    toString() {
      return serialize(sp(this, "toString").list);
    },
  });
  hidden(URLSearchParams.prototype, Symbol.iterator, URLSearchParams.prototype.entries);
  Object.defineProperty(URLSearchParams.prototype, Symbol.toStringTag, { value: "URLSearchParams", configurable: true });
  global("URLSearchParams", URLSearchParams);

  // ----- URL ----------------------------------------------------------------
  const FIELDS = ["href", "origin", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"];
  const urls = new WeakMap(); // instance -> { parts, params }
  const urlArg = (v) => (urls.has(v) ? urls.get(v).parts[0] : usv(v));
  const makeURL = (self, parts) => {
    const state = { parts, params: null };
    const owned = Object.create(URLSearchParams.prototype);
    const s = { list: parseQuery(parts[9]), url: null };
    s.url = {
      query(text) {
        const next = urlSet(state.parts[0], "search", text);
        if (next !== null) state.parts = next;
      },
    };
    params.set(owned, s);
    state.params = owned;
    state.list = s.list;
    urls.set(self, state);
    return self;
  };
  function URL(url) {
    if (new.target === undefined) throw requiresNew("URL");
    if (arguments.length === 0) throw new TypeError("new URL(...) requires a URL string and an optional base URL.");
    const base = arguments[1] === undefined ? undefined : urlArg(arguments[1]);
    const parts = urlParse(urlArg(url), base);
    if (parts === null) throw new TypeError(`new URL(...) received an invalid URL${base === undefined ? "" : " or base URL"}.`);
    makeURL(this, parts);
  }
  ctor(URL, {}, 1);
  const parse = (name, args) => {
    if (args.length === 0) throw new TypeError(`URL.${name} requires a URL argument.`);
    const base = args[1] === undefined ? undefined : urlArg(args[1]);
    return urlParse(urlArg(args[0]), base);
  };
  methods(URL, {
    canParse(url) {
      return parse("canParse", arguments) !== null;
    },
    parse(url) {
      const parts = parse("parse", arguments);
      return parts === null ? null : makeURL(Object.create(URL.prototype), parts);
    },
  });
  const us = (self, name) => {
    const s = urls.get(self);
    if (s === undefined) throw incompatible(`URL.prototype.${name}`, self);
    return s;
  };
  FIELDS.forEach((name, i) => {
    const get = {
      [name]() {
        return us(this, name).parts[i];
      },
    }[name];
    const set =
      name === "origin"
        ? undefined
        : {
            [name](value) {
              const s = us(this, name);
              const next = urlSet(s.parts[0], name, usv(value));
              if (next === null) {
                if (name === "href") throw new TypeError("URL.href received an invalid value.");
                return;
              }
              s.parts = next;
              if (name === "href" || name === "search") {
                const list = parseQuery(next[9]);
                s.list.length = 0;
                s.list.push(...list);
              }
            },
          }[name];
    Object.defineProperty(URL.prototype, name, { get, set, configurable: true, enumerable: true });
  });
  methods(URL.prototype, {
    get searchParams() {
      return us(this, "searchParams").params;
    },
    toString() {
      return us(this, "toString").parts[0];
    },
    toJSON() {
      return us(this, "toJSON").parts[0];
    },
  });
  Object.defineProperty(URL.prototype, "searchParams", { enumerable: true });
  Object.defineProperty(URL.prototype, Symbol.toStringTag, { value: "URL", configurable: true });
  global("URL", URL);

  // ----- Headers ------------------------------------------------------------
  const headers = new WeakMap(); // instance -> list of [lowercase name, value]
  const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
  const name = (method, v) => {
    const n = String(v);
    if (!TOKEN.test(n)) throw new TypeError(`Headers.${method}: "${n}" is an invalid header name.`);
    return n.toLowerCase();
  };
  const value = (method, v) => {
    const s = String(v).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
    if (/[\0\r\n]|[^\0-\xff]/.test(s)) throw new TypeError(`Headers.${method}: "${s}" is an invalid header value.`);
    return s;
  };
  // The sorted and combined list iteration shows: set-cookie values stay apart.
  const combined = (list) => {
    const names = [...new Set(list.map((p) => p[0]))].sort();
    const out = [];
    for (const n of names) {
      const values = list.filter((p) => p[0] === n).map((p) => p[1]);
      if (n === "set-cookie") for (const v of values) out.push([n, v]);
      else out.push([n, values.join(", ")]);
    }
    return out;
  };
  const headersIterator = makeIteratorProto("Headers Iterator");
  const hs = (self, method) => {
    const s = headers.get(self);
    if (s === undefined) throw incompatible(`Headers.prototype.${method}`, self);
    return s;
  };
  const append = (list, method, n, v) => list.push([name(method, n), value(method, v)]);
  function Headers(init) {
    if (new.target === undefined) throw requiresNew("Headers");
    const list = [];
    if (init !== undefined) {
      if (init === null || (typeof init !== "object" && typeof init !== "function")) {
        throw new TypeError("new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.");
      }
      const pairs = readPairs(init, "new Headers(...)") ?? record(init).map(([k, v]) => [k, String(v)]);
      for (const [k, v] of pairs) append(list, "append", k, v);
    }
    headers.set(this, list);
  }
  ctor(Headers, {});
  methods(Headers.prototype, {
    append(n, v) {
      const list = hs(this, "append");
      needs("Headers", "append", arguments, 2);
      append(list, "append", n, v);
    },
    delete(n) {
      const list = hs(this, "delete");
      needs("Headers", "delete", arguments, 1);
      const key = name("delete", n);
      const kept = list.filter((p) => p[0] !== key);
      list.length = 0;
      list.push(...kept);
    },
    get(n) {
      const list = hs(this, "get");
      needs("Headers", "get", arguments, 1);
      const key = name("get", n);
      const values = list.filter((p) => p[0] === key).map((p) => p[1]);
      return values.length === 0 ? null : values.join(", ");
    },
    getSetCookie() {
      return hs(this, "getSetCookie")
        .filter((p) => p[0] === "set-cookie")
        .map((p) => p[1]);
    },
    has(n) {
      const list = hs(this, "has");
      needs("Headers", "has", arguments, 1);
      const key = name("has", n);
      return list.some((p) => p[0] === key);
    },
    set(n, v) {
      const list = hs(this, "set");
      needs("Headers", "set", arguments, 2);
      const key = name("set", n);
      const val = value("set", v);
      const i = list.findIndex((p) => p[0] === key);
      if (i === -1) list.push([key, val]);
      else {
        list[i] = [key, val];
        const kept = list.filter((p, j) => j <= i || p[0] !== key);
        list.length = 0;
        list.push(...kept);
      }
    },
    keys() {
      const list = hs(this, "keys");
      return iterate(headersIterator, () => combined(list), "keys");
    },
    values() {
      const list = hs(this, "values");
      return iterate(headersIterator, () => combined(list), "values");
    },
    entries() {
      const list = hs(this, "entries");
      return iterate(headersIterator, () => combined(list), "entries");
    },
    forEach(f) {
      const list = hs(this, "forEach");
      needs("Headers", "forEach", arguments, 1);
      callback(f, "Headers.forEach");
      forEachPair(this, combined(list), f, arguments[1]);
    },
  });
  hidden(Headers.prototype, Symbol.iterator, Headers.prototype.entries);
  Object.defineProperty(Headers.prototype, Symbol.toStringTag, { value: "Headers", configurable: true });
  global("Headers", Headers);

  // ----- UTF-8 --------------------------------------------------------------
  const utf8Encode = (text) => {
    const s = text.toWellFormed();
    const out = [];
    for (let i = 0; i < s.length; i++) {
      let c = s.charCodeAt(i);
      if (c >= 0xd800 && c <= 0xdbff) {
        c = 0x10000 + ((c - 0xd800) << 10) + (s.charCodeAt(++i) - 0xdc00);
      }
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
      else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
      else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
    return out;
  };
  const INVALID_UTF8 = "TextDecoder.decode: the input is not valid utf-8.";
  // The WHATWG UTF-8 decoder: each maximal invalid subpart becomes one U+FFFD (or, when fatal, a TypeError).
  const utf8Decode = (bytes, fatal) => {
    let out = "";
    let chunk = [];
    const flush = () => {
      out += String.fromCharCode.apply(null, chunk);
      chunk = [];
    };
    const bad = () => {
      if (fatal) throw new TypeError(INVALID_UTF8);
      chunk.push(0xfffd);
    };
    let i = 0;
    const n = bytes.length;
    while (i < n) {
      if (chunk.length > 4096) flush();
      const lead = bytes[i++];
      if (lead < 0x80) {
        chunk.push(lead);
        continue;
      }
      if (lead < 0xc2 || lead > 0xf4) {
        bad();
        continue;
      }
      const width = lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4;
      let cp = lead & (width === 2 ? 0x1f : width === 3 ? 0x0f : 0x07);
      let lower = lead === 0xe0 ? 0xa0 : lead === 0xf0 ? 0x90 : 0x80;
      let upper = lead === 0xed ? 0x9f : lead === 0xf4 ? 0x8f : 0xbf;
      let seen = 1;
      let ok = true;
      while (seen < width) {
        if (i >= n) {
          ok = false;
          break;
        }
        const b = bytes[i];
        if (b < lower || b > upper) {
          ok = false;
          break;
        }
        cp = (cp << 6) | (b & 0x3f);
        i++;
        seen++;
        lower = 0x80;
        upper = 0xbf;
      }
      if (!ok) {
        bad();
        continue;
      }
      if (cp >= 0x10000) {
        cp -= 0x10000;
        chunk.push(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
      } else chunk.push(cp);
    }
    flush();
    return out;
  };
  const bytesOf = (v) => {
    if (isTyped(v)) return new U8(v.buffer, v.byteOffset, v.byteLength);
    if (v instanceof ArrayBuffer) return new U8(v);
    if (v instanceof DataView) return new U8(v.buffer, v.byteOffset, v.byteLength);
    return null;
  };

  // ----- TextEncoder and TextDecoder ----------------------------------------
  const encoders = new WeakSet();
  function TextEncoder() {
    if (new.target === undefined) throw requiresNew("TextEncoder");
    encoders.add(this);
  }
  ctor(TextEncoder, {});
  methods(TextEncoder.prototype, {
    get encoding() {
      return "utf-8";
    },
    encode() {
      const input = arguments[0];
      return new U8(utf8Encode(input === undefined ? "" : String(input)));
    },
  });
  Object.defineProperty(TextEncoder.prototype, Symbol.toStringTag, { value: "TextEncoder", configurable: true });
  global("TextEncoder", TextEncoder);

  // Where a stream's decodable prefix ends; fatal mode also finds the first error.
  const streamBoundary = (bytes, fatal) => {
    let next = fatal ? 0 : Math.max(0, bytes.length - 3);
    while (next < bytes.length) {
      const start = next;
      const lead = bytes[next++];
      if (lead < 0x80) continue;
      if (lead < 0xc2 || lead > 0xf4) {
        if (fatal) return { end: next, error: true };
        continue;
      }
      const width = lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4;
      let lower = lead === 0xe0 ? 0xa0 : lead === 0xf0 ? 0x90 : 0x80;
      let upper = lead === 0xed ? 0x9f : lead === 0xf4 ? 0x8f : 0xbf;
      while (next < start + width) {
        if (next === bytes.length) return { end: start, error: false };
        const byte = bytes[next];
        if (byte < lower || byte > upper) {
          if (fatal) return { end: next, error: true };
          break;
        }
        next++;
        lower = 0x80;
        upper = 0xbf;
      }
    }
    return { end: next, error: false };
  };
  const LABELS = ["unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8", "utf-8", "utf8", "x-unicode20utf8"];
  const decoders = new WeakMap();
  const empty = new U8(0);
  function TextDecoder() {
    if (new.target === undefined) throw requiresNew("TextDecoder");
    const label = arguments[0] === undefined ? "utf-8" : String(arguments[0]).trim().toLowerCase();
    if (!LABELS.includes(label)) throw new RangeError(`The "${label}" encoding is not supported; only UTF-8 is.`);
    const options = arguments[1] !== null && typeof arguments[1] === "object" ? arguments[1] : undefined;
    const flag = (n) => options !== undefined && Boolean(options[n]);
    decoders.set(this, { fatal: flag("fatal"), ignoreBOM: flag("ignoreBOM"), queued: empty, seen: false });
  }
  ctor(TextDecoder, {});
  const ds = (self, name) => {
    const s = decoders.get(self);
    if (s === undefined) throw incompatible(`TextDecoder.prototype.${name}`, self);
    return s;
  };
  methods(TextDecoder.prototype, {
    get encoding() {
      ds(this, "encoding");
      return "utf-8";
    },
    get fatal() {
      return ds(this, "fatal").fatal;
    },
    get ignoreBOM() {
      return ds(this, "ignoreBOM").ignoreBOM;
    },
    decode() {
      const s = ds(this, "decode");
      const input = arguments[0];
      let bytes = input === undefined ? empty : bytesOf(input);
      if (bytes === null) throw new TypeError(`TextDecoder.decode expects a Uint8Array, received ${describe(input)}.`);
      const options = arguments[1];
      if (options !== undefined && typeof options !== "object") throw new TypeError("TextDecoder.decode expects an options object.");
      const stream = options !== null && options !== undefined && Boolean(options.stream);
      if (s.queued.length && bytes.length) {
        const joined = new U8(s.queued.length + bytes.length);
        joined.set(s.queued);
        joined.set(bytes, s.queued.length);
        bytes = joined;
      }
      if (!bytes.length) bytes = s.queued;
      const { end, error } = stream ? streamBoundary(bytes, s.fatal) : { end: bytes.length, error: false };
      s.queued = end < bytes.length ? bytes.slice(end) : empty;
      if (error) throw new TypeError(INVALID_UTF8);
      const strip = !s.ignoreBOM && !s.seen;
      if (!stream) s.seen = false;
      const text = utf8Decode(end === bytes.length ? bytes : bytes.subarray(0, end), s.fatal);
      if (stream && text.length) s.seen = true;
      return strip && text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
    },
  });
  Object.defineProperty(TextDecoder.prototype, Symbol.toStringTag, { value: "TextDecoder", configurable: true });
  global("TextDecoder", TextDecoder);

  // ----- base64 and hex -----------------------------------------------------
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  const encode64 = (bytes, alphabet = B64, pad = true) => {
    let out = "";
    for (let i = 0; i < bytes.length; i += 3) {
      const a = bytes[i];
      const b = i + 1 < bytes.length ? bytes[i + 1] : 0;
      const c = i + 2 < bytes.length ? bytes[i + 2] : 0;
      out += alphabet[a >> 2] + alphabet[((a & 3) << 4) | (b >> 4)];
      out += i + 1 < bytes.length ? alphabet[((b & 15) << 2) | (c >> 6)] : pad ? "=" : "";
      out += i + 2 < bytes.length ? alphabet[c & 63] : pad ? "=" : "";
    }
    return out;
  };
  const VALUES = new Map();
  for (let i = 0; i < 64; i++) {
    VALUES.set(B64[i], i);
    VALUES.set(B64URL[i], i);
  }
  // The forgiving-base64 decode of the HTML standard; null when the input is not base64.
  const decode64 = (text) => {
    let s = text.replace(/[\t\n\f\r ]/g, "");
    if (s.length % 4 === 0) s = s.replace(/==?$/, "");
    if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) return null;
    const out = [];
    let bits = 0;
    let count = 0;
    for (const ch of s) {
      bits = (bits << 6) | VALUES.get(ch);
      count += 6;
      if (count >= 8) {
        count -= 8;
        out.push((bits >> count) & 0xff);
      }
    }
    return out;
  };
  const latin1 = (bytes) => {
    let out = "";
    for (let i = 0; i < bytes.length; i += 4096) out += String.fromCharCode.apply(null, Array.from(bytes.subarray(i, i + 4096)));
    return out;
  };
  global("btoa", {
    btoa(data) {
      if (arguments.length === 0) throw new TypeError("btoa requires 1 argument (a string)");
      const s = String(data);
      const bytes = [];
      for (let i = 0; i < s.length; i++) {
        const c = s.charCodeAt(i);
        if (c > 0xff) throw new TypeError("The string contains invalid characters.");
        bytes.push(c);
      }
      return encode64(bytes);
    },
  }.btoa);
  global("atob", {
    atob(data) {
      if (arguments.length === 0) throw new TypeError("atob requires 1 argument (a string)");
      const bytes = decode64(String(data));
      if (bytes === null) throw new TypeError("The string contains invalid characters.");
      return latin1(new U8(bytes));
    },
  }.atob);
  const HEX = /^(?:[0-9a-fA-F]{2})*$/;
  const toHex = (bytes) => Array.from(bytes, (b) => (b < 16 ? "0" : "") + b.toString(16)).join("");
  // Defined on Uint8Array by the loader (web.rs), which calls these.
  exports.bytes = {
    fromBase64(string) {
      if (typeof string !== "string") throw new TypeError("Uint8Array.fromBase64 expects a string.");
      const bytes = /[-_]/.test(string) ? null : decode64(string);
      if (bytes === null) throw new SyntaxError("Uint8Array.fromBase64: the string is not valid base64.");
      return new U8(bytes);
    },
    fromHex(string) {
      if (typeof string !== "string") throw new TypeError("Uint8Array.fromHex expects a string.");
      if (!HEX.test(string)) throw new SyntaxError("Uint8Array.fromHex: the string is not valid hex.");
      const out = new U8(string.length / 2);
      for (let i = 0; i < out.length; i++) out[i] = parseInt(string.slice(2 * i, 2 * i + 2), 16);
      return out;
    },
    toBase64() {
      if (!isBytes(this)) throw incompatible("Uint8Array.prototype.toBase64", this);
      return encode64(this);
    },
    toHex() {
      if (!isBytes(this)) throw incompatible("Uint8Array.prototype.toHex", this);
      return toHex(this);
    },
  };

  // ----- crypto -------------------------------------------------------------
  const cryptoObject = {};
  methods(cryptoObject, {
    randomUUID() {
      const b = random(16);
      b[6] = (b[6] & 0x0f) | 0x40;
      b[8] = (b[8] & 0x3f) | 0x80;
      const h = toHex(b);
      return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
    },
    getRandomValues(array) {
      if (!isTyped(array) || /^(Float|Big)/.test(typedArrayTag.call(array))) {
        throw new TypeError(`crypto.getRandomValues expects a Uint8Array, received ${describe(array)}.`);
      }
      if (array.byteLength > 65536) throw new TypeError("crypto.getRandomValues fills at most 65536 bytes at a time.");
      new U8(array.buffer, array.byteOffset, array.byteLength).set(random(array.byteLength));
      return array;
    },
  });
  Object.defineProperty(cryptoObject, Symbol.toStringTag, { value: "Crypto", configurable: true });
  global("crypto", cryptoObject);

  // ----- structuredClone ----------------------------------------------------
  const brand = (f, v) => {
    try {
      f.call(v);
      return true;
    } catch {
      return false;
    }
  };
  const getter = (C, k) => Object.getOwnPropertyDescriptor(C.prototype, k).get;
  const mapSize = getter(Map, "size");
  const setSize = getter(Set, "size");
  const bufferLength = getter(ArrayBuffer, "byteLength");
  const regexpSource = getter(RegExp, "source");
  const ERRORS = ["Error", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError"];
  const uncloneable = (v) => new TypeError(`DataCloneError: ${describe(v)} could not be cloned.`);
  const opaque = (v) =>
    typeof v === "function" ||
    v instanceof Promise ||
    brand(WeakMap.prototype.has, v) ||
    brand(WeakSet.prototype.has, v) ||
    brand(WeakRef.prototype.deref, v) ||
    [Number, String, Boolean, Symbol, BigInt].some((C) => brand(C.prototype.valueOf, v));
  global("structuredClone", {
    structuredClone(value) {
      if (arguments.length === 0) throw new TypeError("structuredClone requires 1 argument");
      const memo = new Map();
      const clone = (v) => {
        if (typeof v === "symbol") throw uncloneable(v);
        if (v === null || (typeof v !== "object" && typeof v !== "function")) return v;
        if (memo.has(v)) return memo.get(v);
        const keep = (copy) => {
          memo.set(v, copy);
          return copy;
        };
        if (brand(mapSize, v)) {
          const copy = keep(new Map());
          for (const [k, item] of Map.prototype.entries.call(v)) copy.set(clone(k), clone(item));
          return copy;
        }
        if (brand(setSize, v)) {
          const copy = keep(new Set());
          for (const item of Set.prototype.values.call(v)) copy.add(clone(item));
          return copy;
        }
        if (brand(Date.prototype.getTime, v)) return keep(new Date(Date.prototype.getTime.call(v)));
        if (v !== RegExp.prototype && brand(regexpSource, v)) return keep(new RegExp(regexpSource.call(v), v.flags));
        if (isTyped(v)) {
          const C = G[typedArrayTag.call(v)];
          return keep(new C(v));
        }
        if (brand(bufferLength, v)) return keep(ArrayBuffer.prototype.slice.call(v, 0));
        if (v instanceof DataView) return keep(new DataView(v.buffer.slice(0), v.byteOffset, v.byteLength));
        if (Object.prototype.toString.call(v) === "[object Error]") {
          const n = v.name;
          const message = v.message;
          const C = G[typeof n === "string" && ERRORS.includes(n) ? n : "Error"];
          const copy = keep(message === undefined ? new C() : new C(String(message)));
          const stack = Object.getOwnPropertyDescriptor(v, "stack");
          if (stack && typeof stack.value === "string") hidden(copy, "stack", stack.value);
          if (Object.prototype.hasOwnProperty.call(v, "cause")) hidden(copy, "cause", clone(v.cause));
          return copy;
        }
        if (opaque(v)) throw uncloneable(v);
        const copy = keep(Array.isArray(v) ? new Array(v.length) : {});
        for (const k of Object.keys(v)) copy[k] = clone(v[k]);
        return copy;
      };
      return clone(value);
    },
  }.structuredClone);

  // ----- Buffer -------------------------------------------------------------
  const ENCODINGS = ["utf8", "utf-8", "hex", "base64", "base64url", "latin1", "binary", "ascii", "ucs2", "ucs-2", "utf16le", "utf-16le"];
  const nodeError = (C, message, code) => {
    const e = new C(message);
    if (code !== undefined) e.code = code;
    return e;
  };
  const isEncoding = (v) => typeof v === "string" && ENCODINGS.includes(v.toLowerCase());
  const encoding = (v) => {
    if (v === undefined || v === null) return "utf8";
    if (!isEncoding(v)) throw nodeError(TypeError, `Unknown encoding: ${String(v)}`, "ERR_UNKNOWN_ENCODING");
    const e = v.toLowerCase();
    return e === "utf-8" ? "utf8" : e === "binary" ? "latin1" : e === "ucs-2" || e === "ucs2" || e === "utf-16le" ? "utf16le" : e;
  };
  // Node's lenient decoders: hex stops at the first bad pair, base64 skips what is not base64.
  const encodeText = (s, enc) => {
    switch (enc) {
      case "utf8":
        return utf8Encode(s);
      case "hex": {
        const out = [];
        for (let i = 0; i + 1 < s.length; i += 2) {
          const pair = s.slice(i, i + 2);
          if (!/^[0-9a-fA-F]{2}$/.test(pair)) break;
          out.push(parseInt(pair, 16));
        }
        return out;
      }
      case "base64":
      case "base64url": {
        const out = [];
        let bits = 0;
        let count = 0;
        for (const ch of s) {
          if (ch === "=") break;
          const n = VALUES.get(ch);
          if (n === undefined) continue;
          bits = ((bits << 6) | n) & 0xffffff;
          count += 6;
          if (count >= 8) {
            count -= 8;
            out.push((bits >> count) & 0xff);
          }
        }
        return out;
      }
      case "latin1":
      case "ascii":
        return Array.from(s, (_, i) => s.charCodeAt(i) & 0xff);
      case "utf16le": {
        const out = [];
        for (let i = 0; i < s.length; i++) {
          const c = s.charCodeAt(i);
          out.push(c & 0xff, c >> 8);
        }
        return out;
      }
    }
  };
  const decodeText = (bytes, enc) => {
    switch (enc) {
      case "utf8":
        return utf8Decode(bytes, false);
      case "hex":
        return toHex(bytes);
      case "base64":
        return encode64(bytes);
      case "base64url":
        return encode64(bytes, B64URL, false);
      case "latin1":
        return latin1(bytes);
      case "ascii":
        return latin1(Array.from(bytes, (b) => b & 0x7f));
      case "utf16le": {
        let out = "";
        for (let i = 0; i + 1 < bytes.length; i += 2) out += String.fromCharCode(bytes[i] | (bytes[i + 1] << 8));
        return out;
      }
    }
  };
  let BufferProto;
  const wrap = (bytes, proto = BufferProto) => {
    const b = isBytes(bytes) ? bytes : new U8(bytes);
    Object.setPrototypeOf(b, proto);
    return b;
  };
  const bytesFrom = (value, second, third) => {
    if (typeof value === "number") return new U8(size(value));
    if (typeof value === "string") return encodeText(value, encoding(second));
    if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && value instanceof SharedArrayBuffer)) {
      const offset = second === undefined ? 0 : Number(second);
      return new U8(value, offset, third === undefined ? undefined : Number(third));
    }
    if (isTyped(value)) {
      const bytes = new U8(value.buffer, value.byteOffset, value.byteLength);
      const offset = second === undefined ? 0 : Number(second);
      const length = third === undefined ? undefined : Number(third);
      return U8.prototype.slice.call(bytes, offset, length === undefined ? undefined : offset + length);
    }
    if (Array.isArray(value)) return U8.from(value, Number);
    if (value !== null && typeof value === "object") {
      if (value.type === "Buffer" && Array.isArray(value.data)) return U8.from(value.data, Number);
      const length = value.length;
      if (typeof length === "number") return U8.from({ length }, (_, i) => Number(value[i]));
    }
    throw nodeError(
      TypeError,
      `The first argument must be of type string or an instance of Buffer, Uint8Array, or Array or an Array-like Object. Received ${describe(value)}`,
      "ERR_INVALID_ARG_TYPE",
    );
  };
  const size = (v) => {
    const n = Number(v);
    if (!Number.isInteger(n) || n < 0) throw nodeError(RangeError, `The argument 'size' is invalid. Received ${String(v)}`, "ERR_OUT_OF_RANGE");
    return n;
  };
  function Buffer(value, encodingOrOffset, length) {
    return wrap(bytesFrom(value, encodingOrOffset, length), new.target === undefined ? BufferProto : new.target.prototype);
  }
  BufferProto = Object.create(U8.prototype);
  ctor(Buffer, BufferProto);
  Object.setPrototypeOf(Buffer, U8);
  frozen(Buffer, "BYTES_PER_ELEMENT", 1);
  hidden(Buffer, "poolSize", 8192);
  const isBuffer = (v) => isBytes(v) && BufferProto.isPrototypeOf(v);
  const self = (v, name) => {
    if (!isBytes(v)) throw incompatible(`Buffer.prototype.${name}`, v);
    return v;
  };
  const fillBytes = (target, fill, start, end, enc) => {
    if (typeof fill === "string") {
      const bytes = encodeText(fill, encoding(enc));
      if (bytes.length === 0) {
        if (fill.length > 0) throw nodeError(TypeError, `The argument 'value' is invalid. Received '${fill}'`, "ERR_INVALID_ARG_VALUE");
        U8.prototype.fill.call(target, 0, start, end);
        return;
      }
      for (let i = start, j = 0; i < end; i++, j = (j + 1) % bytes.length) target[i] = bytes[j];
      return;
    }
    if (isTyped(fill)) {
      const bytes = new U8(fill.buffer, fill.byteOffset, fill.byteLength);
      if (bytes.length === 0) throw nodeError(TypeError, "The argument 'value' is invalid.", "ERR_INVALID_ARG_VALUE");
      for (let i = start, j = 0; i < end; i++, j = (j + 1) % bytes.length) target[i] = bytes[j];
      return;
    }
    U8.prototype.fill.call(target, Number(fill) & 255, start, end);
  };
  const compareBytes = (a, b) => {
    const n = Math.min(a.length, b.length);
    for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
    return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
  };
  methods(Buffer, {
    from(value, encodingOrOffset, length) {
      return wrap(bytesFrom(value, encodingOrOffset, length));
    },
    alloc(n, fill, enc) {
      const b = wrap(new U8(size(n)));
      if (fill !== undefined && fill !== 0) fillBytes(b, fill, 0, b.length, typeof fill === "string" ? enc : undefined);
      return b;
    },
    allocUnsafe(n) {
      return wrap(new U8(size(n)));
    },
    allocUnsafeSlow(n) {
      return wrap(new U8(size(n)));
    },
    byteLength(value, enc) {
      if (isTyped(value) || value instanceof DataView) return value.byteLength;
      if (value instanceof ArrayBuffer) return value.byteLength;
      const s = String(value);
      const e = encoding(enc);
      if (e === "base64" || e === "base64url") {
        let n = s.length;
        if (s.charCodeAt(n - 1) === 61) n--;
        if (n > 1 && s.charCodeAt(n - 1) === 61) n--;
        return (n * 3) >>> 2;
      }
      if (e === "hex") return s.length >>> 1;
      if (e === "latin1" || e === "ascii") return s.length;
      if (e === "utf16le") return s.length * 2;
      return utf8Encode(s).length;
    },
    concat(list, totalLength) {
      if (!Array.isArray(list)) throw new TypeError(`Buffer.concat expects an array of Buffers, received ${describe(list)}.`);
      const parts = list.map((item) => {
        if (isBytes(item)) return item;
        throw new TypeError(`Buffer.concat expects an array of Buffers, received ${describe(item)} in it.`);
      });
      const total = totalLength === undefined ? parts.reduce((sum, p) => sum + p.length, 0) : size(totalLength);
      const joined = new U8(total);
      let offset = 0;
      for (const p of parts) {
        if (offset >= total) break;
        joined.set(p.subarray(0, total - offset), offset);
        offset += p.length;
      }
      return wrap(joined);
    },
    isBuffer(v) {
      return isBuffer(v);
    },
    isEncoding(v) {
      return isEncoding(v);
    },
    compare(a, b) {
      if (!isBytes(a) || !isBytes(b)) throw new TypeError("Buffer.compare expects two Buffers or Uint8Arrays.");
      return compareBytes(a, b);
    },
  });
  const clampIndex = (v, length, fallback) => {
    if (v === undefined) return fallback;
    const i = Math.trunc(Number(v)) || 0;
    return i < 0 ? Math.max(0, length + i) : Math.min(i, length);
  };
  const view = (b, start, end) => wrap(U8.prototype.subarray.call(b, clampIndex(start, b.length, 0), clampIndex(end, b.length, b.length)));
  const needle = (value, enc) => {
    if (typeof value === "string") return encodeText(value, encoding(enc));
    if (isTyped(value)) return Array.from(new U8(value.buffer, value.byteOffset, value.byteLength));
    if (typeof value === "number") return [value & 255];
    throw nodeError(TypeError, `The "value" argument must be one of type number or string or an instance of Buffer or Uint8Array. Received ${describe(value)}`, "ERR_INVALID_ARG_TYPE");
  };
  const search = (b, value, offset, enc, last) => {
    if (typeof offset === "string") {
      enc = offset;
      offset = undefined;
    }
    const n = needle(value, enc);
    let start = offset === undefined || Number.isNaN(Number(offset)) ? (last ? b.length : 0) : Math.trunc(Number(offset));
    if (start < 0) start += b.length;
    if (!last) {
      start = Math.max(0, start);
      if (n.length === 0) return Math.min(start, b.length);
      for (let i = start; i + n.length <= b.length; i++) {
        let j = 0;
        while (j < n.length && b[i + j] === n[j]) j++;
        if (j === n.length) return i;
      }
      return -1;
    }
    if (start < 0) return -1;
    start = Math.min(start, b.length - n.length);
    if (n.length === 0) return Math.min(start, b.length);
    for (let i = start; i >= 0; i--) {
      let j = 0;
      while (j < n.length && b[i + j] === n[j]) j++;
      if (j === n.length) return i;
    }
    return -1;
  };
  const separators = (n) => {
    let s = String(n);
    if (!Number.isInteger(n) || Math.abs(n) <= 2 ** 32) return s;
    const sign = s.startsWith("-") ? "-" : "";
    s = s.slice(sign.length);
    let out = "";
    while (s.length > 3) {
      out = "_" + s.slice(-3) + out;
      s = s.slice(0, -3);
    }
    return sign + s + out;
  };
  const offsetAt = (b, offset, width) => {
    if (offset === undefined) offset = 0;
    if (typeof offset !== "number") throw nodeError(TypeError, `The "offset" argument must be of type number. Received ${describe(offset)}`, "ERR_INVALID_ARG_TYPE");
    if (!Number.isInteger(offset)) throw nodeError(RangeError, `The value of "offset" is out of range. It must be an integer. Received ${offset}`, "ERR_OUT_OF_RANGE");
    const max = b.length - width;
    if (max < 0) throw nodeError(RangeError, "Attempt to access memory outside buffer bounds", "ERR_BUFFER_OUT_OF_BOUNDS");
    if (offset < 0 || offset > max) throw nodeError(RangeError, `The value of "offset" is out of range. It must be >= 0 and <= ${max}. Received ${separators(offset)}`, "ERR_OUT_OF_RANGE");
    return offset;
  };
  const checkValue = (value, min, max) => {
    const v = Number(value);
    if (v < min || v > max) {
      throw nodeError(RangeError, `The value of "value" is out of range. It must be >= ${min} and <= ${max}. Received ${separators(v)}`, "ERR_OUT_OF_RANGE");
    }
    return v;
  };
  const dataView = (b) => new DataView(b.buffer, b.byteOffset, b.byteLength);
  const numeric = {};
  const fixed = [
    ["UInt8", 1, "Uint8"],
    ["Int8", 1, "Int8"],
    ["UInt16", 2, "Uint16"],
    ["Int16", 2, "Int16"],
    ["UInt32", 4, "Uint32"],
    ["Int32", 4, "Int32"],
    ["Float", 4, "Float32"],
    ["Double", 8, "Float64"],
  ];
  for (const [name, width, dv] of fixed) {
    const ends = width === 1 ? [""] : ["LE", "BE"];
    const signed = name.startsWith("Int");
    const bits = width * 8;
    const float = name === "Float" || name === "Double";
    const range = float ? null : signed ? [-(2 ** (bits - 1)), 2 ** (bits - 1) - 1] : [0, 2 ** bits - 1];
    for (const end of ends) {
      const little = end === "LE";
      const read = {
        [`read${name}${end}`](offset) {
          const b = self(this, `read${name}${end}`);
          return dataView(b)[`get${dv}`](offsetAt(b, offset, width), little);
        },
      }[`read${name}${end}`];
      const write = {
        [`write${name}${end}`](value, offset) {
          const b = self(this, `write${name}${end}`);
          const at = offsetAt(b, offset, width);
          const v = range ? checkValue(value, range[0], range[1]) : Number(value);
          dataView(b)[`set${dv}`](at, v, little);
          return at + width;
        },
      }[`write${name}${end}`];
      numeric[`read${name}${end}`] = read;
      numeric[`write${name}${end}`] = write;
      if (name.startsWith("UInt")) {
        numeric[`readUint${name.slice(4)}${end}`] = read;
        numeric[`writeUint${name.slice(4)}${end}`] = write;
      }
    }
  }
  const byteLengthArg = (n) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n < 1 || n > 6) {
      throw nodeError(RangeError, `The value of "byteLength" is out of range. It must be >= 1 and <= 6. Received ${n}`, "ERR_OUT_OF_RANGE");
    }
    return n;
  };
  for (const signed of [false, true]) {
    for (const end of ["LE", "BE"]) {
      const kind = signed ? "Int" : "UInt";
      const read = {
        [`read${kind}${end}`](offset, byteLength) {
          const b = self(this, `read${kind}${end}`);
          const n = byteLengthArg(byteLength);
          const at = offsetAt(b, offset, n);
          let v = 0;
          for (let i = 0; i < n; i++) v = v * 256 + b[end === "LE" ? at + n - 1 - i : at + i];
          if (signed && v >= 2 ** (8 * n - 1)) v -= 2 ** (8 * n);
          return v;
        },
      }[`read${kind}${end}`];
      const write = {
        [`write${kind}${end}`](value, offset, byteLength) {
          const b = self(this, `write${kind}${end}`);
          const n = byteLengthArg(byteLength);
          const at = offsetAt(b, offset, n);
          const bits = 8 * n;
          let v = signed ? checkValue(value, -(2 ** (bits - 1)), 2 ** (bits - 1) - 1) : checkValue(value, 0, 2 ** bits - 1);
          if (v < 0) v += 2 ** bits;
          for (let i = 0; i < n; i++) {
            b[end === "LE" ? at + i : at + n - 1 - i] = v % 256;
            v = Math.floor(v / 256);
          }
          return at + n;
        },
      }[`write${kind}${end}`];
      numeric[`read${kind}${end}`] = read;
      numeric[`write${kind}${end}`] = write;
      if (!signed) {
        numeric[`readUint${end}`] = read;
        numeric[`writeUint${end}`] = write;
      }
    }
  }
  methods(BufferProto, numeric);
  const swap = (b, width, name) => {
    if (b.length % width !== 0) throw nodeError(RangeError, `Buffer size must be a multiple of ${width * 8}-bits`, "ERR_INVALID_BUFFER_SIZE");
    for (let i = 0; i < b.length; i += width) {
      for (let j = 0; j < width / 2; j++) {
        const t = b[i + j];
        b[i + j] = b[i + width - 1 - j];
        b[i + width - 1 - j] = t;
      }
    }
    return b;
  };
  methods(BufferProto, {
    toString(enc, start, end) {
      const b = self(this, "toString");
      const s = start === undefined ? 0 : Math.max(0, Number(start) || 0);
      const e = end === undefined ? b.length : Math.min(b.length, Number(end) || 0);
      if (e <= s) return "";
      return decodeText(U8.prototype.subarray.call(b, s, e), encoding(enc));
    },
    toJSON() {
      return { type: "Buffer", data: Array.from(self(this, "toJSON")) };
    },
    slice(start, end) {
      return view(self(this, "slice"), start, end);
    },
    subarray(start, end) {
      return view(self(this, "subarray"), start, end);
    },
    equals(other) {
      const b = self(this, "equals");
      if (!isBytes(other)) {
        throw nodeError(TypeError, `The "otherBuffer" argument must be an instance of Buffer or Uint8Array. Received ${describe(other)}`, "ERR_INVALID_ARG_TYPE");
      }
      return compareBytes(b, other) === 0;
    },
    compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
      const b = self(this, "compare");
      if (!isBytes(target)) {
        throw nodeError(TypeError, `The "target" argument must be an instance of Buffer or Uint8Array. Received ${describe(target)}`, "ERR_INVALID_ARG_TYPE");
      }
      const t = U8.prototype.subarray.call(target, targetStart ?? 0, targetEnd ?? target.length);
      const s = U8.prototype.subarray.call(b, sourceStart ?? 0, sourceEnd ?? b.length);
      return compareBytes(s, t);
    },
    indexOf(value, byteOffset, enc) {
      return search(self(this, "indexOf"), value, byteOffset, enc, false);
    },
    lastIndexOf(value, byteOffset, enc) {
      return search(self(this, "lastIndexOf"), value, byteOffset, enc, true);
    },
    includes(value, byteOffset, enc) {
      return search(self(this, "includes"), value, byteOffset, enc, false) !== -1;
    },
    write(string, offset, length, enc) {
      const b = self(this, "write");
      if (typeof string !== "string") throw nodeError(TypeError, `The "string" argument must be of type string. Received ${describe(string)}`, "ERR_INVALID_ARG_TYPE");
      if (typeof offset === "string") {
        enc = offset;
        offset = 0;
        length = undefined;
      } else if (typeof length === "string") {
        enc = length;
        length = undefined;
      }
      const at = offset === undefined ? 0 : Math.trunc(Number(offset));
      if (at < 0 || at > b.length) throw nodeError(RangeError, `The value of "offset" is out of range. It must be >= 0 && <= ${b.length}. Received ${at}`, "ERR_OUT_OF_RANGE");
      const e = encoding(enc);
      const bytes = encodeText(string, e);
      let n = Math.min(bytes.length, b.length - at, length === undefined ? Infinity : Math.max(0, Math.trunc(Number(length))));
      if (e === "utf8") while (n > 0 && n < bytes.length && (bytes[n] & 0xc0) === 0x80) n--;
      for (let i = 0; i < n; i++) b[at + i] = bytes[i];
      return n;
    },
    fill(value, offset, end, enc) {
      const b = self(this, "fill");
      if (typeof offset === "string") {
        enc = offset;
        offset = undefined;
        end = undefined;
      } else if (typeof end === "string") {
        enc = end;
        end = undefined;
      }
      const s = offset === undefined ? 0 : Math.trunc(Number(offset));
      const e = end === undefined ? b.length : Math.trunc(Number(end));
      if (s < 0 || s > b.length) throw nodeError(RangeError, `The value of "offset" is out of range. It must be >= 0 && <= ${b.length}. Received ${s}`, "ERR_OUT_OF_RANGE");
      if (e < 0 || e > b.length) throw nodeError(RangeError, `The value of "end" is out of range. It must be >= 0 && <= ${b.length}. Received ${e}`, "ERR_OUT_OF_RANGE");
      if (s < e) fillBytes(b, typeof value === "boolean" ? Number(value) : value, s, e, enc);
      return this;
    },
    copy(target, targetStart, sourceStart, sourceEnd) {
      const b = self(this, "copy");
      if (!isTyped(target)) throw nodeError(TypeError, `The "target" argument must be an instance of Buffer or Uint8Array. Received ${describe(target)}`, "ERR_INVALID_ARG_TYPE");
      const t = new U8(target.buffer, target.byteOffset, target.byteLength);
      const ts = Math.max(0, Math.trunc(Number(targetStart ?? 0)) || 0);
      const ss = Math.max(0, Math.trunc(Number(sourceStart ?? 0)) || 0);
      const se = Math.min(b.length, sourceEnd === undefined ? b.length : Math.trunc(Number(sourceEnd)) || 0);
      if (ts >= t.length || ss >= se) return 0;
      const n = Math.min(se - ss, t.length - ts);
      t.set(U8.prototype.subarray.call(b, ss, ss + n), ts);
      return n;
    },
    swap16() {
      return swap(self(this, "swap16"), 2);
    },
    swap32() {
      return swap(self(this, "swap32"), 4);
    },
    swap64() {
      return swap(self(this, "swap64"), 8);
    },
  });
  global("Buffer", Buffer);
});
