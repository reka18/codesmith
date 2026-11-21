// Code Mode's `fetch` and `Response` globals (see fetch.rs). The host makes
// the request with the webfetch tool's checks; a response arrives whole.
(() => {
  "use strict";
  const G = globalThis;
  const call = G.__call;
  const TOOL = "\u0001fetch";
  const state = new WeakMap();
  const nullBody = (status) => status === 101 || status === 204 || status === 205 || status === 304;
  const hidden = (o, k, v) => Object.defineProperty(o, k, { value: v, writable: true, configurable: true, enumerable: false });
  const own = (r, name) => {
    const s = state.get(r);
    if (s === undefined) throw new TypeError(`Response.${name} called on an incompatible receiver.`);
    return s;
  };
  const encode = (text) => new G.TextEncoder().encode(text);
  // The body's bytes, once: a second read fails as it does for a stream.
  const consume = (r, name) => {
    const s = own(r, name);
    if (s.used || s.locked) throw new TypeError("Body has already been used.");
    s.used = true;
    return s.bytes === null ? new Uint8Array(0) : s.bytes.slice();
  };
  // A minimal readable body: one chunk, then done.
  const stream = (s) => {
    if (s.stream !== undefined) return s.stream;
    const reader = () => {
      if (s.locked) throw new TypeError("The body is locked to a reader.");
      if (s.used) throw new TypeError("Body has already been used.");
      s.locked = true;
      let sent = false;
      return {
        read: async () => {
          if (sent || s.bytes === null || s.bytes.length === 0) {
            s.used = true;
            return { done: true, value: undefined };
          }
          sent = true;
          s.used = true;
          return { done: false, value: s.bytes.slice() };
        },
        cancel: async () => {
          s.used = true;
          sent = true;
        },
        releaseLock: () => {
          s.locked = false;
        },
        get closed() {
          return Promise.resolve(undefined);
        },
      };
    };
    const body = {
      getReader: reader,
      get locked() {
        return s.locked;
      },
      cancel: async () => {
        s.used = true;
      },
      [Symbol.asyncIterator]() {
        const r = reader();
        return {
          next: () => r.read(),
          return: async () => {
            r.releaseLock();
            return { done: true, value: undefined };
          },
          [Symbol.asyncIterator]() {
            return this;
          },
        };
      },
    };
    s.stream = body;
    return body;
  };
  const make = (s) => {
    const r = Object.create(Response.prototype);
    state.set(r, s);
    return r;
  };
  const toBytes = (body, headers) => {
    if (body === null || body === undefined) return null;
    const type = (t) => {
      if (!headers.has("content-type")) headers.set("content-type", t);
    };
    if (typeof body === "string") {
      type("text/plain;charset=UTF-8");
      return encode(body);
    }
    if (body instanceof G.URLSearchParams) {
      type("application/x-www-form-urlencoded;charset=UTF-8");
      return encode(body.toString());
    }
    if (body instanceof ArrayBuffer) return new Uint8Array(body.slice(0));
    if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength));
    type("text/plain;charset=UTF-8");
    return encode(String(body));
  };

  class Response {
    constructor(body = null, init = {}) {
      const status = init.status === undefined ? 200 : Number(init.status);
      if (!Number.isInteger(status) || status < 200 || status > 599) throw new RangeError(`Invalid response status ${init.status}.`);
      const headers = new G.Headers(init.headers);
      const bytes = toBytes(body, headers);
      if (bytes !== null && nullBody(status)) throw new TypeError(`Response with status ${status} cannot have a body.`);
      state.set(this, {
        status,
        statusText: init.statusText === undefined ? "" : String(init.statusText),
        url: "",
        redirected: false,
        headers,
        bytes,
        used: false,
        locked: false,
      });
    }
    static json(data, init = {}) {
      const headers = new G.Headers(init.headers);
      if (!headers.has("content-type")) headers.set("content-type", "application/json");
      return new Response(JSON.stringify(data), { ...init, headers });
    }
    get status() {
      return own(this, "status").status;
    }
    get statusText() {
      return own(this, "statusText").statusText;
    }
    get ok() {
      const status = own(this, "ok").status;
      return status >= 200 && status < 300;
    }
    get url() {
      return own(this, "url").url;
    }
    get redirected() {
      return own(this, "redirected").redirected;
    }
    get type() {
      own(this, "type");
      return "basic";
    }
    get headers() {
      return own(this, "headers").headers;
    }
    get bodyUsed() {
      return own(this, "bodyUsed").used;
    }
    get body() {
      const s = own(this, "body");
      return s.bytes === null ? null : stream(s);
    }
    async arrayBuffer() {
      return consume(this, "arrayBuffer").buffer;
    }
    async bytes() {
      return consume(this, "bytes");
    }
    async text() {
      return new G.TextDecoder().decode(consume(this, "text"));
    }
    async json() {
      return JSON.parse(new G.TextDecoder().decode(consume(this, "json")));
    }
    clone() {
      const s = own(this, "clone");
      if (s.used || s.locked) throw new TypeError("Response.clone: the body has already been used.");
      return make({ ...s, headers: new G.Headers(s.headers), stream: undefined });
    }
  }
  hidden(Response.prototype, Symbol.toStringTag, "Response");

  const STANDARD = ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"];
  async function fetch(input, init) {
    init = init === undefined || init === null ? {} : init;
    let url;
    if (input instanceof G.URL) url = input.href;
    else if (input !== null && typeof input === "object" && typeof input.url === "string") url = input.url;
    else url = String(input);
    let method = init.method === undefined ? "GET" : String(init.method);
    if (STANDARD.includes(method.toUpperCase())) method = method.toUpperCase();
    const headers = new G.Headers(init.headers);
    if (init.body !== undefined && init.body !== null && (method === "GET" || method === "HEAD"))
      throw new TypeError("Request with GET/HEAD method cannot have body.");
    const bytes = toBytes(init.body, headers);
    let r;
    try {
      r = await call(TOOL, { url, method, headers: Array.from(headers), body: bytes === null ? null : bytes.toBase64() });
    } catch (e) {
      // The host's message, without the internal call name before it.
      const message = String(e && e.message);
      throw new Error(message.startsWith(TOOL + ": ") ? message.slice(TOOL.length + 2) : message);
    }
    const bodyless = method === "HEAD" || nullBody(r.status);
    return make({
      status: r.status,
      statusText: r.statusText,
      url: r.url,
      redirected: r.redirected,
      headers: new G.Headers(r.headers),
      bytes: bodyless ? null : Uint8Array.fromBase64(r.body),
      used: false,
      locked: false,
    });
  }
  Object.defineProperty(G, "fetch", { value: fetch, writable: true, configurable: true, enumerable: false });
  Object.defineProperty(G, "Response", { value: Response, writable: true, configurable: true, enumerable: false });
})();
