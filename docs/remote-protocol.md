# The remote protocol

> **In development**, like [remote access](remote.md) itself. This page
> describes the parts built so far: the transport, pairing and the error
> shape. Sessions, events and control are added as they're built. The
> JSON Schema for every body is
> [remote-protocol.schema.json](remote-protocol.schema.json).

This is the wire protocol between a CodeSmith machine's gateway
(`codesmith __remoted`) and a client such as a phone app. It's public:
anyone may write a client.

- [Transport](#transport)
- [Requests](#requests)
- [Errors](#errors)
- [Pairing](#pairing)
- [The check code](#the-check-code)
- [Paired connections](#paired-connections)
- [Versioning](#versioning)

## Transport

The machine and each device have an ed25519 key. A key's id (its **node
id**) is the public key as 64 lower-case hex characters. The client dials
the machine's node id over [iroh](https://iroh.computer) 1.5: QUIC with
TLS 1.3, authenticated by both keys, direct or through an encrypted relay.
The machine opens no listening TCP port.

The machine doesn't publish its addresses anywhere (no DNS or pkarr
lookup): a client keeps the relay URL and direct addresses from the pairing
link, and dials the node id it pinned from the link, never another.

| ALPN | For |
|---|---|
| `codesmith/pair/1` | Claiming a pairing code. |
| `codesmith/remote/1` | Everything a paired device does. |

The machine closes a connection with these application error codes:

| Code | When |
|---|---|
| 0 | The gateway is stopping, too many connections are open, or a pairing connection is done. |
| 1 | On `codesmith/remote/1`: the device key isn't paired, or was just revoked. |
| 2 | A connection whose ALPN the gateway doesn't serve. (In practice the TLS handshake already fails for an ALPN other than the two above, with `no_application_protocol`, so no connection is made.) |
| 3 | On `codesmith/pair/1`: no pairing code is live (no pairing dialog is open). |

## Requests

Each bidirectional stream carries one HTTP/1.1 request and its response,
then both sides finish the stream. The connection is the authentication:

- there's no `Authorization` header;
- `Host` is ignored;
- a request with an `Origin` header is refused (400 `bad_request`);
- a body needs a `Content-Length` (no transfer encodings) and may be at
  most 64 KiB (413 `too_large`); the head may be at most 16 KiB;
- a request must arrive within 10 seconds of the stream opening (30 on
  `codesmith/remote/1`).

Responses are `application/json` with a `Content-Length`.

## Errors

Every error body is:

```json
{"error": {"code": "code_expired", "message": "that pairing code is wrong, used or expired; scan the QR code again"}}
```

| Code | Status | |
|---|---|---|
| `bad_request` | 400 | A malformed request or body. |
| `not_found` | 404 | No such route (or, later, a session the device has no grant for). |
| `pairing_refused` | 403 | The terminal refused the pairing, or didn't answer in time. |
| `code_expired` | 410 | The pairing code is wrong, already used, or expired. |
| `too_large` | 413 | The request is too large. |
| `rate_limited` | 429 | Too many bad claims from this device key. |
| `internal` | 500 | The machine couldn't save the pairing. |

`message` is for people; clients act on `code`. New codes may be added: a
client treats an unknown code by its status.

## Pairing

### The link

The QR code holds a link:

```
codesmith://pair?v=1&n=<node id>&r=<relay URL>&a=<addresses>&m=<machine>&s=<session>&c=<code>
```

| Key | | |
|---|---|---|
| `v` | required | `1`. |
| `n` | required | The machine's node id. |
| `r` | optional | Its home relay URL. |
| `a` | optional | Direct addresses, comma separated (`ip:port`, IPv6 in brackets). |
| `m` | required | The machine's name. |
| `s` | optional | The session the pairing offers. |
| `c` | required | The pairing code. |

Every value is percent-encoded, leaving only RFC 3986's unreserved
characters (`A-Z a-z 0-9 - . _ ~`) as they are; `+` is a plus, not a space.
CodeSmith's links pass the app team's published `pairing.json` vectors.

The **pairing code** is 16 bytes from the OS RNG as base64url without
padding (22 characters). It's valid for 180 seconds and for one claim, and
only one is live per machine. While the pairing dialog is open, a new code
replaces each one that runs out.

### The claim

On `codesmith/pair/1`:

```
POST /v1/pair/claim
{"code": "pn00cOpwBxfLmPR62H1EaQ",
 "device": {"name": "Ray's iPhone", "platform": "ios"},
 "push": {"kind": "apns", "token": "…"},
 "unlock_key": {"alg": "ES256", "spki": "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE…"}}
```

- The **device id** is the node id the connection authenticated, never
  anything in the body.
- `device.name` loses control and text-direction characters and is cut to
  64 characters; `device.platform` the same, cut to 32. Neither may be
  empty after that.
- `push` (optional): `kind` is 1 to 32 of `A-Z a-z 0-9 - _`; `token` is
  opaque, 1 to 4096 bytes. It's stored for wake-ups, which come later.
- `unlock_key` (optional): `alg` is `ES256`; `spki` is the DER
  SubjectPublicKeyInfo of a P-256 key, standard base64 with padding. It's
  stored with the device for unlocking terminals later, and replaced only
  by pairing again.

The machine answers:

| Status | When |
|---|---|
| 200 | The terminal allowed it. |
| 400 `bad_request` | The body is malformed. This doesn't use up the code. |
| 403 `pairing_refused` | The terminal refused, or nobody answered within 90 seconds. |
| 410 `code_expired` | The code is wrong, used or expired. |
| 429 `rate_limited` | This device key made more than 5 bad claims in 60 seconds, and is locked out for 5 minutes. |

A claim with the right code uses it up at once, whatever the answer. The
request is held while the terminal asks, for at most 90 seconds. Claims
from a locked-out key are refused before the code is looked at, so they
never use it up; the lockout is per device key, so another phone can still
claim.

A 200's body:

```json
{"machine": {"id": "<node id>", "name": "Rays-Mac-mini"},
 "device": {"id": "<the device's node id>", "name": "Ray's iPhone", "platform": "ios"},
 "grants": [{"session": "ses_1a21547aca5e297de", "scope": "control"}]}
```

`grants` lists every grant the device now has. A grant's `session` is a
session id, or `*` for every session on the machine, now and future;
`scope` is `control` or `watch`. A device paired with no session has an
empty list. A device that was already paired keeps its grants and gets the
new one (replacing any it had on that session).

## The check code

Both screens show a check code, so the person can see that the phone
asking is the one in their hand. It's computed over exactly these bytes
(UTF-8, `\n` between fields, no trailing newline):

```
codesmith-pair-check/1
<code>
<machine node id, lower-case hex>
<device node id, lower-case hex>
```

Take SHA-256 of them, read the first 8 bytes as a big-endian unsigned
integer, take it mod 10^12, zero-pad it to 12 digits and show it in groups
of three: `779 852 318 371`.

For example, code `pn00cOpwBxfLmPR62H1EaQ`, machine
`aa980150be84faa3788d7d170f02984f084bcaeb6871fe1fe7805ff2505a375c` and
device `f8820d751e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6`
give `779 852 318 371`.

## Paired connections

On `codesmith/remote/1`, only a paired, unrevoked device key is accepted;
any other connection closes with code 1. Revoking a device closes its open
connections with code 1 at once.

The routes come with the next part: until then, every request answers 404
`not_found`. They'll be `GET /v1/machine`, `/v1/sessions`,
`/v1/sessions/{id}` and its messages, and the event streams, each checking
the device's grants and answering 404 for a session it has none for.

## Versioning

The ALPNs and the link's `v` carry the version. A change a client must
understand gets a new one; new fields, routes, event types and error codes
don't, and clients ignore what they don't know.
