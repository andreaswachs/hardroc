-- Roc language server (`hardroc`) setup for Neovim 0.11+.
--
-- Install as `~/.config/nvim/after/plugin/roc.lua` (or `lua/roc.lua` plus a
-- `require("roc")` from your config). Assumes `hardroc` is on `$PATH`; see the
-- README in this directory for build instructions.
--
-- The file is idempotent: it may be sourced more than once without stacking
-- duplicate configuration. It never touches global diagnostics or keymaps.

if not (vim.lsp.config and vim.lsp.enable) then
  vim.notify(
    "hardroc: Neovim 0.11 or newer is required (vim.lsp.config/vim.lsp.enable missing)",
    vim.log.levels.ERROR
  )
  return
end

-- Map `*.roc` to the `roc` filetype. `vim.filetype.add` replaces any previous
-- entry for the extension, so repeating this is harmless.
vim.filetype.add({
  extension = { roc = "roc" },
})

-- Prefer nvim-cmp capabilities when the plugin is installed; fall back to the
-- protocol defaults otherwise. `pcall` keeps this file usable without nvim-cmp.
local has_cmp, cmp_lsp = pcall(require, "cmp_nvim_lsp")
local capabilities
if has_cmp then
  capabilities = cmp_lsp.default_capabilities()
else
  capabilities = vim.lsp.protocol.make_client_capabilities()
end

-- Server definition. `init_options` mirrors the documented `hardroc` defaults
-- from the project README (which are accepted both nested and at top level).
vim.lsp.config("hardroc", {
  cmd = { "hardroc", "--stdio" },
  filetypes = { "roc" },
  root_markers = { "main.roc", "app.roc", "package.roc", ".git" },
  capabilities = capabilities,
  init_options = {
    compilerDiagnostics = true,
    syntaxDiagnostics = true,
    checkDebounceMs = 800,
    inspectPackages = true,
    inlayHints = {
      enabled = true,
      parameterNames = true,
      parameterTypes = true,
      returnTypes = true,
      bindingTypes = true,
    },
    completion = {
      snippets = true,
      builtins = true,
      packageMembers = true,
      resolveDocumentation = true,
    },
  },
})

vim.lsp.enable("hardroc")

-- Without nvim-cmp, use Neovim's built-in completion popup. Only enable it when
-- the API exists (0.11+) and no completion plugin owns the buffer.
--
-- `vim.lsp.completion.enable` takes a numeric client id, so it cannot be called
-- for `hardroc` before a client has attached. Enable it for clients that are
-- already running (e.g. when this file is re-sourced after `LspAttach`) and for
-- every future attach below.
local function enable_builtin_completion(client_id)
  if has_cmp or not (vim.lsp.completion and vim.lsp.completion.enable) then
    return
  end
  pcall(vim.lsp.completion.enable, true, client_id, nil, { autotrigger = true })
end

if not has_cmp then
  for _, client in ipairs(vim.lsp.get_clients({ name = "hardroc" })) do
    enable_builtin_completion(client.id)
  end

  local group = vim.api.nvim_create_augroup("hardroc_completion", { clear = true })
  vim.api.nvim_create_autocmd("LspAttach", {
    group = group,
    desc = "Enable built-in LSP completion for hardroc without nvim-cmp",
    callback = function(args)
      local client = vim.lsp.get_client_by_id(args.data.client_id)
      if client and client.name == "hardroc" then
        enable_builtin_completion(client.id)
      end
    end,
  })
end

-- Neovim 0.12 can keep stale inlay-hint columns after a write and then trip
-- `Invalid 'col': out of range` in its decoration provider (upstream issue
-- neovim/neovim#39772). Drop the cached hints and refetch them after every
-- write; this is a no-op when hints are disabled.
local function refresh_inlay_hints_after_write(bufnr)
  if not (vim.lsp.inlay_hint and vim.lsp.inlay_hint.is_enabled) then
    return
  end
  if not vim.api.nvim_buf_is_valid(bufnr) then
    return
  end
  if not vim.lsp.inlay_hint.is_enabled({ bufnr = bufnr }) then
    return
  end
  vim.lsp.inlay_hint.enable(false, { bufnr = bufnr })
  vim.schedule(function()
    if vim.api.nvim_buf_is_valid(bufnr) then
      vim.lsp.inlay_hint.enable(true, { bufnr = bufnr })
    end
  end)
end

vim.api.nvim_create_autocmd("BufWritePost", {
  group = vim.api.nvim_create_augroup("hardroc_inlay_hints", { clear = true }),
  desc = "Refresh stale inlay hints after writing a Roc buffer",
  callback = function(args)
    refresh_inlay_hints_after_write(args.buf)
  end,
})

-- User commands -----------------------------------------------------------------

local function notify(message, level)
  vim.notify("hardroc: " .. message, level or vim.log.levels.INFO)
end

-- Send `workspace/executeCommand` to the client attached to `bufnr` and render
-- the JSON result with `vim.notify`.
local function execute(bufnr, command, arguments, label)
  if not vim.api.nvim_buf_is_valid(bufnr) then
    notify(label .. ": invalid buffer", vim.log.levels.ERROR)
    return
  end

  local params = { command = command }
  if arguments then
    params.arguments = arguments
  end

  local sent = vim.lsp.buf_request(
    bufnr,
    "workspace/executeCommand",
    params,
    function(err, result, ctx)
      if err then
        notify(label .. " failed: " .. tostring(err.message or err), vim.log.levels.ERROR)
        return
      end
      if result == nil then
        local client = ctx and ctx.client_id and vim.lsp.get_client_by_id(ctx.client_id)
        local reason = client and "the server returned no result" or "no hardroc client responded"
        notify(label .. ": " .. reason, vim.log.levels.WARN)
        return
      end
      notify(label .. "\n" .. vim.inspect(result))
    end
  )

  if not sent then
    notify(label .. ": hardroc is not attached to this buffer", vim.log.levels.WARN)
  end
end

-- `hardroc.deps` / `hardroc.runTest` take the current file path; the server also
-- accepts a `file://` URI. `hardroc.packageInfo` ignores arguments.
local function execute_for_buffer(command, label)
  local path = vim.api.nvim_buf_get_name(0)
  if path == "" then
    notify(label .. ": buffer has no file name; save it first", vim.log.levels.WARN)
    return
  end
  execute(0, command, { path }, label)
end

vim.api.nvim_create_user_command("RocDeps", function()
  execute_for_buffer("hardroc.deps", "roc deps")
end, { desc = "Run `roc deps` for the current Roc file via hardroc" })

vim.api.nvim_create_user_command("RocTest", function()
  execute_for_buffer("hardroc.runTest", "roc test")
end, { desc = "Run `roc test` for the current Roc file via hardroc" })

vim.api.nvim_create_user_command("RocPackageInfo", function()
  execute(0, "hardroc.packageInfo", nil, "package info")
end, { desc = "Show the current Roc package tree via hardroc" })
