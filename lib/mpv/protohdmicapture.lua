-- protoHDMIcapture keys and overlay for the feed window. Loaded by view.sh
-- with --script; bindings are forced, so they win over input.conf for this
-- window only.
--
--   Tab  open or close the overlay: the keys, the live settings and the
--        source's setup tips (see GLOSSARY.md)
--   f    cycle the display mode: fullscreen -> borderless -> windowed
--   d    open the live debug dashboard
--   1-8  colour (mpv's own keys); s screenshot (mpv's own)
--   m    mute, 9 / 0 volume down / up: the audio loopback, when the sound
--        runs through it (AUDIO=loopback), not mpv's own (it has none)
--   While the overlay is open: l levels, c scaling, r reset colour,
--   t setup tips (left/right: other sources), w save as defaults, Esc close.
--
-- Changes are live changes: they last until the feed closes, unless w saves
-- them to the config through `view.sh --save`.
--
-- mpv's own window properties are set here, which is all most desktops need;
-- then `view.sh --place-window MODE` lets the desktop adapter finish where
-- the compositor ignores a client (size, maximize and border on Hyprland).
--
-- It also marks a normal shutdown in `quitfile`: mpv sometimes segfaults in
-- libavdevice's v4l2 teardown after it has already quit (an upstream bug),
-- and view.sh uses the mark to tell that apart from a crash mid-feed.

local msg = require("mp.msg")
local utils = require("mp.utils")

local opts = { helper = "", mode = "fullscreen", quitfile = "", scaling = "balanced",
               levels = "auto", sources = "", source = "", audio = "" }
require("mp.options").read_options(opts, "protohdmicapture")

local function helper(...)
    if opts.helper == "" then return end
    mp.command_native_async({ name = "subprocess", args = { opts.helper, ... },
                              playback_only = false, capture_stderr = true }, function() end)
end

local function set(prop, value)
    local ok, err = mp.set_property(prop, value)
    if not ok then msg.error("cannot set " .. prop .. "=" .. tostring(value) .. ": " .. tostring(err)) end
    return ok
end

-- --- display mode -----------------------------------------------------------
local modes = { "fullscreen", "borderless", "windowed" }
local names = { fullscreen = "Fullscreen", borderless = "Borderless", windowed = "Windowed" }
local current = opts.mode

local function apply_mode(mode)
    current = mode
    if mode == "fullscreen" then
        mp.set_property_native("fullscreen", true)
    else
        mp.set_property_native("fullscreen", false)
        mp.set_property_native("border", mode == "windowed")
        mp.set_property_native("title-bar", mode == "windowed")
        mp.set_property_native("window-maximized", mode == "borderless")
    end
    helper("--place-window", mode)
end

local function next_of(list, value)
    for i, v in ipairs(list) do
        if v == value then return list[i % #list + 1] end
    end
    return list[1]
end

-- --- scaling ----------------------------------------------------------------
-- Applied here rather than on the command line, so all three can be cycled
-- live. The snapshot is what mpv.conf (plus --profile=low-latency) gave, i.e.
-- SCALING=config; the other two override it. "fast" is the list
-- `mpv --show-profile=fast` prints on 0.41.0.
local SCALE_KEYS = { "scale", "cscale", "dscale", "deband", "dither", "correct-downscaling",
                     "linear-downscaling", "sigmoid-upscaling", "hdr-compute-peak",
                     "allow-delayed-peak-detect" }
local SCALINGS = {
    balanced = { scale = "spline36", cscale = "spline36", dscale = "mitchell", deband = "no", dither = "no" },
    fast = { scale = "bilinear", dscale = "bilinear", dither = "no", ["correct-downscaling"] = "no",
             ["linear-downscaling"] = "no", ["sigmoid-upscaling"] = "no", ["hdr-compute-peak"] = "no",
             ["allow-delayed-peak-detect"] = "yes" },
    config = {},
}
local scaling_order = { "balanced", "fast", "config" }
local snapshot = {}
for _, k in ipairs(SCALE_KEYS) do snapshot[k] = mp.get_property(k) end
local scaling = opts.scaling

local function apply_scaling(mode)
    if not SCALINGS[mode] then msg.error("unknown scaling " .. mode); return end
    for _, k in ipairs(SCALE_KEYS) do set(k, SCALINGS[mode][k] or snapshot[k]) end
    scaling = mode
    msg.info("scaling: " .. mode)
end
apply_scaling(scaling)

-- --- levels -----------------------------------------------------------------
-- The range of the card's frames, as a labelled format filter (view.sh adds
-- it at start when LEVELS is not auto).
local levels_order = { "auto", "limited", "full" }
local levels = opts.levels

