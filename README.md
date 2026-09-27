# gh-img

Upload screenshots to your own server and get Markdown to paste into GitHub issues and pull requests.

GitHub accepts image attachments only through its web UI, so the `gh` CLI and coding agents cannot attach a screenshot to a PR. `gh-img` uploads the image to a server you run and prints the Markdown link. You need a Linux server with a public host name and [Tailscale](https://tailscale.com) on both the server and your machine.

```console
$ gh-img --alt "Checkout page after the fix" shot.png
![Checkout page after the fix](https://img.example.com/mHXXyS0nhdGXVR2KPQ4WQQ.png)

$ gh pr comment 42 --body "$(gh-img --alt "Before" before.png; gh-img --alt "After" after.png)"
```

Design notes, limits and the HTTP API are in [DESIGN.md](DESIGN.md).

## Usage

```text
gh-img [--alt TEXT] [--ttl 30d | --keep] FILE...   upload, print ![alt](url)
gh-img --url FILE...                               upload, print bare URLs
gh-img rm URL...                                   delete, and purge Cloudflare's cache
```

- Each file prints one line on stdout. Errors go to stderr and the exit status is 1, so scripts and agents can use the output directly.
- `--alt` sets the alt text. Without it the alt text is `screenshot`.
- Images expire after 90 days. `--ttl 7d` sets another expiry (`d`, `h` or `m`), and `--keep` keeps the image until you delete it.
- PNG, JPEG, GIF and WebP up to 10 MB are accepted. SVG, video and animated WebP are refused.

## What it does not protect

- **Anyone with an image URL can view the image**, including images posted in private repositories. GitHub loads images through its [camo proxy](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/about-anonymized-urls) from the public internet, so the read side cannot require a login. URLs contain 128 random bits and are never listed, so nobody can guess them, but anyone who sees the issue or PR can copy them.
- **You must check screenshots before uploading.** Crop or redact tokens, passwords, personal data and anything else that should stay inside the repository.
- **Deleting is not instant everywhere.** `gh-img rm` deletes the file and, when a Cloudflare token is configured, purges Cloudflare's copy. GitHub's camo proxy keeps its own cached copy. GitHub documents `curl -X PURGE <camo URL>` to clear it.
- **Metadata is removed, but the pixels stay.** The server decodes and re-encodes every image, which drops EXIF, GPS, ICC profiles, comments and any appended data. Whatever the screenshot shows is still there.

## Requirements

- **Server:** Linux with systemd, nginx and Tailscale, plus a TLS certificate for the public host name. A Cloudflare-proxied DNS record and a Cloudflare origin certificate work.
- **Build machine:** Rust, [zig](https://ziglang.org) and `cargo install --locked cargo-zigbuild`. The build produces a static `x86_64-unknown-linux-musl` binary, so the server needs no Rust toolchain.
- **Client:** bash, curl and jq.

## Server setup

```sh
git clone https://github.com/jamiehdev/gh-img && cd gh-img
cp deploy/host.env.example deploy/host.env   # then edit it
GH_IMG_SSH=<ssh host> scripts/deploy.sh
```

`deploy/host.env` holds the server's Tailscale address, the tailnet addresses allowed to upload, the public host name and the certificate paths. It is git-ignored.

`scripts/deploy.sh` runs the tests, builds the binary, copies it to the server and runs `deploy/install.sh` there. The install script:

- creates a `gh-img` system user, `/srv/gh-img` and an upload token in `/etc/gh-img/token`;
- installs a sandboxed systemd service and a daily timer that deletes expired images;
- adds the nginx site.

It is safe to run again. `scripts/rotate-token.sh` replaces the token and copies the new one to your machine.

## Client setup

```sh
ln -s "$PWD/bin/gh-img" ~/.local/bin/gh-img
mkdir -p ~/.config/gh-img && chmod 700 ~/.config/gh-img
ssh <ssh host> 'sudo cat /etc/gh-img/token' > ~/.config/gh-img/token
chmod 600 ~/.config/gh-img/token
echo 'GH_IMG_ENDPOINT=http://<server tailscale address>:8787' > ~/.config/gh-img/config
```

To purge Cloudflare's cache on delete, add `GH_IMG_CF_ZONE=<zone id>` to the config and put a Cloudflare API token with Cache Purge permission for that zone in `~/.config/gh-img/cloudflare-token` (mode 600).

## How it works

- `gh-img-server` is a small Rust service. It listens only on the server's Tailscale address and requires the bearer token. systemd drops connections from tailnet addresses outside the allowlist.
- It re-encodes each upload, stores it under a random name and records its expiry.
- nginx serves the stored images as static files and answers 404 for every other path, so no application code is reachable from the internet.

## Development

```sh
cargo test                   # unit, property and HTTP integration tests
scripts/check-fixtures.sh    # exiftool and Pillow check the re-encoded fixtures
scripts/make-fixtures.sh     # regenerate tests/fixtures (needs uv and exiftool)
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
