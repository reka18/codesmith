//! OpenCode's Code Mode language tests (`packages/codemode/test`, MIT; the
//! Test262-derived ones under the BSD license in
//! `fixtures/LICENSE.test262`, the web-platform-tests-derived ones under the
//! BSD license in `fixtures/LICENSE.wpt`), run through CodeSmith's Code Mode.
//!
//! OpenCode runs programs on its own interpreter, CodeSmith on Boa, so these
//! pin the JavaScript a model's program can rely on: each case is a program
//! from an OpenCode test and the value (or failure) it expects.
//! `fixtures/oc_codemode.json` holds the programs and expected values,
//! extracted as text from the test files, or, where a test computes them,
//! recorded by running the test file under Node with Code Mode stubbed out
//! (V8 only steers the tests' helpers; no OpenCode source runs). Only
//! programs that use no tools and no OpenCode-specific host API are
//! included. A case with a `path` asserts on part of the result, and an
//! expected `{"$prog": ...}` is another program's value. A value crosses back as JSON
//! does in OpenCode's results (`undefined` and non-finite numbers inside it
//! become `null`, and undefined fields are dropped), except that `-0`
//! stays distinct.
//!
//! Cases Boa gets wrong, or where OpenCode deliberately differs from
//! standard JavaScript, are listed in [`KNOWN`] with the reason; the test
//! fails if any other case fails, and also if a listed one starts passing
//! (so the list stays accurate). See `crates/codesmith/PARITY.md`.

use codesmith_core::codemode;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const CANON: &str = r#"
function __canon(v, seen) {
  seen = seen || [];
  if (v === undefined || typeof v === "function" || typeof v === "symbol") return null;
  if (v === null) return null;
  const t = typeof v;
  if (t === "number") {
    if (!Number.isFinite(v)) return null;
    if (v === 0 && 1 / v < 0) return { $n: "-0" };
    return v;
  }
  if (t === "string" || t === "boolean") return v;
  if (t === "bigint") return { $big: v.toString() };
  if (seen.indexOf(v) >= 0) return { $cycle: 1 };
  seen = seen.concat([v]);
  if (v instanceof Date) return isNaN(v.getTime()) ? null : v.toISOString();
  if (typeof URL !== "undefined" && v instanceof URL) return v.href;
  if (typeof URLSearchParams !== "undefined" && v instanceof URLSearchParams) return v.toString();
  if (v instanceof Set) return Array.from(v, (x) => __canon(x, seen));
  if (v instanceof Map || v instanceof RegExp) return {};
  if (Array.isArray(v)) {
    const out = [];
    for (let i = 0; i < v.length; i++) out.push(__canon(v[i], seen));
    return out;
  }
  if (typeof v.toJSON === "function") return __canon(v.toJSON(), seen);
  const out = {};
  for (const k of Object.keys(v)) {
    const x = v[k];
    if (x === undefined || typeof x === "function" || typeof x === "symbol") continue;
    out[k] = __canon(x, seen);
  }
  return out;
}
"#;

/// OpenCode's interpreter restricts or converts values on purpose: opaque
/// function and iterator references, cycles, typed arrays at the boundary,
/// errors returned as `{ name, message }`, promises awaited inside results,
/// deterministic UTC dates and `en-US` locale formatting, empty programs.
const BOUNDARY: &str = "OpenCode-specific restriction or result conversion";
/// Strict-mode checks where Boa's strict mode (Code Mode runs programs in
/// strict mode, as OpenCode applies its checks) still differs: other
/// wording for writes to frozen objects, an early `SyntaxError` for what
/// OpenCode accepts (`delete` of a variable, a parameter named
/// `arguments`), and no `TypeError` for writing a named function
/// expression's own name or, in one case, a frozen array.
const STRICT: &str = "strict-mode checks Boa's strict mode differs on";
/// The same error, worded differently by Boa.
const MESSAGE: &str = "Boa's error message wording";
/// CodeSmith's own limits: 400 nested calls (OpenCode allows 10000) and
/// 64 MiB buffers.
const LIMIT: &str = "CodeSmith's limits";
/// Where Boa 0.22 departs from the specification or lacks a recent
/// addition (Set methods on Set-likes, `Error.isError`, the RegExp `d` flag,
/// `substr`, `super` in arrows, some TDZ and property-order cases, ...).
const ENGINE: &str = "Boa differs from the specification";
/// Boa's result depends on the local time zone: it passes in UTC (CI) and
/// fails elsewhere, so either outcome is accepted.
const TIMEZONE: &str = "Boa's answer depends on the local time zone";