local function apply_levels(value)
    for _, f in ipairs(mp.get_property_native("vf") or {}) do
        if f.label == "phc-levels" then
            local _, err = mp.command_native({ "vf", "remove", "@phc-levels" })
            if err then msg.error("cannot remove the levels filter: " .. err); return end
        end
    end
    if value ~= "auto" then
        local _, err = mp.command_native({ "vf", "add", "@phc-levels:format:colorlevels=" .. value })
        if err then msg.error("cannot set levels " .. value .. ": " .. err); return end
    end
    levels = value
end

-- --- colour -----------------------------------------------------------------
local COLOUR = { "contrast", "brightness", "gamma", "saturation" }

local render_page   -- defined with the overlay below

-- --- audio loopback -------------------------------------------------------
-- view.sh owns the loopback; `view.sh --audio` changes its stream and
-- prints the result, so the panel shows what is, not what was asked.
local audio = { volume = nil, mute = nil, error = nil }

local function audio_ctl(...)
    local args = { opts.helper, "--audio", ... }
    mp.command_native_async({ name = "subprocess", args = args, playback_only = false,
                              capture_stdout = true, capture_stderr = true }, function(_, r)
        if r and r.status == 0 then
            audio.volume = tonumber(r.stdout:match("volume=(%d+)"))
            audio.mute = r.stdout:match("mute=(%a+)") == "yes"
            audio.error = nil
            mp.osd_message(audio.mute and "Audio muted" or ("Volume " .. audio.volume .. "%"), 1.5)
        else
            audio.error = ((r and r.stderr ~= "" and r.stderr) or (r and r.error_string) or "failed"):gsub("%s+$", "")
            msg.error("audio: " .. audio.error)
            mp.osd_message("Audio: " .. audio.error, 3)
        end
        render_page()
    end)
end

