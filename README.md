# hardroc

A language server for [Roc](https://www.roc-lang.org/), built for a world-class
editing experience in [Helix](https://helix-editor.com/) and any other LSP
client. It ships its own lossless Roc parser and semantic index, uses the
official compiler for authoritative type errors and formatting, and understands
Roc's package graph (URL bundles, path packages, and exposed type modules).

```
┌──────────────┐  JSON-RPC/LSP   ┌──────────────────────────────────────────┐
│ Helix, Neovim│ ──────────────► │ hardroc                                  │
│ VS Code, ... │ ◄────────────── │  parser → scopes → module/package graph  │
└──────────────┘                 │  features ← builtin API + compiler checks│
                                 └──────────────────────────────────────────┘
```

## Features

| Capability | What you get |
| --- | --- |
| Completion | Scope-aware locals and top-level names, imports and `exposing`, builtin types/modules/tags (`Bool.True`, `Try.Ok`), module and package members after `.`, record fields, tags in types/patterns, snippets for headers, types, matches, loops, expectations. Documentation and signatures on every item. |
| Hover | Types, signatures, doc comments, builtin API docs, record fields, operators, and package metadata (source, version, exposed modules, cache status) on imports and dependency declarations. |
| Signature help | Function and method calls, including nested calls and `|>`; active parameter tracking. |
| Go to definition / declaration / type definition / implementation | Locals, top-level items, associated items, imports, `exposing`, package modules, and files imported at compile time. |
| References / rename / highlights | Workspace-wide, cross-module, scope-correct; rename refuses builtins, package-cache files, keywords, and invalid Roc names. |
| Diagnostics | Immediate parser/analysis errors plus background `roc check` type errors and warnings, with correct UTF-16 ranges. Compiler checks run against an overlay of open buffers, so unsaved edits get live type feedback. |
| Symbols | Nested document symbols (types → methods/nested types) and workspace symbols across the project and its packages. |
| Code actions | Import/qualify unknown names, add missing imports/exposing entries, organize imports, add type annotations, apply analysis fixes. |
| Formatting | `roc fmt` through `textDocument/formatting`; range formatting is restricted to the requested lines. If formatting changes the line count, a partial range returns no edit rather than risk moving unselected code; a whole-document range gets one exact replacement. |
| Folding / selection ranges | Headers, doc comments, imports, type bodies, records, lists, lambdas, `if`/`match`, blocks. |
| Semantic tokens | Full semantic highlighting for clients that support it (Helix uses tree-sitter instead). |
| Inlay hints | Parameter names, parameter types, annotation-backed binding/return types. |
| Document links | Import targets, file imports, and package bundle URLs. |
| Code lenses | Reference counts and test entry points. |
| Package inspection | Dependency-tree command, package hover, public package-module completion (including public subdirectory modules and nested type imports such as `json.Parser.ParseErr`), go-to into `~/.cache/roc/packages/<hash>`, and workspace symbols for package exports. |

Roc language coverage follows the current compiler syntax: qualified tags in
expressions and patterns, interpolations in single-line and multiline strings,
annotated and uninitialized `var` declarations, record-builder suffixes
(`{ … }.Builder`), nested module/type imports, and compiler-owned
`platform glue` dependencies. Package imports resolve only through a package's
declared public module names; private source modules stay private unless they
are opened directly.

## Requirements

- Rust 1.75+ to build.
- Roc nightly. The builtin API database is generated from
  `roc docs --builtins`; regenerate it when the compiler changes:
  ```sh
  roc docs --builtins --output=/tmp/roc-builtins/out
  python3 tools/extract_builtins.py --input /tmp/roc-builtins/out --output data/builtins.json
  ```
- `roc` on `PATH` for compiler diagnostics and formatting. Everything else
  works without it.

## Build and install

```sh
cargo install --path . --locked
hardroc --version
```

The server speaks LSP over stdin/stdout (`hardroc --stdio`, which is also the
default). It has no network access: package sources are read from Roc's own
cache, populated by `roc deps`/`roc check`.

## Helix setup

Helix ships a `roc` language entry that runs `roc experimental-lsp`. Add the
following to `~/.config/helix/languages.toml` to use `hardroc` instead. (A copy
lives at [`editors/helix/languages.toml`](editors/helix/languages.toml).)

No Helix-side sync setting is needed: Helix sends `textDocument/didChange`
incrementally as you type. `hardroc` publishes parser/analysis diagnostics from
the in-memory buffer immediately and runs compiler checks against a shadow of
the open buffers, so type errors do not wait for `:write`. Opening a document
triggers a compiler check immediately; `checkDebounceMs` controls how long
typing must pause before subsequent compiler checks start.

Helix attaches language servers to named buffers. A `[scratch]` buffer with no
path does not start `hardroc`, even after `:set-language roc`; save it once
(`:w path/to/file.roc`). A named file, including one whose path does not exist
on disk yet, is sent to `hardroc` as soon as it is opened.

```toml
[language-server.hardroc]
command = "hardroc"
args = ["--stdio"]

# Optional: Helix passes this table as initializationOptions.
[language-server.hardroc.config]
compilerDiagnostics = true
syntaxDiagnostics = true
checkDebounceMs = 800
inspectPackages = true

[[language]]
name = "roc"
scope = "source.roc"
injection-regex = "roc"
file-types = ["roc"]
shebangs = ["roc"]
roots = ["main.roc", "app.roc", "package.roc", ".git"]
comment-token = "#"
indent = { tab-width = 4, unit = "\t" }
language-servers = ["hardroc"]
auto-format = true
```

Restart Helix and check the wiring with:

```sh
hx --health roc
```

You should see `hardroc` as the configured language server. Useful Helix commands
for the features above:

| Helix command | Feature |
| --- | --- |
| `Space` `k` | Hover |
| `C-x` (insert mode) | Completion (typing `.`/`:` also triggers) |
| `Space` `g` `d` | Go to definition |
| `Space` `g` `y` | Go to type definition |
| `Space` `g` `i` | Go to implementation |
| `Space` `g` `r` | References |
| `Space` `g` `D` | Go to declaration |
| `Space` `r` | Rename symbol |
| `Space` `a` | Code action |
| `Space` `s` | Document symbols |
| `Space` `S` | Workspace symbols |
| `:format` | Format document or selection |
| `Space` `d` | Diagnostics picker |
| `:lsp-workspace-command` | Run `hardroc.deps`, `hardroc.runTest`, `hardroc.packageInfo`, `hardroc.version` |

Signature help appears automatically while typing arguments when
`editor.lsp.auto-signature-help` is enabled; `Space` `k` on a call also shows
the signature. Inlay hints are controlled by `:toggle lsp-inlay-hints` (or the
`editor.lsp.inlay-hints` setting).

Completion inside imports: after `import ` press `C-x` to open the list because
a space is not an LSP trigger character. Once you type the first module
character — or the `.` in `import cli.` — completion opens automatically.
Typing `import util.Util exposing ` then `C-x` lists the module’s exports.

## Neovim setup

Neovim 0.11+ is required, because the client uses the built-in `vim.lsp.config`
API (`vim.lsp.enable` plus `vim.filetype.add` — no `nvim-lspconfig` plugin).

Build the server, then copy the drop-in config into your Neovim config. Full
details live in [`editors/neovim/README.md`](editors/neovim/README.md).

```sh
cargo install --path . --locked
mkdir -p ~/.config/nvim/after/plugin
cp editors/neovim/roc.lua ~/.config/nvim/after/plugin/roc.lua
```

The file registers the `roc` filetype for `*.roc`, configures `hardroc`
(`hardroc --stdio`), enables the server, and adds the `:RocDeps`, `:RocTest` and
`:RocPackageInfo` user commands.

With lazy.nvim, load the same file instead of copying it:

```lua
{
  "hardroc",
  dir = vim.fn.expand("~/path/to/hardroc/editors/neovim"),
  lazy = false,
  config = function()
    dofile(vim.fn.expand("~/path/to/hardroc/editors/neovim/roc.lua"))
  end,
}
```

Restart Neovim, open a `.roc` file, and verify the wiring:

```vim
:set ft?                  " roc
:checkhealth vim.lsp      " hardroc: attached, executable hardroc
:lua =vim.lsp.get_clients({ bufnr = 0 })
:lua =vim.lsp.config.hardroc
```

With the cursor in a Roc buffer, the built-in LSP mappings are available:

| Key | Feature |
| --- | --- |
| `K` | Hover |
| `grn` | Rename symbol |
| `gra` | Code action |
| `grr` | References |
| `gri` | Go to implementation |
| `grt` | Go to type definition |

Typing `.` or `:` triggers completion, and signature help appears while typing
arguments. Server commands run through `:RocDeps` (dependencies of the current
package), `:RocTest` (run the current test) and `:RocPackageInfo`; results are
shown with `vim.notify`. Run `:lua vim.lsp.buf.format()` to format the buffer
with `roc fmt`.

The config also works around Neovim's stale inlay-hint crash after writes
(`Invalid 'col': out of range`, neovim/neovim#39772) by clearing and refetching
hints in `BufWritePost`; see the editor README for details.

## Configuration

All settings are optional; defaults are shown below. Settings can be passed as
`initializationOptions` under `hardroc` or directly, or sent through
`workspace/didChangeConfiguration`.

```json
{
  "hardroc": {
    "rocPath": "roc",
    "compilerDiagnostics": true,
    "syntaxDiagnostics": true,
    "checkDebounceMs": 800,
    "checkTimeoutSecs": 90,
    "formatTimeoutSecs": 15,
    "inspectPackages": true,
    "maxWorkspaceFiles": 20000,
    "includePaths": [],
    "excludePaths": ["target", ".git", "node_modules"],
    "inlayHints": {
      "enabled": true,
      "parameterNames": true,
      "parameterTypes": true,
      "returnTypes": true,
      "bindingTypes": true
    },
    "completion": {
      "snippets": true,
      "builtins": true,
      "packageMembers": true,
      "resolveDocumentation": true
    }
  }
}
```

`HARDROC_ROC` overrides the `roc` executable path without config changes.

## Package inspection

Roc packages resolve to:

- URL bundles: `$HOME/.cache/roc/packages/<base58-blake3-hash>/` (or
  `$ROC_CACHE_DIR/packages/...`), with the compiler's
  `<hash>.deps` sidecar (format 5, line-oriented) giving the transitive alias
  map. Legacy `.deps.json` sidecars are also read.
- Path dependencies may point at a directory or directly at a nonstandard root
  file such as `./lib/Lib.roc`; the exact root file is retained for navigation.
- path dependencies: resolved relative to the declaring module, including
  workspace-root-relative imports (`import /Internal/Parser`).
- compiler-owned platforms: `platform glue` dependencies are recognized as
  known aliases but intentionally have no on-disk module tree to index.

The server reads these sources directly, so completion, hover, go-to, and
workspace symbols work inside dependencies. A dependency that has not been
fetched is reported in hover with the exact cache path; run `roc deps` to fetch
it.

## Architecture

```
src/
  syntax/      lossless lexer, forgiving parser, AST, tokens, doc comments
  analysis/    scopes, symbols, references, module/package graph, builtins
  features/    completion, hover, navigation, structure, actions, formatting
  server.rs    LSP lifecycle, document sync, diagnostics, dispatch
  compiler.rs  background `roc check` output parser
  format.rs    `roc fmt --stdin`
  text.rs      UTF-16 position mapping and incremental edits
data/builtins.json   generated builtin API (signatures, docs, type defs)
tools/extract_builtins.py
tests/         parser corpus, analysis, feature, and stdio LSP tests
```

Design rules:

- Byte offsets internally, UTF-16 at the protocol boundary.
- The parser never panics; malformed code yields a partial tree plus diagnostics.
- Analysis is pure per file; the workspace index resolves imports, aliases,
  packages, and builtins. Definition keys (`Local`, `TopLevel`, `Builtin`,
  `Module`) drive references and rename across files.
- Compiler checks run debounced in the background and never block typing.
- The builtin API is embedded, so hover/completion work offline.

## Development

```sh
cargo test
cargo run -- --stdio          # speak LSP manually if you like
```

Tests cover the parser, UTF-16 edits, scopes/references, package resolution,
every feature, and an end-to-end stdio LSP harness. Set `ROC_EXAMPLES_DIR` to a
checkout of Roc's examples to run the parser corpus over real code.

## License

MIT.