/// Cases that fail on purpose, by file, test name and assertion number,
/// with the reason.
const KNOWN: &[(&str, &str, usize, &str)] = &[
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.fromEntries accepts every supported entry collection"#,
        1,
        BOUNDARY,
    ),
    (
        "callbacks.test.ts",
        r#"constructors callable without new, like JS > Date() without new returns a deterministic ISO string and ignores arguments"#,
        1,
        BOUNDARY,
    ),
    (
        "callbacks.test.ts",
        r#"constructors callable without new, like JS > Object returns objects unchanged and rejects primitive wrappers"#,
        3,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > caught, uncaught, returned, logged and stringified like plain errors"#,
        2,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > caught, uncaught, returned, logged and stringified like plain errors"#,
        3,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > caught, uncaught, returned, logged and stringified like plain errors"#,
        4,
        BOUNDARY,
    ),
    (
        "descriptors.test.ts",
        r#"arrays > elements take only the array's shared attributes, never accessors"#,
        1,
        BOUNDARY,
    ),
    (
        "descriptors.test.ts",
        r#"arrays > elements take only the array's shared attributes, never accessors"#,
        2,
        BOUNDARY,
    ),
    (
        "errors.test.ts",
        r#"source syntax > rejects empty code"#,
        1,
        BOUNDARY,
    ),
    (
        "new-expression.test.ts",
        r#"new on a non-constructible callee > built-in functions without construction point at the plain call"#,
        1,
        BOUNDARY,
    ),
    (
        "new-expression.test.ts",
        r#"new on a non-constructible callee > the failure is a catchable TypeError"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"Error values and instanceof > interpreter failures are TypeErrors; unsupported syntax is a SyntaxError"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"String and Number method arguments convert through ToPrimitive > conversion failures surface and opaque arguments still reject"#,
        4,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"ToPrimitive: operators and conversions honor program valueOf and toString > functions and other opaque values still reject arithmetic, and an object without a primitive form throws"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"ToPrimitive: operators and conversions honor program valueOf and toString > functions and other opaque values still reject arithmetic, and an object without a primitive form throws"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: unknown static members read as undefined > feature detection on missing statics works like native JS"#,
        8,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"computed property keys convert through the object's own toString > opaque values keep their built-in key form and a tool reference toString is never called"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"loose equality and operator gates on opaque references > coercing an opaque reference against a non-nullish primitive still rejects"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"loose equality and operator gates on opaque references > coercing an opaque reference against a non-nullish primitive still rejects"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"loose equality and operator gates on opaque references > coercing an opaque reference against a non-nullish primitive still rejects"#,
        3,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"loose equality and operator gates on opaque references > coercing an opaque reference against a non-nullish primitive still rejects"#,
        4,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"loose equality and operator gates on opaque references > operators look only at their direct operands, so nested functions coerce like other data"#,
        1,
        BOUNDARY,
    ),
    (
        "promise-test262.test.ts",
        r#"Test262 Promise statics > Promise.race([]) returns a promise whose CodeMode failure is catchable"#,
        1,
        BOUNDARY,
    ),
    (
        "promise.test.ts",
        r#"promises at data boundaries > an un-awaited promise inside a result or a tool argument is awaited"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Date > coercions: Number is the time, String is ISO, Boolean is true"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Date > template interpolation renders the ISO form"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > matchAll materializes match arrays with captures"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > cannot cross the tool boundary; the error says how to encode it"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > cannot cross the tool boundary; the error says how to encode it"#,
        3,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > constructs from a length, an array, an iterable, or another Uint8Array"#,
        3,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > constructs from a length, an array, an iterable, or another Uint8Array"#,
        4,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > constructs from a length, an array, an iterable, or another Uint8Array"#,
        5,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"built-in iterators > iterators are opaque references"#,
        4,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object values and entries accept arrays"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object values and entries accept arrays"#,
        3,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.assign preserves mutations before a circular field"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.assign preserves mutations before a circular field"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.assign rejects direct and nested cycles"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.assign rejects direct and nested cycles"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.assign rejects direct and nested cycles"#,
        3,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.assign rejects direct and nested cycles"#,
        4,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"toLocaleString > arrays join each element's toLocaleString, skipping holes and nullish elements"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"toLocaleString > numbers and dates format as en-US in UTC; everything else falls back to toString"#,
        1,
        BOUNDARY,
    ),
    (
        "descriptors.test.ts",
        r#"arrays > a frozen array refuses new values and keeps its elements"#,
        4,
        STRICT,
    ),
    (
        "parity.test.ts",
        r#"Object.freeze, seal, and preventExtensions > a frozen object rejects writes, additions, and deletes with TypeErrors"#,
        1,
        STRICT,
    ),
    (
        "parity.test.ts",
        r#"Object.freeze, seal, and preventExtensions > frozen arrays reject element, length, and mutating-method writes like JS"#,
        1,
        STRICT,
    ),
    (
        "parity.test.ts",
        r#"property deletion > a non-reference operand is evaluated and the result is true; a variable cannot be deleted"#,
        1,
        STRICT,
    ),
    (
        "parity.test.ts",
        r#"this, arguments, and Function.prototype.call/apply/bind > arguments is an unmapped array-like that arrows and parameters interact with as in JS"#,
        1,
        STRICT,
    ),
    (
        "var-hoisting-test262.test.ts",
        r#"function declarations and expressions > a named function expression sees its own name inside its body, read-only"#,
        1,
        STRICT,
    ),
    (
        "classes.test.ts",
        r#"base classes > calling a class without new throws a TypeError"#,
        1,
        MESSAGE,
    ),
    (
        "descriptors.test.ts",
        r#"Object.create and defineProperties > Object.create takes descriptors, and defineProperties reads them all before defining"#,
        1,
        MESSAGE,
    ),
    (
        "descriptors.test.ts",
        r#"Object.defineProperty > data properties default to false attributes, and read-only ones refuse writes"#,
        1,
        MESSAGE,
    ),
    (
        "descriptors.test.ts",
        r#"Object.defineProperty > the transitions ValidateAndApplyPropertyDescriptor allows and refuses"#,
        1,
        MESSAGE,
    ),
    (
        "descriptors.test.ts",
        r#"defineProperty edge cases > a refused length define leaves length alone, and a frozen array's length cannot change"#,
        1,
        MESSAGE,
    ),
    (
        "enumeration.test.ts",
        r#"Object.keys over arrays > non-object inputs follow ToObject, and nullish inputs name what was received"#,
        2,
        MESSAGE,
    ),
    (
        "enumeration.test.ts",
        r#"Object.keys over arrays > non-object inputs follow ToObject, and nullish inputs name what was received"#,
        5,
        MESSAGE,
    ),
    (
        "errors.test.ts",
        r#"error identity > allSettled reasons for the same failure are identical"#,
        1,
        MESSAGE,
    ),
    (
        "errors.test.ts",
        r#"host errors escaping built-ins > become the same-named program error"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"CodeMode-specific string behavior > an invalid normalize form is a clear catchable error"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"Error values and instanceof > errors serialize as { name, message } by brand; neither is enumerable"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"Error values and instanceof > errors serialize as { name, message } by brand; neither is enumerable"#,
        2,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: unknown static members read as undefined > calling an unknown static reports a native-style TypeError"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: unknown static members read as undefined > calling an unknown static reports a native-style TypeError"#,
        2,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: unknown static members read as undefined > calling an unknown static reports a native-style TypeError"#,
        3,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: unknown static members read as undefined > calling an unknown static reports a native-style TypeError"#,
        4,
        MESSAGE,
    ),
    (
        "callbacks.test.ts",
        r#"constructors callable without new, like JS > array length is bounded below the JS maximum, so one call cannot materialize an unbounded array"#,
        1,
        LIMIT,
    ),
    (
        "errors.test.ts",
        r#"call depth > runaway recursion fails fast with a catchable RangeError"#,
        1,
        LIMIT,
    ),
    (
        "errors.test.ts",
        r#"call depth > the limit is 10000 nested calls"#,
        1,
        LIMIT,
    ),
    (
        "errors.test.ts",
        r#"call depth > the limit is 10000 nested calls"#,
        2,
        LIMIT,
    ),
    (
        "accessors.test.ts",
        r#"internal arrays ignore indexed accessors on Object.prototype > a String.prototype index getter past the string's end is read"#,
        1,
        ENGINE,
    ),
    (
        "accessors.test.ts",
        r#"internal arrays ignore indexed accessors on Object.prototype > a getter-only accessor does not break returning an object"#,
        1,
        ENGINE,
    ),
    (
        "callbacks.test.ts",
        r#"sort accepts the unified callback set > sort and toSorted take built-in comparators"#,
        1,
        ENGINE,
    ),
    (
        "callbacks.test.ts",
        r#"sort accepts the unified callback set > sort and toSorted take built-in comparators"#,
        2,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"class inheritance > arrows capture super and this"#,
        1,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"class inheritance > super.x with ++, --, compound and logical assignment, and delete super.x"#,
        1,
        ENGINE,
    ),
    (
        "descriptors.test.ts",
        r#"reading descriptors and names > getOwnPropertyDescriptor(s) and getOwnPropertyNames"#,
        1,
        ENGINE,
    ),
    (
        "errors.test.ts",
        r#"error identity > Error.isError is true for every Error value and nothing else"#,
        1,
        ENGINE,
    ),
    (
        "generator-test262.test.ts",
        r#"confined generators > a generator's return() through for...of or destructuring surfaces a failing close, as break does"#,
        1,
        ENGINE,
    ),
    (
        "generator-test262.test.ts",
        r#"generator protocol misuse > a sync generator method borrowed by an async generator throws a catchable TypeError"#,
        1,
        ENGINE,
    ),
    (
        "json-callbacks-test262.test.ts",
        r#"CodeMode JSON callback boundaries > JSON.stringify serializes prototype-named own keys"#,
        1,
        ENGINE,
    ),
    (
        "labeled-control-test262.test.ts",
        r#"Test262 labeled continue adaptations > multiple labels can target the same iteration statement"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"CodeMode lexical scope integration > keeps classic for initializers inside the header TDZ"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"CodeMode lexical scope integration > keeps typeof and constant assignment inside the TDZ"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"CodeMode-specific string behavior > exposes the Annex B string aliases every engine ships"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"Date components convert through ToPrimitive > setters on an invalid Date answer NaN without overwriting a time set during coercion"#,
        1,
        TIMEZONE,
    ),
    (
        "parity.test.ts",
        r#"Error values and instanceof > caught failures carry the constructor name the real-JS failure would have"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"String and Number method arguments convert through ToPrimitive > the receiver converts first, then each consumed argument, in spec order; extra arguments are untouched"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"ToPrimitive: operators and conversions honor program valueOf and toString > == converts an object facing a non-nullish primitive through its own valueOf"#,
        2,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"ToPrimitive: operators and conversions honor program valueOf and toString > update and compound assignment convert the current value"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"destructuring assignment > supports defaults, nesting, rest, and member targets"#,
        1,
        ENGINE,
    ),
    (
        "parity.test.ts",
        r#"error constructor prototype chain > derived error constructors extend Error and inherit its statics"#,
        1,
        ENGINE,
    ),
    (
        "promise-test262.test.ts",
        r#"Test262 Promise.any > rejects with an AggregateError carrying the reasons in input order"#,
        1,
        ENGINE,
    ),
    (
        "regexp-math-test262.test.ts",
        r#"RegExp Test262 parity > d flag exposes match indices through exec, match, and matchAll"#,
        1,
        ENGINE,
    ),
    (
        "regexp-math-test262.test.ts",
        r#"RegExp Test262 parity > match and matchAll preserve named, unmatched, and prototype-named index groups"#,
        1,
        ENGINE,
    ),
    (
        "set-methods-test262.test.ts",
        r#"Set composition Test262 parity > accepts supported Set-like objects and uses the size-selected method"#,
        1,
        ENGINE,
    ),
    (
        "set-methods-test262.test.ts",
        r#"Set composition Test262 parity > copies receiver data before Set-like key iteration mutates it"#,
        1,
        ENGINE,
    ),
    (
        "set-methods-test262.test.ts",
        r#"Set composition Test262 parity > handles duplicate Set-like keys using receiver membership"#,
        1,
        ENGINE,
    ),
    (
        "set-methods-test262.test.ts",
        r#"Set composition Test262 parity > normalizes negative zero"#,
        1,
        ENGINE,
    ),
    (
        "set-methods-test262.test.ts",
        r#"Set relation Test262 parity > compares Maps and supported Set-like objects"#,
        1,
        ENGINE,
    ),
    (
        "set-methods-test262.test.ts",
        r#"Set relation Test262 parity > normalizes negative zero from Set-like keys"#,
        1,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > lastIndex is writable and stores a number"#,
        1,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > lastIndex is writable and stores a number"#,
        2,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > split, search, and matchAll leave lastIndex unchanged like JS"#,
        2,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > split, search, and matchAll leave lastIndex unchanged like JS"#,
        3,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > coercion, console, JSON, and Object.prototype.toString"#,
        1,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.fromEntries accepts every supported entry collection"#,
        2,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > Object.fromEntries accepts every supported entry collection"#,
        3,
        ENGINE,
    ),
    (
        "string-date-methods-test262.test.ts",
        r#"String.prototype.substr Test262 parity > test/annexB/built-ins/String/prototype/substr/length-positive.js"#,
        1,
        ENGINE,
    ),
    (
        "string-date-methods-test262.test.ts",
        r#"String.prototype.substr Test262 parity > test/annexB/built-ins/String/prototype/substr/length-undef.js"#,
        1,
        ENGINE,
    ),
    (
        "string-date-methods-test262.test.ts",
        r#"String.prototype.substr Test262 parity > test/annexB/built-ins/String/prototype/substr/start-negative.js"#,
        1,
        ENGINE,
    ),
    (
        "var-hoisting-test262.test.ts",
        r#"function declarations and expressions > generator functions have their own prototype"#,
        1,
        ENGINE,
    ),
    (
        "accessors.test.ts",
        r#"reads call the getter > a getter that throws is caught in the caller"#,
        2,
        MESSAGE,
    ),
    (
        "accessors.test.ts",
        r#"reads call the getter > a recursive getter hits the call depth limit"#,
        1,
        LIMIT,
    ),
    (
        "accessors.test.ts",
        r#"writes call the setter > an inherited setter takes the write; a getter-only accessor refuses it"#,
        2,
        MESSAGE,
    ),
    (
        "accessors.test.ts",
        r#"the iterator protocol > break calls a return getter once, and yield* delegates through getters"#,
        1,
        ENGINE,
    ),
    (
        "accessors.test.ts",
        r#"internal arrays ignore indexed accessors on Object.prototype > assigning a cycle through a data property is still rejected"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"base classes > unsupported class syntax still reports UnsupportedSyntax"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"base classes > unsupported class syntax still reports UnsupportedSyntax"#,
        2,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"base classes > unsupported class syntax still reports UnsupportedSyntax"#,
        3,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        2,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        3,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        4,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        5,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        6,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        7,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"classes extending Error > every error type can be subclassed"#,
        8,
        ENGINE,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > private fields"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > private methods"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > #x in o"#,
        1,
        MESSAGE,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > static blocks"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > new.target"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > decorators"#,
        1,
        MESSAGE,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > extends null"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > new on a plain function"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > calling a class"#,
        1,
        MESSAGE,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > extending a built-in"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > super() twice"#,
        1,
        MESSAGE,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > super() in an arrow"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > destructuring into super.x"#,
        1,
        BOUNDARY,
    ),
    (
        "classes.test.ts",
        r#"unsupported features > a static member named prototype"#,
        1,
        MESSAGE,
    ),
    (
        "for-await-test262.test.ts",
        r#"Test262 for-await-of adaptations > an awaited rejection exits through normal try/catch"#,
        3,
        BOUNDARY,
    ),
    (
        "for-await-test262.test.ts",
        r#"Test262 for-await-of adaptations > rejects malformed iterator protocol results"#,
        2,
        MESSAGE,
    ),
    (
        "for-await-test262.test.ts",
        r#"Test262 for-await-of adaptations > rejects objects without an iterator protocol method"#,
        2,
        MESSAGE,
    ),
    (
        "generator-test262.test.ts",
        r#"confined generators > rejects async generators in every synchronous iterable consumer"#,
        1,
        ENGINE,
    ),
    (
        "labeled-control-test262.test.ts",
        r#"Test262 labeled continue adaptations > a label on a non-iteration statement is not a continue target"#,
        2,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"property deletion > rejects deletion from opaque runtime references"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"CodeMode-specific array behavior > splice rejects inserting a container into itself"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"CodeMode-specific array behavior > splice rejects inserting a container into itself"#,
        2,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"compound assignment matches its binary operator > CodeMode Date += concatenates its string form, like d = d + 1"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"compound assignment matches its binary operator > compound assignment through a member target coerces the same way"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"H5: builtin coercion functions work as array callbacks > a non-callable callback is still rejected"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: String method arguments coerce like native JS > opaque runtime references still reject as data errors"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: String method arguments coerce like native JS > opaque runtime references still reject as data errors"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"coercion parity: ++ and -- use CodeMode numeric coercion > opaque runtime references reject with a clear error"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"async function line breaks > a line break between async and function is not an async function"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"String.raw > a template without a raw array is a TypeError"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"Object.freeze, seal, and preventExtensions > primitives pass through, and Object.assign honors the flags"#,
        2,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"ToPrimitive: operators and conversions honor program valueOf and toString > program-installed valueOf and toString on opaque values are ignored at every site"#,
        1,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"computed property keys convert through the object's own toString > a nullish base throws before the key converts, as ToObject precedes ToPropertyKey"#,
        1,
        MESSAGE,
    ),
    (
        "parity.test.ts",
        r#"String and Number method arguments convert through ToPrimitive > split, replace, match, and search convert a plain pattern but keep a RegExp as is"#,
        2,
        BOUNDARY,
    ),
    (
        "parity.test.ts",
        r#"String and Number method arguments convert through ToPrimitive > split, replace, match, and search convert a plain pattern but keep a RegExp as is"#,
        3,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Number and Math > Number valueOf does not enable boxed numbers"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Number and Math > Number valueOf does not enable boxed numbers"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > invalid patterns fail with actionable messages"#,
        1,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > invalid patterns fail with actionable messages"#,
        2,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > invalid patterns fail with actionable messages"#,
        3,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > invalid patterns fail with actionable messages"#,
        4,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > invalid patterns fail with actionable messages"#,
        5,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"RegExp > invalid patterns fail with actionable messages"#,
        6,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Headers > serializes as a name-to-value object at the boundary and in JSON; prints for console"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"Headers > rejects what it cannot build from, and invalid names and values, with TypeErrors the program can catch"#,
        1,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > from rejects a non-callable mapper even for an empty source"#,
        1,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > from rejects a non-callable mapper even for an empty source"#,
        2,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Uint8Array > from rejects a non-callable mapper even for an empty source"#,
        3,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Iterator helpers > argument validation"#,
        1,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Iterator helpers > argument validation"#,
        2,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Iterator helpers > argument validation"#,
        3,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Iterator helpers > argument validation"#,
        4,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Iterator helpers > argument validation"#,
        5,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Iterator helpers > argument validation"#,
        6,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"Object.prototype.toString > reports the built-in kind it is inherited by, as JS does through Symbol.toStringTag"#,
        1,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > new dispatches on the constructor value, not its name"#,
        4,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > new dispatches on the constructor value, not its name"#,
        5,
        MESSAGE,
    ),
    (
        "stdlib.test.ts",
        r#"stdlib integration > deterministic Math methods match the host runtime"#,
        1,
        ENGINE,
    ),
    (
        "stdlib.test.ts",
        r#"CodeMode values at intra-CodeMode checkpoints > Object enumeration rejects invalid receivers and gives promises an await hint"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"CodeMode values at intra-CodeMode checkpoints > Object enumeration rejects invalid receivers and gives promises an await hint"#,
        3,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"CodeMode values at intra-CodeMode checkpoints > Array.from rejects invalid receivers and gives promises an await hint"#,
        2,
        BOUNDARY,
    ),
    (
        "stdlib.test.ts",
        r#"CodeMode values at intra-CodeMode checkpoints > Array.from rejects invalid receivers and gives promises an await hint"#,
        3,
        BOUNDARY,
    ),
    (
        "string-core-test262.test.ts",
        r#"Test262-adapted core String behavior > test/built-ins/String/prototype/localeCompare/15.5.4.9_CE.js"#,
        1,
        ENGINE,
    ),
    (
        "string-date-methods-test262.test.ts",
        r#"String.prototype.substr Test262 parity > test/annexB/built-ins/String/prototype/substr/length-falsey.js"#,
        1,
        ENGINE,
    ),
    (
        "string-date-methods-test262.test.ts",
        r#"String.prototype.substr Test262 parity > test/annexB/built-ins/String/prototype/substr/length-negative.js"#,
        1,
        ENGINE,
    ),
    (
        "string-regexp-test262.test.ts",
        r#"Test262-adapted matchAll behavior > test/built-ins/String/prototype/matchAll/regexp-prototype-matchAll-v-u-flag.js"#,
        1,
        ENGINE,
    ),
];

