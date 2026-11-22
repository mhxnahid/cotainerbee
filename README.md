# ⬢ containerbee

A fast Docker TUI for the terminal. One ~1.5 MB binary, about 5 MB of RAM, no daemon and no config file. It drives your own `docker` CLI, so it works with whatever context or remote host the CLI is already pointed at.

## Install

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/mhxnahid/cotainerbee/main/install.sh | sh
```

Installs to `/usr/local/bin` when writable, otherwise `~/.local/bin`. Set `CONTAINERBEE_INSTALL_DIR` to choose.

Windows and manual downloads: grab an archive from the [latest release](https://github.com/mhxnahid/cotainerbee/releases/latest) and put the binary on your `PATH`.

From source (needs Rust):

```sh
cargo install --git https://github.com/mhxnahid/cotainerbee
```

Then run `containerbee`. Press `?` anywhere for the keys that apply to the current screen.

## What you get

| | |
|---|---|
| **Compose-aware** | Containers are grouped by compose project. On a project row, `S`/`s`/`r`/`p`/`D`/`U` run compose start, stop, restart, pause, down and up -d. `l` shows the whole project's logs merged and colour-coded per service. |
| **Bulk actions** | Mark containers, images, volumes or networks with `␣` (or `a` for all), then act on everything marked at once. |
| **File browser** | `f` on a container. Running ones are browsed live, stopped or distroless ones via `docker export`. View text files, inspect binaries as hex, download (`d`), upload (`u`), or open a shell in the current directory (`e`). |
| **Image explorer** | `x` on an image lists every layer with its size, file count and the Dockerfile command behind it. Open a layer to see only what it added, changed or removed, or browse the merged filesystem. |
| **Volume browser** | `f` on a volume mounts it into a throwaway `busybox` helper so you can browse, download and upload files. The helper is removed when you leave. |
| **Log search** | Regex search with `/`, `n`, `N` and highlighting; `f` keeps only matching lines. Smart case, follow mode, timestamps, wrap and horizontal scroll. The same viewer handles inspect output and files. |
| **Live monitoring** | `m` shows CPU, memory and network charts, disk I/O, PIDs and a running process table. Lists carry CPU sparklines and update instantly from `docker events`. |
| **Odds and ends** | `o` opens a published port in the browser, `y` copies an ID (works over SSH), `E` runs a one-off command, `e` on an image opens a shell in a throwaway container. |

## Settings

Everything is optional, set as environment variables:

- `CONTAINERBEE_DOCKER` — docker binary to call (default `docker`)
- `CONTAINERBEE_HELPER_IMAGE` — image used for volume browsing (default `busybox:latest`)
- `CONTAINERBEE_DOWNLOADS` — where downloads land (default: current directory)
- `CONTAINERBEE_TAIL` — log lines loaded initially (default 1000, or 300 per container for merged logs)

## License

AGPL-3.0-only. Free to use, study and modify; if you run a modified version as a service, your users are entitled to its source. See [LICENSE](LICENSE).
