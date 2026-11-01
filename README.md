# readme

A small Rust README viewer for the terminal, with an interactive outline, folding, and export formats.

## Install

```sh
cargo install readme-viewer-tui
```

Run `readme` in a directory containing `README.md` (case insensitive). When several names match, choose one interactively. An explicit path also works: `readme docs/guide.md`.

## Usage

```sh
readme                 # interactive viewer
readme --static        # rendered plain text, suitable for pipes
readme --raw           # original Markdown
readme --json          # structured JSON
readme --yaml          # structured YAML
readme --toml          # structured TOML
readme --html > readme.html       # standalone styled HTML
readme --html --raw > source.html # escaped Markdown in a preformatted HTML page
```

JSON, YAML and TOML contain `source`, `markdown`, and `headings` (each with `level` and `text`). `--raw` bypasses extensions; with a structured format its Markdown field contains the unmodified source.

In the viewer, use arrow keys or `j`/`k` to scroll, `o` for the outline, Enter to jump from the outline, and Enter or Space on a heading at the top of the viewport to fold or unfold it. `q` closes the outline or exits. Page Up/Down scroll by ten lines. Output is simplified terminal text; HTML provides richer Markdown rendering.

## Appearance and extensions

Create `~/.config/readme/config.toml` (or the platform's configuration directory):

```toml
heading_color = "cyan"
text_color = "white"
# html_css = "body { font: 18px sans-serif; max-width: 60em; margin: auto; }"
extensions = ["my-filter"]
```

Colors: black, red, green, yellow, blue, magenta, cyan, white. `html_css` replaces the default HTML stylesheet.

Install a trusted local executable with `readme install ./my-filter`, then enable its filename in `extensions`. Filters run in the listed order, receive UTF-8 Markdown on standard input and must write UTF-8 Markdown to standard output. They run with your user permissions; only install code you trust. List installed filters with `readme extensions` and remove one with `readme uninstall my-filter`. Disable a filter by removing it from the config. Extensions run for rendering and structured exports, except with `--raw`.

## Development

```sh
cargo test
cargo run -- --static
```

MIT licensed.