fn is_u(v: &Value) -> bool {
    v.get("$u").is_some() && v.as_object().is_some_and(|o| o.len() == 1)
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// Jest's `toEqual` (`strict`: `toStrictEqual`) on canonical values.
fn equal(a: &Value, b: &Value, strict: bool) -> bool {
    if let Some(t) = b.get("$contains").and_then(Value::as_str) {
        return a.as_str().is_some_and(|s| s.contains(t));
    }
    if let Some(m) = b.get("$match") {
        return matches(a, m);
    }
    if let Some(t) = b.get("$any").and_then(Value::as_str) {
        return match t {
            "Number" => a.is_number() || a.get("$n").is_some(),
            "String" => a.is_string(),
            "Boolean" => a.is_boolean(),
            "Array" => a.is_array(),
            "Object" => a.is_object() || a.is_array(),
            "BigInt" => a.get("$big").is_some(),
            _ => false,
        };
    }
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => num(a) == num(b),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q, strict))
        }
        (Value::Object(x), Value::Object(y)) => {
            let keys = |o: &serde_json::Map<String, Value>| -> Vec<String> {
                let mut k: Vec<String> = o
                    .iter()
                    .filter(|(_, v)| strict || !is_u(v))
                    .map(|(k, _)| k.clone())
                    .collect();
                k.sort();
                k
            };
            let (kx, ky) = (keys(x), keys(y));
            kx == ky && kx.iter().all(|k| equal(&x[k], &y[k], strict))
        }
        _ => a == b,
    }
}

