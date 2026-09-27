# Design

## Threat model

- The upload side accepts files only from the owner's machines. The token and the Tailscale-only listener both have to pass.
- The read side is public, because GitHub's camo proxy fetches images from the internet. An image URL is effectively a password for that image.
- Screenshots from private repositories end up on the read side. So uploads must not carry more than their pixels, and old images should not live forever.

## Why re-encode instead of stripping metadata

An earlier version walked PNG, JPEG and WebP chunks and dropped the metadata ones. Review found that approach kept too much:

- **JPEG:** data appended after the end marker (JPEG+ZIP polyglots) and MPF secondary images, such as phone gain maps, which carry their own EXIF.
- **PNG:** unknown private chunks.
- **WebP:** chunks past the declared RIFF size.
- **GIF:** comments and XMP application extensions.
- **ICC profiles:** kept in every format. A macOS display profile names the monitor model.

Decoding to pixels and encoding again removes all of these at once. The costs:

- JPEG is saved again at quality 90.
- A 5K screenshot takes under a second of CPU, and about 70 MB peak memory on the server.
- Animated WebP is refused, because the `image` crate cannot encode it.
- PNG, GIF and WebP output keeps the exact pixels. The tests check this.

## Limits

| Limit | Value | Reason |
|---|---|---|
| Upload size | 10 MiB | Checked from Content-Length before reading, and again while streaming, so chunked bodies are capped too |
| Image side | 16384 px | Decompression bombs |
| Pixels per frame | 40 million | Decompression bombs |
| GIF frames | 300, and 400 million pixels in total | CPU time |
| Decoder allocation | 192 MiB | The service's cgroup is capped at 256 MiB |
| Concurrent decodes | 1 | Peak memory stays at one image |
| Connections | 16 | Resource exhaustion |
| Header and body timeouts | 10 s and 30 s | Slow clients |
| Free disk kept | 2 GiB | The disk is shared with other services |
| Store size | 5 GiB | |

## HTTP API

| Request | Auth | Result |
|---|---|---|
| `GET` or `HEAD /health` | none | 200 `{"ok":true}` |
| `POST /upload?alt=&ttl=&keep=` | Bearer | 201 `{url, markdown, bytes, expires}` |
| `DELETE /<name>` | Bearer | 200 `{deleted, note}`, or 404 |
| known path, wrong method | Bearer | 405 with `Allow` |
| anything else | Bearer | 404, or 401 without a valid token |

**Error bodies** are `{"error": "..."}`:

- 400: empty body or bad `ttl`.
- 401: unauthorised. The reply is delayed 1 s and carries `WWW-Authenticate: Bearer`.
- 408: body timeout.
- 413: too large.
- 415: unsupported type or animated WebP.
- 422: undecodable, or over a limit.
- 507: the disk floor or the store cap was reached.

**Request details:**

- The token check compares SHA-256 digests in constant time, so a wrong token of any length takes the same path.
- `ttl` takes `<n>d`, `<n>h` or `<n>m`. `keep=1` means the image never expires. The default is 90 days.
- `alt` replaces `[`, `]`, `\` and control characters with spaces, collapses runs of whitespace, and keeps at most 200 characters.

**Names** are 16 random bytes in base64url, then the extension. nginx serves only `^[A-Za-z0-9_-]{22}\.(png|jpg|gif|webp)$`, and DELETE checks the raw path against the same pattern, so paths such as `../` never reach the filesystem.

## Storage

- `/srv/gh-img` is owned by `gh-img:www-data` with mode 2750, and files are 0640. nginx can read the images, and other service users cannot.
- Writes go to a dot-prefixed temp file that nginx will not serve. The file is fsynced, renamed without overwriting, and the directory is fsynced. Temp files older than an hour are removed at startup.
- Expiry times live in `/var/lib/gh-img/index.json`, outside the served directory. `gh-img-server sweep` deletes expired files daily. `gh-img-server adopt` indexes files that predate the index.
- The log records image names, sizes and client addresses, never tokens, alt text or bodies.

## Sandbox

- **systemd:** the unit uses `IPAddressAllow` for the allowed tailnet clients, `IPAddressDeny=any`, a `@system-service` syscall filter, `MemoryDenyWriteExecute`, `ProtectSystem=strict` and no capabilities. `systemd-analyze security` rates it 1.2.
- **Credential:** the token arrives through `LoadCredential`.
- **Binding:** the listener uses `IP_FREEBIND`, so it starts before the Tailscale address exists.
- **Build:** the binary is static musl and has no C dependencies.
