#!/usr/bin/env bash
# E2E target: Lua command-line todo app with tests

TARGET_TIMEOUT=900

scaffold() {
    mkdir -p "${TARGET}"

    # Check Lua is available
    if ! command -v lua &>/dev/null; then
        err "lua is not installed"
        exit 1
    fi

    LUA_VERSION="$(lua -v 2>&1 | head -1)"
    log "Found: ${LUA_VERSION}"

    cat > "${TARGET}/README.md" <<'README'
# Todo App

A command-line todo application in Lua.

## Requirements

- Add a todo item with a title
- List all todo items (with optional filter: all, active, done)
- Mark a todo item as done by ID
- Delete a todo item by ID
- Persist todos to a file (todos.json) using a simple JSON format
- Include tests in test_todo.lua that verify all operations
- Pure Lua only - no external dependencies or package managers

## Pre-provided

- `json.lua` - JSON encoder/decoder (use `local json = require("json")`)
- `test.lua` - minimal test runner (use `local test = require("test")`)
README

    # Pre-provide json.lua so agents don't have to write their own JSON encoder/decoder
    cat > "${TARGET}/json.lua" <<'JSONLUA'
-- json.lua: minimal JSON encoder/decoder for todo data
-- Handles: arrays, objects, strings, numbers, booleans, null

local json = {}

local function is_array(t)
  local i = 0
  for _ in pairs(t) do
    i = i + 1
    if t[i] == nil then return false end
  end
  return true
end

local function encode_value(val)
  local t = type(val)
  if val == nil then
    return "null"
  elseif t == "boolean" then
    return tostring(val)
  elseif t == "number" then
    return tostring(val)
  elseif t == "string" then
    return '"' .. val:gsub('\\', '\\\\'):gsub('"', '\\"'):gsub('\n', '\\n'):gsub('\r', '\\r'):gsub('\t', '\\t') .. '"'
  elseif t == "table" then
    if is_array(val) then
      local items = {}
      for _, v in ipairs(val) do
        items[#items + 1] = encode_value(v)
      end
      return "[" .. table.concat(items, ",") .. "]"
    else
      local parts = {}
      for k, v in pairs(val) do
        parts[#parts + 1] = '"' .. tostring(k) .. '":' .. encode_value(v)
      end
      return "{" .. table.concat(parts, ",") .. "}"
    end
  end
  error("cannot encode type: " .. t)
end

function json.encode(val)
  return encode_value(val)
end

local function skip_ws(s, i)
  while i <= #s and s:sub(i, i):match('%s') do i = i + 1 end
  return i
end

local function decode_value(s, i)
  i = skip_ws(s, i)
  local c = s:sub(i, i)
  if c == '"' then
    local j = i + 1
    local chars = {}
    while j <= #s do
      local ch = s:sub(j, j)
      if ch == '"' then
        return table.concat(chars), j + 1
      elseif ch == '\\' then
        local esc = s:sub(j + 1, j + 1)
        local map = { n='\n', r='\r', t='\t', ['"']='"', ['\\']='\\'  }
        chars[#chars + 1] = map[esc] or esc
        j = j + 2
      else
        chars[#chars + 1] = ch
        j = j + 1
      end
    end
    error("unterminated string")
  elseif c == '[' then
    local arr = {}
    i = skip_ws(s, i + 1)
    if s:sub(i, i) == ']' then return arr, i + 1 end
    while true do
      local val
      val, i = decode_value(s, i)
      arr[#arr + 1] = val
      i = skip_ws(s, i)
      local sep = s:sub(i, i)
      if sep == ']' then return arr, i + 1 end
      if sep ~= ',' then error("expected , or ] at " .. i) end
      i = i + 1
    end
  elseif c == '{' then
    local obj = {}
    i = skip_ws(s, i + 1)
    if s:sub(i, i) == '}' then return obj, i + 1 end
    while true do
      local key
      key, i = decode_value(s, i)
      i = skip_ws(s, i)
      if s:sub(i, i) ~= ':' then error("expected : at " .. i) end
      local val
      val, i = decode_value(s, i + 1)
      obj[key] = val
      i = skip_ws(s, i)
      local sep = s:sub(i, i)
      if sep == '}' then return obj, i + 1 end
      if sep ~= ',' then error("expected , or } at " .. i) end
      i = i + 1
    end
  elseif c == 't' and s:sub(i, i + 3) == 'true'  then return true,  i + 4
  elseif c == 'f' and s:sub(i, i + 4) == 'false' then return false, i + 5
  elseif c == 'n' and s:sub(i, i + 3) == 'null'  then return nil,   i + 4
  elseif c:match('[%-0-9]') then
    local numstr = s:match('^-?%d+%.?%d*[eE]?[+-]?%d*', i)
    return tonumber(numstr), i + #numstr
  else
    error("unexpected character '" .. c .. "' at position " .. i)
  end
end

function json.decode(s)
  local val, _ = decode_value(s, 1)
  return val
end

return json
JSONLUA

    # Pre-provide test.lua so agents don't have to write their own test runner
    cat > "${TARGET}/test.lua" <<'TESTLUA'
-- test.lua: minimal test runner
-- Usage:
--   local test = require("test")
--   test("my test", function() assert(1 == 1) end)
--   test.summary()  -- prints "N/N tests passed", exits 1 if any failed

local M = {}
local passes = 0
local failures = 0

function M.run(name, fn)
  local ok, err = pcall(fn)
  if ok then
    print("PASS: " .. name)
    passes = passes + 1
  else
    print("FAIL: " .. name .. ": " .. tostring(err))
    failures = failures + 1
  end
end

setmetatable(M, { __call = function(_, name, fn) return M.run(name, fn) end })

function M.summary()
  local total = passes + failures
  print(string.format("%d/%d tests passed", passes, total))
  os.exit(failures > 0 and 1 or 0)
end

return M
TESTLUA

    (
        cd "${TARGET}"
        git init -q
        echo -e "todos.json\ntest_todo_*.json" > .gitignore
        git add -A
        git commit -q -m "init: scaffold with json.lua and test.lua"
    )
    ok "Lua target ready at ${TARGET}"
}

target_validation_commands() {
    true
}

target_goal() {
    echo "Build a Lua command-line todo application. The app should support: add, list, done, and delete commands. Persist todos to a JSON file using pure Lua (no external dependencies). Include tests in test_todo.lua that verify all operations. Entry point is cli.lua, core logic in todo.lua."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/lua-todo.md"
}

collect_results() {
    for f in todo.lua cli.lua test_todo.lua; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done
}

verify() {
    local pass=true

    # Check files exist
    for f in todo.lua cli.lua test_todo.lua; do
        if [[ -f "${TARGET}/${f}" ]]; then
            ok "${f} exists"
        else
            warn "${f} missing"
            pass=false
        fi
    done

    # Run the CLI
    echo ""
    if (cd "${TARGET}" && lua cli.lua add "Test item from e2e" 2>&1); then
        ok "cli.lua add succeeded"
        if (cd "${TARGET}" && lua cli.lua list 2>&1 | grep -q "Test item"); then
            ok "cli.lua list shows the added item"
        else
            warn "cli.lua list did not show the added item"
            pass=false
        fi
    else
        warn "cli.lua add failed"
        pass=false
    fi

    # Run tests
    echo ""
    if (cd "${TARGET}" && lua test_todo.lua 2>&1 | /usr/bin/tail -15); then
        ok "tests completed"
    else
        warn "tests had failures"
        pass=false
    fi

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
    fi
}
