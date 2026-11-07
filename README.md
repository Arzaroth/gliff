# gliff

[![CI](https://github.com/omacom/gliff/actions/workflows/ci.yml/badge.svg)](https://github.com/omacom/gliff/actions/workflows/ci.yml)

gliff is a remote desktop for Hyprland. It shows the Hyprland session of
another machine in a window on yours, and all it needs between the two is
ssh: no open ports, no accounts, no relay.

Text stays sharp. Most remote desktops send video with halved colour
resolution, which blurs coloured text; gliff sends full-resolution colour
(4:4:4) through the H.264 hardware every GPU has. Video runs on the GPU at
both ends, and falls back to the CPU on machines where it cannot.

![A gliff window showing a terminal on a remote machine](.github/assets/screenshot.png)

## What you get

- Mirror any screen of the remote machine, or get a private screen that
  exists only for your session and follows the size of your window.
- Your keyboard layout, not the remote's: keys map the same on both ends.
- A shared clipboard in both directions: text, images, and copied files.
- The remote machine's sound, in both directions, so two machines can show
  each other at once without echo.
- Your microphone on the remote machine, as an input its applications can
  record from.
- Quality that adapts to the link, giving up frame rate before sharpness.
- Reconnection when the link drops.
- A window that follows your Omarchy theme.

## Requirements

- Hyprland on both machines, with a session running on the remote one.
- For sound from the remote: PipeWire (`pw-record`, `pw-dump`, `pw-link`),
  or PulseAudio's `parec`. Playback uses the client's ALSA default device;
  install `pipewire-alsa` or `pulseaudio-alsa` to route it through your sound server.
- For the microphone: PipeWire on both machines (`pw-record` and
  `pw-metadata` on yours, `pw-cli` and `pw-metadata` on the remote one).
- ssh access from your machine to the remote one.
- For GPU video: an AMD or Intel GPU with its Vulkan and VA-API drivers
  (on Arch, `vulkan-radeon` and `mesa` for AMD, or `vulkan-intel` and
  `intel-media-driver` for Intel). Without them, and on NVIDIA, gliff uses
  the CPU and needs nothing extra.

## Install

Install gliff on both machines: your machine runs `gliff`, the remote one
runs `gliff-server`.

Gliff is currently available on the **edge** channel of the [Omarchy Package
Repository (OPR)](https://github.com/omacom/omarchy-pkgs):

```
omarchy pkg add gliff
```

Then open **Gliff** from the app launcher, or run `gliff` in a terminal.

If your channel reports `target not found: gliff`, or you use Arch without
OPR, build and install the package from source:

```
git clone https://github.com/omacom/gliff.git
cd gliff
bin/install
```

This uses the current checkout, including fixes newer than the last release.

To build from source, see [docs/development.md](docs/development.md).

## Use

```
gliff user@host
```

This mirrors the screen that has focus on the remote machine. To pick a
screen, or to get a private one:

```
gliff --output DP-1 user@host   # mirror a named remote screen
gliff --headless user@host      # a private remote screen, sized and
                                # scaled to this window
```

A mirrored screen keeps its own size and is fitted into the window. A private
screen follows the window as you resize it, and is removed when you
disconnect.

When the remote machine has more than one screen, the screen button in the
header lists them and switches the mirrored one without reconnecting. The
list is read again each time the menu opens, so it shows screens plugged in
since. A private screen cannot be switched.

**All screens** in the same menu shows every screen at once, laid out as
they stand on the remote machine. Each extra screen is a connection of its
own (one more ssh session per screen), so each keeps full sharpness and its
own adaptation to the link. Keyboard and pointer go to the screen under the
pointer. Sound, the microphone and the clipboard stay with the tab's own
connection. Picking a single screen goes back to one.

Every machine you connect to gets a tab in the title bar. Click a tab to
connect to that machine, or to switch to it: each connection keeps running
while you look at another, so switching is instant. Hover a tab for its
button: a stop square disconnects a running machine, and an X forgets one
that is not running. Drag the tabs to reorder them. The + after the tabs
opens the address bar; type `user@host` and press Enter to add a machine.
A bare `gliff` opens the window with a tab for each machine you have used,
and connects again to the ones that were connected when you closed it; with
no machines yet, it opens with the address bar ready.

**Keyboard.** While the pointer is over the picture, everything you type goes
to the remote machine, window-manager shortcuts included; move the pointer
off it to get your own back. Press `Shift+Esc` to get your shortcuts back
without moving the pointer. `--release-hotkey` changes that key (`ctrl+alt+q`,
`double-escape`, or `none`).

**Session menu.** The ⋮ button in the header holds the rest:

- *View only* stops sending your keyboard and pointer, to watch without
  touching anything. The window then leaves the keyboard alone, so your
  own shortcuts keep working with the pointer over the picture.
- *Send keys* sends a shortcut your own desktop would keep for itself:
  Super, Alt+Tab, Shift+Esc, Ctrl+Alt+Delete or Print Screen.
- *Lock remote screen* locks the shown machine now, and *Lock when
  disconnecting* locks every machine whose connection ends, however it
  ends, and is remembered. The lock goes through logind, so the remote's
  own idle daemon shows its usual lock screen (on Omarchy, hypridle's
  `lock_cmd`).

**Clipboard.** Copy on one machine and paste on the other. A copy announces
only its types, and the contents are sent when the other side asks for them,
normally when you paste. A paste that takes more than a second shows its
progress with a cancel button. Pasted files are kept in `~/.cache/gliff/clipboard`.

What you copy locally is offered only to the machine in the tab you are
looking at. Switching tabs withdraws it from the old machine and offers your
current clipboard to the new one.

Clipboard sharing trusts the host you are looking at: it can request the
advertised clipboard contents without a local paste. Use `gliff --no-clipboard`
to disable sharing in both directions when connecting to an untrusted host.
Local selections marked `x-kde-passwordManagerHint` are not shared.

Changing the clipboard cancels unfinished pastes from this machine to the
remote host. Wait for a file paste to finish before copying something else. Switching
tabs also cancels unfinished pastes to the tab you leave.

**Sound.** The window plays the sound of the machine it shows. The speaker
button in the header mutes it and remembers that; `gliff --no-audio` starts
muted. Only the tab you look at plays, and other tabs' hosts stop recording.
A host with PipeWire's `pw-record` and `pw-dump` sends what its applications
play except Gliff itself, so when two machines show each other, neither hears
its own sound come back. A host missing either sends its whole output, Gliff
included, and two such machines that show each other echo; mute one side to
stop it. Hosts keep playing their sound on their own speakers. Sound is Opus
at 128 kbit/s, and the client buffers 40 to 250 ms of it, more on a jittery
link.

**Microphone.** The microphone button in the header sends your microphone to
the machine in the tab you look at. It starts off, and `gliff --mic` starts
with it on. The remote machine gets an input named "Gliff microphone",
which becomes its default input while your microphone is on; its previous
default comes back when you turn the microphone off or disconnect. Gliff
records your default input as it is when the microphone turns on, or the
PipeWire node you name with `--mic-source` (`wpctl status` lists them), and
stays on it. With speakers rather than a headset, the remote's sound reaches
your microphone and goes back to it; name an echo-cancelled source, such as
one from PipeWire's `module-echo-cancel`, with `--mic-source`.

**Slow links.** gliff measures the link and lowers the frame rate, then the
colour detail, then the resolution, and raises them again when there is
room. Two flags change the video, and you pass them to the server through
`--server-bin`: `--low-bandwidth` sends 4:2:0 colour from the start, and
`--full-chroma` keeps 4:4:4 on a server that encodes on the CPU.

```
gliff --server-bin 'gliff-server --low-bandwidth' user@host
```

## If something does not work

- Run `gliff-probe all` on each machine. It checks the compositor, the GPU
  drivers and both video paths, and prints PASS or FAIL for each.
- If ssh cannot find `gliff-server`, give its path:
  `gliff --server-bin /path/to/gliff-server user@host`.
- The server needs the remote user's running Hyprland session, which it
  finds under `XDG_RUNTIME_DIR`, or `/run/user/<uid>` when ssh does not set
  it. If the session is elsewhere, set it through
  `--server-bin 'env XDG_RUNTIME_DIR=/path gliff-server'`.
- To rule out a driver problem, force the CPU on your end with
  `gliff --video cpu user@host`.
- If the window connects but stays black, the remote GPU may allocate
  capture buffers it never fills (a VM's virtio-gpu with virgl, or a GPU the
  compositor does not render on). Have the compositor read the frames back
  instead: `gliff --server-bin 'gliff-server --capture shm' user@host`.
  The server then encodes on the CPU.
- Screens wider than 4096 pixels are streamed at a reduced size and scaled
  back up.

## More

- [docs/architecture.md](docs/architecture.md): how it works, the design
  choices, measurements, and what is not built yet.
- [docs/hardware-quirks.md](docs/hardware-quirks.md): what each GPU driver
  and Hyprland do, and what is tested where.
- [docs/development.md](docs/development.md): building, running on one
  machine, and the tests.

gliff is released under the MIT license.
