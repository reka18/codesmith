// Code Mode's `console`, as OpenCode's Code Mode formats it
// (`packages/codemode/src/language/stdlib/console.ts`, MIT): every method
// appends one line to `__logs`. A value is shown without running program
// code: an accessor shows as `[Getter]`, `[Setter]` or `[Getter/Setter]`,
// a proxy's traps are not called, and `toJSON` is not consulted. `Map`,
// `Set`, errors, class instances and classes show what they are; a cycle
// shows as `[Circular]` and anything nested deeper than 32 levels as `...`.
// One line is cut at `MAX_LINE` characters, so a value that fans out
// through shared references cannot grow without bound.
const __logs = [];
const console = (() => {
  "use strict";
  const kind = __kind;
  const MAX_DEPTH = 32;
  const MAX_LINE = 100000;
  const apply = Reflect.apply;
  const ownKeys = Reflect.ownKeys;
  const getProto = Object.getPrototypeOf;
  const describe = Object.getOwnPropertyDescriptor;
  const defineProperty = Object.defineProperty;
  const isArray = Array.isArray;
  const stringify = JSON.stringify;
  const toStr = String;
  const FunctionProto = Function.prototype;
  const slice = String.prototype.slice;
  const join = Array.prototype.join;
  const getter = (o, k) => describe(o, k).get;
  const mapSize = getter(Map.prototype, "size");
  const mapEntries = Map.prototype.entries;
  const setSize = getter(Set.prototype, "size");
  const setValues = Set.prototype.values;
  const mapNext = getProto(new Map().entries()).next;
  const setNext = getProto(new Set().values()).next;
  const weakMapHas = WeakMap.prototype.has;
  const weakSetHas = WeakSet.prototype.has;
  const dateTime = Date.prototype.getTime;
  const dateISO = Date.prototype.toISOString;
  const regexpSource = getter(RegExp.prototype, "source");
  const regexpFlags = getter(RegExp.prototype, "flags");
  const typedTag = getter(getProto(Int8Array.prototype), Symbol.toStringTag);
  const typedLength = getter(getProto(Int8Array.prototype), "length");
  const bufferLength = getter(ArrayBuffer.prototype, "byteLength");
  const isFinite = Number.isFinite;
  let webInspect;

  // web.js hands over how its values show (URL, Headers, Buffer, ...) when
  // it loads; the slot is fixed so a program cannot take it.
  defineProperty(globalThis, "__consoleInspect", {
    value: (f) => {
      if (webInspect === undefined) webInspect = f;
    },
    writable: false,
    enumerable: false,
    configurable: false,
  });

  // A brand check: true when `f` accepts `v` as its receiver.
  const is = (f, v, ...args) => {
    try {
      apply(f, v, args);
      return true;
    } catch {
      return false;
    }
  };

  // An own data property's value (`undefined` for an accessor or none).
  const ownData = (o, k) => {
    const d = describe(o, k);
    return d !== undefined && "value" in d ? d.value : undefined;
  };
  // A data property's value along the prototype chain, as Error.prototype.toString
  // reads it, without running a getter or a proxy trap.
  const peek = (o, k) => {
    for (let p = o; p !== null && kind(p) !== "proxy"; p = getProto(p)) {
      const d = describe(p, k);
      if (d !== undefined) return "value" in d ? d.value : undefined;
    }
    return undefined;
  };
  const label = (d) => (d.get !== undefined && d.set !== undefined ? "[Getter/Setter]" : d.get !== undefined ? "[Getter]" : "[Setter]");
  // Own enumerable string-keyed entries, an accessor as its label.
  const shownEntries = (o) => {
    const out = [];
    const keys = ownKeys(o);
    for (let i = 0; i < keys.length; i++) {
      const k = keys[i];
      if (typeof k !== "string") continue;
      const d = describe(o, k);
      if (d === undefined || !d.enumerable) continue;
      out[out.length] = [k, "value" in d ? d.value : label(d)];
    }
    return out;
  };

  // Boa keeps no source text for a class, but only a class gives the
  // functions a program writes a read-only `prototype`.
  const isClass = (f) => {
    if (kind(f) !== "function") return false;
    const d = describe(f, "prototype");
    return d !== undefined && "value" in d && !d.writable;
  };
  const ownName = (o) => {
    const n = ownData(o, "name");
    return typeof n === "string" ? n : "";
  };
  // The class an instance was made by, when its prototype's constructor is one.
  const className = (o) => {
    const proto = getProto(o);
    if (proto === null || kind(proto) === "proxy") return "";
    const c = ownData(proto, "constructor");
    if (typeof c !== "function" || kind(c) === "proxy" || !isClass(c)) return "";
    return ownName(c);
  };
  const errorText = (e) => {
    const name = peek(e, "name");
    const message = peek(e, "message");
    const n = typeof name === "string" ? name : "Error";
    const m = typeof message === "string" ? message : "";
    if (m === "") return n;
    if (n === "") return m;
    return n + ": " + m;
  };

  class Full {}
  // Formats into one bounded string.
  const format = (value, top) => {
    const parts = [];
    let count = 0;
    let size = 0;
    // The objects being shown, outermost first.
    const seen = [];
    let open = 0;
    const emit = (s) => {
      parts[count++] = s;
      size += s.length;
      if (size > MAX_LINE) throw new Full();
    };
    const joined = () => apply(join, parts, [""]);
    const item = (v, depth) => {
      if (v === null || v === undefined) return emit("null");
      const t = typeof v;
      if (t === "string") return emit(stringify(v));
      if (t !== "object" && t !== "function") return emit(toStr(v));
      const k = kind(v);
      if (k === "proxy") return emit(t === "function" ? "[opaque reference]" : "[Proxy]");
      if (t === "function") {
        if (!isClass(v)) return emit("[opaque reference]");
        const name = ownName(v) || "(anonymous)";
        const parent = getProto(v);
        const base = parent !== FunctionProto && typeof parent === "function" && kind(parent) !== "proxy" ? ownName(parent) : "";
        return emit(base === "" ? `[class ${name}]` : `[class ${name} extends ${base}]`);
      }
      if (k === "error") return emit(errorText(v));
      if (k === "promise") return emit("[Promise (await it to get its value)]");
      if (depth > MAX_DEPTH) return emit("...");
      for (let i = 0; i < open; i++) if (seen[i] === v) return emit("[Circular]");
      seen[open++] = v;
      try {
        body(v, depth);
      } finally {
        seen[--open] = undefined;
      }
    };
    const body = (v, depth) => {
      const web = webInspect === undefined ? undefined : webInspect(v);
      if (web !== undefined) return emit(web);
      if (isArray(v)) {
        emit("[");
        const n = ownData(v, "length");
        for (let i = 0; i < n; i++) {
          if (i > 0) emit(",");
          const d = describe(v, toStr(i));
          if (d === undefined) emit("null");
          else if ("value" in d) item(d.value, depth + 1);
          else emit(label(d));
        }
        return emit("]");
      }
      if (is(mapSize, v)) {
        emit(`Map(${apply(mapSize, v, [])}) [`);
        const it = apply(mapEntries, v, []);
        let first = true;
        for (let r = apply(mapNext, it, []); !r.done; r = apply(mapNext, it, [])) {
          if (!first) emit(",");
          first = false;
          emit("[");
          item(r.value[0], depth + 1);
          emit(",");
          item(r.value[1], depth + 1);
          emit("]");
        }
        return emit("]");
      }
      if (is(setSize, v)) {
        emit(`Set(${apply(setSize, v, [])}) [`);
        const it = apply(setValues, v, []);
        let first = true;
        for (let r = apply(setNext, it, []); !r.done; r = apply(setNext, it, [])) {
          if (!first) emit(",");
          first = false;
          item(r.value, depth + 1);
        }
        return emit("]");
      }
      if (is(weakMapHas, v, {})) return emit("WeakMap { <items unknown> }");
      if (is(weakSetHas, v, {})) return emit("WeakSet { <items unknown> }");
      if (is(dateTime, v)) return emit(isFinite(apply(dateTime, v, [])) ? apply(dateISO, v, []) : "Invalid Date");
      if (is(regexpSource, v)) return emit(`/${apply(regexpSource, v, [])}/${apply(regexpFlags, v, [])}`);
      const tag = apply(typedTag, v, []);
      if (tag !== undefined) {
        const n = apply(typedLength, v, []);
        emit(`${tag}(${n}) [`);
        for (let i = 0; i < n; i++) {
          if (i > 0) emit(",");
          emit(toStr(v[i]));
        }
        return emit("]");
      }
      if (is(bufferLength, v)) return emit(`ArrayBuffer(${apply(bufferLength, v, [])})`);
      const name = className(v);
      if (name !== "") emit(name + " ");
      emit("{");
      const keys = ownKeys(v);
      let first = true;
      for (let i = 0; i < keys.length; i++) {
        const key = keys[i];
        if (typeof key !== "string") continue;
        const d = describe(v, key);
        if (d === undefined || !d.enumerable) continue;
        if (!first) emit(",");
        first = false;
        emit(stringify(key) + ":");
        if ("value" in d) item(d.value, depth + 1);
        else emit(label(d));
      }
      emit("}");
    };
    try {
      if (top && value === undefined) return "undefined";
      if (top && typeof value === "string") return value;
      item(value, 0);
      return joined();
    } catch (e) {
      if (e instanceof Full) return apply(slice, joined(), [0, MAX_LINE]) + "... (cut)";
      throw e;
    }
  };
  // One value as `console.log` shows it.
  const show = (v) => format(v, true);
  const cell = (v) => (v === undefined ? "" : typeof v === "string" ? v : format(v, false));

  // A table row's column as display shows it: an accessor, own or inherited, by its label.
  const column = (o, k) => {
    for (let p = o; p !== null && kind(p) !== "proxy"; p = getProto(p)) {
      const d = describe(p, k);
      if (d !== undefined) return "value" in d ? d.value : label(d);
    }
    return undefined;
  };
  const rowValues = (v, columns) => {
    if (v !== null && typeof v === "object" && kind(v) !== "proxy" && !isArray(v)) {
      const out = [];
      if (columns !== undefined) for (let i = 0; i < columns.length; i++) out[i] = [columns[i], column(v, columns[i])];
      else return shownEntries(v);
      return out;
    }
    return [["Value", v]];
  };
  const table = (data, columnsArg) => {
    if (data === undefined) return "undefined";
    let columns;
    if (isArray(columnsArg)) {
      columns = [];
      for (let i = 0; i < columnsArg.length; i++) columns[i] = toStr(columnsArg[i]);
    }
    const rows = [];
    if (isArray(data)) {
      const n = ownData(data, "length");
      for (let i = 0; i < n; i++) rows[i] = [toStr(i), rowValues(column(data, toStr(i)), columns)];
    } else if (data !== null && typeof data === "object" && kind(data) !== "proxy") {
      const entries = shownEntries(data);
      for (let i = 0; i < entries.length; i++) rows[i] = [entries[i][0], rowValues(entries[i][1], columns)];
    } else rows[0] = ["0", [["Value", data]]];
    let keys = columns;
    if (keys === undefined) {
      keys = [];
      for (let i = 0; i < rows.length; i++)
        for (let x = 0; x < rows[i][1].length; x++) {
          const k = rows[i][1][x][0];
          let found = false;
          for (let j = 0; j < keys.length; j++) if (keys[j] === k) found = true;
          if (!found) keys[keys.length] = k;
        }
    }
    let out = "(index)";
    for (let j = 0; j < keys.length; j++) out += "\t" + keys[j];
    for (let i = 0; i < rows.length; i++) {
      out += "\n" + rows[i][0];
      const values = rows[i][1];
      for (let j = 0; j < keys.length; j++) {
        let v;
        for (let x = 0; x < values.length; x++) if (values[x][0] === keys[j]) v = values[x][1];
        out += "\t" + cell(v);
        if (out.length > MAX_LINE) return apply(slice, out, [0, MAX_LINE]) + "... (cut)";
      }
    }
    return out;
  };

  const line = (name, args) => {
    if (name === "dir") return args.length === 0 ? "undefined" : show(args[0]);
    if (name === "table") return table(args[0], args[1]);
    const prefix = name === "warn" ? "[warn] " : name === "error" ? "[error] " : name === "debug" ? "[debug] " : "";
    let out = prefix;
    for (let i = 0; i < args.length; i++) out += (i > 0 ? " " : "") + show(args[i]);
    return out;
  };
  const console = {};
  for (const name of ["log", "info", "debug", "warn", "error", "dir", "table"]) {
    const method = {
      [name](...args) {
        __logs[__logs.length] = line(name, args);
      },
    }[name];
    defineProperty(console, name, { value: method, writable: true, enumerable: true, configurable: true });
  }
  return console;
})();
