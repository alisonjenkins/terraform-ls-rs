-- Headless nvim driver for the diagnostic-alignment-after-format probe.
--
-- Reads the spawn command for tfls (with or without lspmux wrapping)
-- from $TFLS_CMD, attaches it as an LSP client on `terraform`
-- filetype, opens the fixture, waits for diagnostics, runs an
-- opinionated format, waits again, and asserts every diagnostic's
-- range still points at a line whose text matches the diagnostic's
-- message subject. Exits 0 on PASS, 1 on FAIL.

local function fail(msg)
  io.stderr:write("FAIL: " .. tostring(msg) .. "\n")
  vim.cmd("cquit 1")
end

local function info(msg)
  io.stderr:write("INFO: " .. tostring(msg) .. "\n")
end

local function getenv_required(name)
  local v = vim.env[name]
  if not v or v == "" then
    fail("missing required env var " .. name)
  end
  return v
end

local function split_args(s)
  -- Whitespace-split that honours single quotes around args. Good
  -- enough for our flat argv shapes (`lspmux client --server-path
  -- /path/to/tfls`).
  local out = {}
  for tok in string.gmatch(s, "%S+") do
    out[#out + 1] = tok
  end
  return out
end

local tfls_cmd = split_args(getenv_required("TFLS_CMD"))
local fixture = getenv_required("TFLS_FIXTURE")
local format_style = vim.env.TFLS_FORMAT_STYLE or "opinionated"

info("tfls cmd: " .. table.concat(tfls_cmd, " "))
info("fixture: " .. fixture)

-- Open the fixture FIRST so vim.lsp.start picks up the buffer.
vim.cmd("edit " .. vim.fn.fnameescape(fixture))

local client_id = vim.lsp.start({
  name = "tfls",
  cmd = tfls_cmd,
  root_dir = vim.fs.dirname(fixture),
  init_options = { formatStyle = format_style },
})
if not client_id then
  fail("vim.lsp.start returned nil — could not spawn tfls client")
end
info("client id: " .. tostring(client_id))

local function wait_for(predicate, timeout_ms, label)
  local elapsed = 0
  local step = 100
  while elapsed < timeout_ms do
    if predicate() then
      return true
    end
    vim.wait(step)
    elapsed = elapsed + step
  end
  fail("timed out waiting for " .. label)
end

-- Wait for the LSP client to be running.
wait_for(function()
  return vim.lsp.client_is_stopped(client_id) == false
end, 5000, "client to start")

-- Wait for didOpen and initial diagnostics. tfls publishes within
-- ~50 ms of did_open, so 8s is plenty.
wait_for(function()
  return #vim.diagnostic.get(0) > 0
end, 8000, "initial diagnostics")

-- Give the workspace-scan publish a moment to land too — pre-format
-- count should match what the LSP probe sees end-to-end.
vim.wait(500)

local function snapshot()
  local diags = vim.diagnostic.get(0)
  local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  local out = {}
  for _, d in ipairs(diags) do
    out[#out + 1] = {
      lnum = d.lnum,
      end_lnum = d.end_lnum,
      col = d.col,
      end_col = d.end_col,
      message = d.message,
      severity = d.severity,
    }
  end
  return out, lines
end

local pre_diags, pre_lines = snapshot()
info(("pre-format: %d diagnostic(s), %d line(s)"):format(#pre_diags, #pre_lines))

-- Run the LSP format synchronously. Standard nvim flow: server
-- returns TextEdits, nvim applies them, sends didChange, server
-- re-publishes diagnostics.
local ok, err = pcall(function()
  vim.lsp.buf.format({ async = false, timeout_ms = 8000 })
end)
if not ok then
  fail("vim.lsp.buf.format failed: " .. tostring(err))
end

-- Give the server time to recompute + publish post-format diagnostics.
vim.wait(2000)

local post_diags, post_lines = snapshot()
info(("post-format: %d diagnostic(s), %d line(s)"):format(#post_diags, #post_lines))

-- The same alignment expectations the LSP probe uses.
local EXPECTED = {
  { msg = "`unused_z`", needle = "unused_z" },
  { msg = "`unused_a`", needle = "unused_a" },
  { msg = "`unused_local_z`", needle = "unused_local_z" },
  { msg = "`unused_local_a`", needle = "unused_local_a" },
}

local function check(label, diags, lines)
  local failures = {}
  for _, exp in ipairs(EXPECTED) do
    local matching = {}
    for _, d in ipairs(diags) do
      if string.find(d.message, exp.msg, 1, true) then
        matching[#matching + 1] = d
      end
    end
    if #matching == 0 then
      failures[#failures + 1] = ("[%s] no diagnostic matching %q (got: %s)"):format(
        label,
        exp.msg,
        vim.inspect(vim.tbl_map(function(d) return d.message end, diags))
      )
    end
    for _, d in ipairs(matching) do
      local line = lines[d.lnum + 1] or "<oob>"
      if not string.find(line, exp.needle, 1, true) then
        failures[#failures + 1] = ("[%s] diagnostic %q points at line %d = %q, expected to contain %q"):format(
          label,
          d.message:gsub("\n.*", ""),
          d.lnum,
          line,
          exp.needle
        )
      end
    end
  end
  return failures
end

local fails = {}
for _, f in ipairs(check("pre-format", pre_diags, pre_lines)) do
  fails[#fails + 1] = f
end
for _, f in ipairs(check("post-format", post_diags, post_lines)) do
  fails[#fails + 1] = f
end

-- Dump diagnostics for inspection.
io.stderr:write("\n=== pre-format diagnostics ===\n")
for _, d in ipairs(pre_diags) do
  io.stderr:write(("L%d c%d-%d: %s\n  line: %q\n"):format(
    d.lnum, d.col, d.end_col,
    (d.message:gsub("\n.*", "")),
    pre_lines[d.lnum + 1] or "<oob>"
  ))
end
io.stderr:write("\n=== post-format diagnostics ===\n")
for _, d in ipairs(post_diags) do
  io.stderr:write(("L%d c%d-%d: %s\n  line: %q\n"):format(
    d.lnum, d.col, d.end_col,
    (d.message:gsub("\n.*", "")),
    post_lines[d.lnum + 1] or "<oob>"
  ))
end

if #fails > 0 then
  io.stderr:write("\n=== alignment failures ===\n")
  for _, f in ipairs(fails) do
    io.stderr:write("  - " .. f .. "\n")
  end
  fail(("%d alignment failure(s)"):format(#fails))
end

io.stderr:write("\nPASS\n")
vim.cmd("cquit 0")
