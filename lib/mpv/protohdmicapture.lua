-- protoHDMIcapture keys for the feed window. Loaded by view.sh with --script;
-- bindings are forced, so they win over input.conf for this window only.
--
--   f   cycle the display mode: fullscreen -> borderless -> windowed
--   d   open the live debug dashboard
--
-- mpv's own window properties are set here, which is all most desktops need;
-- then `view.sh --place-window MODE` lets the desktop adapter finish where
-- the compositor ignores a client (size, maximize and border on Hyprland).

local opts = { helper = "", mode = "fullscreen" }
require("mp.options").read_options(opts, "protohdmicapture")

local modes = { "fullscreen", "borderless", "windowed" }
local names = { fullscreen = "Fullscreen", borderless = "Borderless", windowed = "Windowed" }
local current = opts.mode

local function helper(...)
    if opts.helper == "" then return end
    mp.command_native_async({ name = "subprocess", args = { opts.helper, ... },
                              playback_only = false, capture_stderr = true }, function() end)
end

local function apply(mode)
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
    mp.osd_message("Display: " .. names[mode], 1.5)
end

mp.add_forced_key_binding("f", "protohdmicapture-cycle-window", function()
    for i, m in ipairs(modes) do
        if m == current then
            apply(modes[i % #modes + 1])
            return
        end
    end
    apply("fullscreen")
end)

mp.add_forced_key_binding("d", "protohdmicapture-dashboard", function()
    helper("--open-dashboard")
    mp.osd_message("Opening the debug dashboard", 1.5)
end)
