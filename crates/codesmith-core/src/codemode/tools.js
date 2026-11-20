// The `tools` tree and `search` (see search.rs). Evaluates to a function
// that, given the natives, defines `search` and returns the tree builder.
(function install(native) {
  "use strict";
  const { call, search, closest, within } = native;
  Object.defineProperty(globalThis, "search", {
    value: {
      search(query) {
        return search(query === undefined ? null : query);
      },
    }.search,
    writable: true,
    configurable: true,
    enumerable: false,
  });
  const expression = (path) =>
    "tools" + path.map((s) => (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(s) ? "." + s : "[" + JSON.stringify(s) + "]")).join("");
  const disjunction = (items) =>
    items.length <= 2 ? items.join(" or ") : items.slice(0, -1).join(", ") + ", or " + items[items.length - 1];
  const failure = (message, suggestion) => new Error(suggestion ? `${message} ${suggestion}` : message);
  const unknown = (path, root) => {
    const name = path.join(".");
    const ns = path.length > 1 && root.children.has(path[0]) ? path[0] : "";
    const best = closest(ns, ns ? path.slice(1).join(" ") : name)[0];
    return failure(
      best ? `Unknown tool '${name}'. Did you mean ${expression(best.split("."))}?` : `Unknown tool '${name}'.`,
      "Use search to find available tools.",
    );
  };
  const notCallable = (path) => {
    const name = path.join(".");
    const under = within(name);
    const ranked = closest(name, path[path.length - 1]);
    const shown = [...ranked, ...under.filter((p) => !ranked.includes(p))].slice(0, 3).map((p) => expression(p.split(".")));
    const more = under.length - shown.length;
    return failure(
      shown.length === 0
        ? `Tool '${name}' is not callable.`
        : `Tool '${name}' is not callable, it is a namespace. Did you mean ${disjunction(shown)}?`,
      more > 0
        ? `${more} more under '${name}'. Use search({ namespace: ${JSON.stringify(name)} }) to list them.`
        : "Use search to find available tools.",
    );
  };
  // Names programs reach for that are not tool paths: promise checks, JSON, conversions.
  const passthrough = new Set(["then", "toJSON"]);
  return function tools(list) {
    const root = { children: new Map(), tool: undefined };
    for (const [tool, path] of list) {
      let node = root;
      for (const segment of path.split(".")) {
        if (!node.children.has(segment)) node.children.set(segment, { children: new Map(), tool: undefined });
        node = node.children.get(segment);
      }
      node.tool = tool;
    }
    const made = new Map();
    const reference = (node, path) => {
      const key = path.join("\u0000");
      if (made.has(key)) return made.get(key);
      const target = path.length === 0 ? {} : () => {};
      const proxy = new Proxy(target, {
        get(t, k) {
          if (typeof k === "symbol") return undefined;
          if (node !== undefined && node.children.has(k)) return reference(node.children.get(k), [...path, ...k.split(".")]);
          if (passthrough.has(k)) return undefined;
          if (k in t) return Reflect.get(t, k);
          // A dotted key spells nested segments: tools["api.issues"] is tools.api.issues.
          const segments = k.split(".");
          let next = node;
          for (const s of segments) next = next === undefined ? undefined : next.children.get(s);
          return reference(next, [...path, ...segments]);
        },
        has(t, k) {
          return typeof k === "string" && node !== undefined && node.children.has(k);
        },
        ownKeys() {
          if (node === undefined) throw failure(`Unknown tool namespace '${path.join(".")}'.`, "Use search to find available tools.");
          return [...node.children.keys()];
        },
        getOwnPropertyDescriptor(t, k) {
          if (typeof k !== "string" || node === undefined || !node.children.has(k)) return undefined;
          return { value: reference(node.children.get(k), [...path, k]), writable: false, enumerable: true, configurable: true };
        },
        set() {
          return false;
        },
        defineProperty() {
          return false;
        },
        deleteProperty() {
          return false;
        },
        apply(t, self, args) {
          return (async () => {
            if (node === undefined) throw unknown(path, root);
            if (node.tool === undefined) throw notCallable(path);
            if (args.length > 1) throw failure(`Tool '${path.join(".")}' expects at most one input object.`);
            return call(node.tool, args[0] === undefined ? {} : args[0]);
          })();
        },
      });
      made.set(key, proxy);
      return proxy;
    };
    return reference(root, []);
  };
});