/// Jest's `toMatchObject`.
fn matches(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y))
            if b.get("$any").is_none() && b.get("$u").is_none() =>
        {
            y.iter().all(|(k, v)| match x.get(k) {
                Some(av) => matches(av, v),
                None => is_u(v),
            })
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| matches(p, q))
        }
        _ => equal(a, b, false),
    }
}

fn js_len(v: &Value) -> Option<usize> {
    match v {
        Value::Array(a) => Some(a.len()),
        Value::String(s) => Some(s.encode_utf16().count()),
        _ => None,
    }
}

fn check(matcher: &str, got: &Value, want: &Value) -> Result<bool, String> {
    Ok(match matcher {
        "toBe" => {
            if want.is_array()
                || (want.is_object()
                    && want.get("$n").is_none()
                    && !is_u(want)
                    && want.get("$big").is_none())
            {
                return Err("toBe with an object".into());
            }
            equal(got, want, true)
        }
        "toEqual" => equal(got, want, false),
        "toStrictEqual" => equal(got, want, true),
        "toMatchObject" => matches(got, want),
        "toContain" => match (got, want) {
            (Value::String(s), Value::String(w)) => s.contains(w.as_str()),
            (Value::Array(a), w) => a.iter().any(|x| equal(x, w, true)),
            _ => false,
        },
        "toHaveLength" => js_len(got).map(|n| n as f64) == num(want),
        "toBeUndefined" => is_u(got),
        "toBeNull" => got.is_null(),
        "toBeNaN" => got == &json!({"$n": "NaN"}),
        "toBeTrue" => got == &json!(true),
        "toBeDefined" => !is_u(got),
        "toStartWith" => match (got, want) {
            (Value::String(s), Value::String(w)) => s.starts_with(w.as_str()),
            _ => false,
        },
        "toEndWith" => match (got, want) {
            (Value::String(s), Value::String(w)) => s.ends_with(w.as_str()),
            _ => false,
        },
        "toBeGreaterThan" => num(got).zip(num(want)).is_some_and(|(a, b)| a > b),
        "toBeGreaterThanOrEqual" => num(got).zip(num(want)).is_some_and(|(a, b)| a >= b),
        "toBeLessThan" => num(got).zip(num(want)).is_some_and(|(a, b)| a < b),
        "toBeLessThanOrEqual" => num(got).zip(num(want)).is_some_and(|(a, b)| a <= b),
        "toBeFalse" => got == &json!(false),
        m => return Err(format!("unsupported matcher {m}")),
    })
}