-- --- source profiles (sources/<id>.md; shape in lib/sources.sh) --------------
local function read_source(id)
    local f = io.open(opts.sources .. "/" .. id .. ".md", "r")
    if not f then return nil end
    local s = { id = id, name = id, status = "", tips = {} }
    local first = true
    for line in f:lines() do
        if first then s.name = line:gsub("^# ", ""); first = false
        elseif line:match("^## ") then break
        elseif line:match("^Status: ") then s.status = line:gsub("^Status: ", "")
        elseif line:match("^%- ") then s.tips[#s.tips + 1] = line:sub(3)
        end
    end
    f:close()
    return s
end

local function source_ids()
    local ids = {}
    for _, name in ipairs(utils.readdir(opts.sources, "files") or {}) do
        local id = name:match("^(.+)%.md$")
        if id and id ~= "generic" then ids[#ids + 1] = id end
    end
    table.sort(ids)
    ids[#ids + 1] = "generic"
    return ids
end

-- --- overlay ----------------------------------------------------------------
local overlay = mp.create_osd_overlay("ass-events")
local page = nil            -- nil (closed), "panel" or "tips"
local tips_id = opts.source ~= "" and opts.source or "generic"

-- Leading spaces become hard spaces: libass drops ordinary ones at the start
-- of a line, which would lose the tips' hanging indent.
local function ass_escape(s)
    s = s:gsub("\\", "\\\\"):gsub("{", "\\{"):gsub("}", "\\}")
    return (s:gsub("^( +)", function(sp) return ("\\h"):rep(#sp) end))
end

local function wrap(text, width)
    local lines, line = {}, ""
    for word in text:gmatch("%S+") do
        if line ~= "" and #line + 1 + #word > width then
            lines[#lines + 1] = line; line = word
        else
            line = line == "" and word or line .. " " .. word
        end
    end
    if line ~= "" then lines[#lines + 1] = line end
    return lines
end

-- What a save would write: each saved setting that differs from the start.
local baseline = {}
local function current_settings()
    local s = { WINDOW = current, LEVELS = levels, SCALING = scaling }
    for _, k in ipairs(COLOUR) do s[k:upper()] = tostring(mp.get_property_number(k, 0)) end
    return s
end
local function changes()
    local out, now = {}, current_settings()
    for _, k in ipairs({ "WINDOW", "LEVELS", "SCALING", "BRIGHTNESS", "CONTRAST", "SATURATION", "GAMMA" }) do
        if now[k] ~= baseline[k] then out[#out + 1] = k .. "=" .. now[k] end
    end
    return out
end

local function panel_lines()
    local colour = {}
    for _, k in ipairs(COLOUR) do colour[#colour + 1] = k .. " " .. mp.get_property_number(k, 0) end
    local src = read_source(tips_id)
    local ch = changes()
    return {
        "protoHDMIcapture",
        "",
        "f     Display      " .. names[current],
        "l     Levels       " .. levels,
        "c     Scaling      " .. scaling .. (scaling == "config" and " (mpv.conf)" or ""),
        "1-8   Colour       " .. table.concat(colour, " · "),
        opts.audio == "loopback"
            and ("m 9 0 Audio        " .. (audio.error and ("unavailable: " .. audio.error)
                 or audio.volume and ((audio.mute and "muted · " or "") .. "volume " .. audio.volume .. "%")
                 or "volume 100% (unchanged)"))
            or ("      Audio        " .. (opts.audio == "" and "?" or opts.audio)),
        "r     Reset colour",
        "t     Setup tips   " .. (src and src.name or tips_id),
        "w     Save         " .. (#ch == 0 and "nothing changed" or table.concat(ch, " ")),
        "",
        "s screenshot   d dashboard   Tab/Esc close   q quit",
    }
end

local function tips_lines()
    local src = read_source(tips_id)
    if not src then return { "No setup tips for '" .. tips_id .. "'", "", "◂ ▸ other sources   t back" } end
    local level = src.status:match("^(%S+)") or ""
    local out = { "Setup tips · " .. src.name .. (level ~= "" and level ~= "reported" and " · " .. level or ""), "" }
    for _, tip in ipairs(src.tips) do
        for i, l in ipairs(wrap(tip, 64)) do out[#out + 1] = (i == 1 and "• " or "  ") .. l end
    end
    out[#out + 1] = ""
    out[#out + 1] = "◂ ▸ other sources   t back   Tab/Esc close"
    return out
end

render_page = function()
    if not page then overlay:remove(); return end
    local w, h = mp.get_osd_size()
    if not w or w == 0 then w, h = 1280, 720 end
    overlay.res_x, overlay.res_y = w, h
    local lines = page == "tips" and tips_lines() or panel_lines()
    local fs = math.max(14, math.floor(h / 40))
    local lh = math.floor(fs * 1.3)
    local cw = fs * 0.6
    local maxc = 0
    -- characters, not bytes: "·" and "◂" are several bytes each
    for _, l in ipairs(lines) do maxc = math.max(maxc, select(2, l:gsub("[^\128-\191]", ""))) end
    local pad = fs
    local bw, bh = math.floor(maxc * cw + 2 * pad), #lines * lh + 2 * pad
    -- Below the line mpv's own OSD messages use, so they never overlap.
    local x0, y0 = math.floor(fs * 1.5), math.floor(fs * 3.5)
    local ev = {
        string.format("{\\an7\\pos(%d,%d)\\bord0\\shad0\\1c&H101010&\\1a&H30&\\p1}m 0 0 l %d 0 %d %d 0 %d{\\p0}",
                      x0, y0, bw, bw, bh, bh),
    }
    for i, l in ipairs(lines) do
        local style = i == 1 and "\\b1\\1c&HFFFFFF&" or "\\b0\\1c&HDDDDDD&"
        ev[#ev + 1] = string.format("{\\an7\\pos(%d,%d)\\fnmonospace\\fs%d\\bord1\\3c&H000000&\\shad0%s}%s",
                                    x0 + pad, y0 + pad + (i - 1) * lh, fs, style, ass_escape(l))
    end
    overlay.data = table.concat(ev, "\n")
    overlay:update()
end

-- Keys that only mean something while the overlay is open.
local open_keys = {}
local function bind_open(key, name, fn)
    open_keys[#open_keys + 1] = name
    mp.add_forced_key_binding(key, name, fn, { repeatable = true })
end

local function close()
    for _, name in ipairs(open_keys) do mp.remove_key_binding(name) end
    open_keys = {}
    page = nil
    render_page()
end

local function cycle_source(step)
    local ids = source_ids()
    for i, id in ipairs(ids) do
        if id == tips_id then tips_id = ids[(i - 1 + step) % #ids + 1]; render_page(); return end
    end
    tips_id = ids[1]; render_page()
end

local function save()
    local ch = changes()
    if #ch == 0 then mp.osd_message("Nothing changed since the feed started", 2); return end
    if opts.helper == "" then mp.osd_message("Cannot save: no helper", 3); return end
    local args = { opts.helper, "--save" }
    for _, kv in ipairs(ch) do args[#args + 1] = kv end
    local r = mp.command_native({ name = "subprocess", args = args, playback_only = false,
                                  capture_stdout = true, capture_stderr = true })
    if r.status == 0 then
        baseline = current_settings()
        mp.osd_message("Saved: " .. table.concat(ch, " "), 3)
    else
        local why = (r.stderr ~= "" and r.stderr or r.error_string or ("exit " .. tostring(r.status))):gsub("%s+$", "")
        msg.error("save failed: " .. why)
        mp.osd_message("Save failed: " .. why, 5)
    end
    render_page()
end

local function open(which)
    if not page then
        bind_open("ESC", "phc-close", close)
        bind_open("l", "phc-levels", function() apply_levels(next_of(levels_order, levels)); render_page() end)
        bind_open("c", "phc-scaling", function() apply_scaling(next_of(scaling_order, scaling)); render_page() end)
        bind_open("r", "phc-reset-colour", function()
            for _, k in ipairs(COLOUR) do set(k, 0) end
        end)
        bind_open("w", "phc-save", save)
        bind_open("t", "phc-tips", function() page = page == "tips" and "panel" or "tips"; render_page() end)
        bind_open("LEFT", "phc-prev-source", function() if page == "tips" then cycle_source(-1) end end)
        bind_open("RIGHT", "phc-next-source", function() if page == "tips" then cycle_source(1) end end)
    end
    page = which
    render_page()
end

mp.add_forced_key_binding("TAB", "protohdmicapture-overlay", function()
    if page then close() else open("panel") end
end)

mp.add_forced_key_binding("f", "protohdmicapture-cycle-window", function()
    apply_mode(next_of(modes, current))
    if page then render_page() else mp.osd_message("Display: " .. names[current], 1.5) end
end)

if opts.audio == "loopback" then
    mp.add_forced_key_binding("m", "protohdmicapture-mute", function() audio_ctl("mute") end)
    mp.add_forced_key_binding("9", "protohdmicapture-volume-down", function() audio_ctl("volume", "-5") end,
                              { repeatable = true })
    mp.add_forced_key_binding("0", "protohdmicapture-volume-up", function() audio_ctl("volume", "+5") end,
                              { repeatable = true })
end

mp.add_forced_key_binding("d", "protohdmicapture-dashboard", function()
    helper("--open-dashboard")
    mp.osd_message("Opening the debug dashboard", 1.5)
end)

for _, k in ipairs(COLOUR) do mp.observe_property(k, "number", function() if page then render_page() end end) end
mp.observe_property("osd-dimensions", "native", function() if page then render_page() end end)

-- The start, for "what changed": options are applied before scripts load.
baseline = current_settings()

mp.register_event("shutdown", function()
    if opts.quitfile == "" then return end
    local f = io.open(opts.quitfile, "w")
    if f then f:write("quit\n"); f:close() end
end)
