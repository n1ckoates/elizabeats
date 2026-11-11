# Changelog

All notable changes to Elizabeats. Entries come from the files in `.changeset/` (see `knope.toml`).

## 1.0.3

### Fixes

- don't leave space for traffic lights in fullscreen

#### No extra gap to the left of the header in fullscreen

In fullscreen, the header left room for the macOS window buttons, but they were hidden so it was just a blank area.

## 1.0.2

### Fixes

#### Cmd+K selects the search text

Pressing Cmd+K (Ctrl+K on Windows and Linux) to jump to the search box now selects whatever is already in it, so you can start typing right away to replace it, like Cmd+L in a browser.

## 1.0.1

### Fixes

#### Selecting text in the search box no longer drags the track behind it

Clicking or dragging in the search box could start dragging a search result hidden underneath it. The search box now keeps the mouse to itself.

## 1.0.0

Initial release of Elizabeats.