async fn run(code: &str) -> Result<Value, String> {
    codemode::execute_with(None, code, &[], 50, Duration::from_secs(5), |_, _| async {
        Err::<String, String>("no tools here".into())
    })
    .await
    .result
}

/// A JavaScript property access for each key of `path`.
fn projection(path: &[Value]) -> String {
    path.iter()
        .map(|k| {
            format!(
                "[{}]",
                serde_json::to_string(k.as_str().unwrap_or_default()).unwrap()
            )
        })
        .collect()
}

/// Run one case: `Ok` when it behaves as OpenCode's test expects. A case
/// with a `path` asserts on part of the result, as its OpenCode test reads
/// it: `["value", ...]` a member of the value, `["error", "message"]` the
/// failure's message, `["logs", ...]` what the program logged.
async fn case(c: Value) -> Result<(), String> {
    let code = c["code"].as_str().unwrap();
    let matcher = c["matcher"].as_str().unwrap();
    if matcher == "fails" {
        return match run(code).await {
            Err(_) => Ok(()),
            Ok(v) => Err(format!("expected a failure, got {v}")),
        };
    }
    if matcher == "succeeds" {
        return run(code)
            .await
            .map(drop)
            .map_err(|e| format!("failed: {e}"));
    }
    let path: Vec<Value> = c["path"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![json!("value")]);
    let not = c["not"] == json!(true);
    // An expected value OpenCode's test computes by running another program.
    let computed;
    let want = match c["expected"].get("$prog").and_then(Value::as_str) {
        Some(other) => {
            let at: Vec<Value> = c["expected"]["$path"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let program = format!(
                "{CANON}\nconst __v = await (async () => {{\n{}\n}})();\nreturn __canon(__v{});",
                codemode::program(other),
                projection(&at)
            );
            computed = run(&program)
                .await
                .map_err(|e| format!("the expected program failed: {e}"))?;
            &computed
        }
        None => &c["expected"],
    };
    let got = match path[0].as_str() {
        Some("value") => {
            let program = format!(
                "{CANON}\nconst __v = await (async () => {{\n{}\n}})();\nreturn __canon(__v{});",
                codemode::program(code),
                projection(&path[1..])
            );
            run(&program).await.map_err(|e| format!("failed: {e}"))?
        }
        Some("error") => {
            let message = match run(code).await {
                Err(e) => e,
                Ok(v) => return Err(format!("expected a failure, got {v}")),
            };
            match &path[1..] {
                [m] if m == "message" => json!(message),
                _ => return Err(format!("no error field {path:?} in CodeSmith")),
            }
        }
        Some("logs") => {
            let out =
                codemode::execute_with(None, code, &[], 50, Duration::from_secs(5), |_, _| async {
                    Err::<String, String>("no tools here".into())
                })
                .await;
            let logs = json!(out.logs);
            match &path[1..] {
                [] => logs,
                [i] => logs[i
                    .as_str()
                    .unwrap_or_default()
                    .parse::<usize>()
                    .unwrap_or(usize::MAX)]
                .clone(),
                _ => return Err(format!("unsupported path {path:?}")),
            }
        }
        _ => return Err(format!("unsupported path {path:?}")),
    };
    match check(matcher, &got, want)? {
        ok if ok != not => Ok(()),
        _ => Err(format!(
            "{}{matcher}: got {got}, want {want}",
            if not { "not." } else { "" }
        )),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_codemode_language_cases() {
    use futures_util::StreamExt;
    let doc: Value =
        serde_json::from_str(include_str!("fixtures/oc_codemode.json")).expect("fixture");
    let cases = doc["cases"].as_array().unwrap().clone();
    let known: BTreeMap<(&str, &str, usize), &str> = KNOWN
        .iter()
        .map(|(f, t, n, why)| ((*f, *t, *n), *why))
        .collect();
    let parallel = std::thread::available_parallelism().map_or(4, |n| n.get());
    let outcomes: Vec<(Value, Result<(), String>)> = futures_util::stream::iter(cases)
        .map(|c| async move {
            let r = case(c.clone()).await;
            (c, r)
        })
        .buffered(parallel)
        .collect()
        .await;
    let mut unexpected = vec![];
    let mut fixed = vec![];
    let mut passed = 0;
    for (c, outcome) in &outcomes {
        let file = c["file"].as_str().unwrap();
        let test = c["test"].as_str().unwrap();
        let n = c["n"].as_u64().unwrap() as usize;
        let listed = known.get(&(file, test, n));
        match (outcome, listed) {
            (Ok(()), None) => passed += 1,
            (Ok(()), Some(why)) if *why == TIMEZONE => passed += 1,
            (Ok(()), Some(_)) => fixed.push(format!("{file} | {test} | {n}")),
            (Err(_), Some(_)) => {}
            (Err(e), None) => unexpected.push(format!("{file} | {test} | {n}: {e}")),
        }
    }
    eprintln!(
        "{passed} of {} OpenCode Code Mode cases pass ({} known failures)",
        outcomes.len(),
        known.len()
    );
    assert!(
        unexpected.is_empty() && fixed.is_empty(),
        "unexpected failures ({}):\n{}\n\nlisted in KNOWN but passing ({}):\n{}",
        unexpected.len(),
        unexpected.join("\n"),
        fixed.len(),
        fixed.join("\n")
    );
}

/// promise.test.ts: "resolve wraps plain values and passes promises
/// through", its assertion with a tool (the others are in the fixture): a
/// tool call's promise passes through `Promise.resolve`. (adapted: a
/// CodeSmith tool's result is its text, so the echo is `"3"`.)
#[tokio::test(flavor = "multi_thread")]
async fn oc_promise_resolve_passes_a_tool_call_through() {
    let echo = codemode::Entry {
        tool: "host_echo".into(),
        namespace: "host".into(),
        name: "echo".into(),
        description: "Echo an id".into(),
        schema: json!({"type": "object", "properties": {"id": {"type": "number"}}}),
        output: None,
        freeform: false,
    };
    let out = codemode::execute_with(
        None,
        "return await Promise.resolve(tools.host.echo({ id: 3 }))",
        &[echo],
        50,
        Duration::from_secs(5),
        |_, input| async move { Ok::<String, String>(input["id"].to_string()) },
    )
    .await;
    assert_eq!(out.result, Ok(json!("3")));
}
