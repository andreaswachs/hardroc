# Neovim

Config for the Roc language server (`hardroc`) on Neovim **0.11 or newer**. It
uses the built-in LSP client APIs (`vim.lsp.config`, `vim.lsp.enable`) and
optionally picks up [nvim-cmp](https://github.com/hrsh7th/nvim-cmp)
capabilities.

## Build and install the server

From the repository root:

```sh
cargo install --path . --locked
```

If Cargo's bin directory is not on `$PATH`, either add it or copy `hardroc`
somewhere that is (for example `/usr/local/bin`). Verify the server runs:

```sh
hardroc --version
hardroc --help
```

## Install the config

This directory is a drop-in copy of the configuration:

```sh
mkdir -p ~/.config/nvim/after/plugin
cp editors/neovim/roc.lua ~/.config/nvim/after/plugin/roc.lua
```

`after/plugin/` files are sourced after the rest of your config, which makes
this safe to add next to any existing Neovim setup. The file only registers the
`roc` filetype, the `hardroc` server, and the `:RocDeps`, `:RocTest`, and
`:RocPackageInfo` commands. It does not override global diagnostics, keymaps, or
other LSP settings, and it is safe to source repeatedly.

Alternatively, with `lazy.nvim`:

```lua
{
  dir = vim.fn.expand("~/Source/hardroc/editors/neovim"),
  name = "hardroc",
  lazy = false,
  config = function()
    dofile(vim.fn.expand("~/Source/hardroc/editors/neovim/roc.lua"))
  end,
}
```

Adjust `dir` to your checkout. You can also keep the file in
`~/.config/nvim/lua/roc.lua` and add `require("roc")` to your config.

The server's options are passed as LSP `init_options`, matching the defaults
documented in the project README:

```lua
compilerDiagnostics = true,
syntaxDiagnostics = true,
checkDebounceMs = 800,
inspectPackages = true,
inlayHints = { enabled = true, parameterNames = true, parameterTypes = true,
               returnTypes = true, bindingTypes = true },
completion = { snippets = true, builtins = true, packageMembers = true,
               resolveDocumentation = true },
```

## Verify the setup

Open any `.roc` file and check:

```vim
:set ft?          " => filetype=roc
:checkhealth vim.lsp
:lua =vim.lsp.get_clients({ bufnr = 0 })
```

`hardroc` should be listed as attached with `hardroc` running `--stdio`. If it is
not, open the LSP log with `:lua vim.cmd("edit " .. vim.lsp.get_log_path())`
and confirm `hardroc` is on `$PATH` inside Neovim:

```vim
:echo executable("hardroc")
```

A quick headless check of the filetype mapping:

```sh
nvim --headless -u NONE -c 'filetype on' \
  -c 'lua dofile("editors/neovim/roc.lua")' \
  -c 'lua print(vim.filetype.match({ filename = "main.roc" }))' \
  -c qa
```

The final line should print `roc`.

## Built-in LSP keymaps

Neovim 0.11's default LSP mappings are active for Roc buffers:

| Key | Action |
| --- | --- |
| `K` | Hover |
| `gri` | Go to implementation |
| `grt` | Go to type definition |
| `grn` | Rename symbol |
| `gra` | Code action |
| `grr` | References |
| `gO` | Document symbols |
| `<C-s>` (insert) | Signature help |
| `[d` / `]d` | Previous / next diagnostic |
| `[D` / `]D` | First / last diagnostic |

Formatting is handled by the server, so `vim.lsp.buf.format()` (or your own
`:Format` command / format-on-save setup) works without extra configuration.
Inlay hints are shown when enabled with `vim.lsp.inlay_hint.enable(true)`.

## Commands

| Command | Server command | Purpose |
| --- | --- | --- |
| `:RocDeps` | `hardroc.deps` | Run `roc deps` for the current file and show stdout/stderr |
| `:RocTest` | `hardroc.runTest` | Run `roc test` for the current file and show the result |
| `:RocPackageInfo` | `hardroc.packageInfo` | Show the current package/workspace tree and dependencies |

Results are displayed with `vim.notify`. `:RocDeps` and `:RocTest` require the
buffer to be saved and attached to `hardroc`; `:RocPackageInfo` needs nothing
more than an open Roc file.

## Inlay-hint crash after writing

Neovim 0.12 can retain stale inlay-hint columns after a write and then report
`inlay_hint.lua: Invalid 'col': out of range` from its decoration provider
(upstream [neovim/neovim#39772](https://github.com/neovim/neovim/issues/39772)).
`roc.lua` clears and refetches cached hints after `BufWritePost` as a
workaround; this only runs when inlay hints are enabled. Upgrade Neovim when the
upstream fix is available.
