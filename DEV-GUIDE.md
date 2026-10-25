# Development guide

## Desktop app (desktop-gpui)

Build the browser payload once (embedded at compile time into both the
desktop app and app-daemon):

    make web-client-build

Run the desktop app:

    make desktop

The desktop app bundles the daemon's server: the native UI and the web view
at http://127.0.0.1:3081/ (settings → Web View) share the SAME session — a
single librqbit instance serves both. No app-daemon needed.

## Headless daemon (app-daemon)

    make daemon

Serves the web client at http://localhost:3080/ and the WS protocol at /ws.

librqbit has no shared/multi-instance sessions: each process owns its own
session. Running both at once is fine port-wise (desktop: 3030 HTTP API +
3081 web view; daemon: 3080), but they hold independent sessions — use the
desktop app's bundled web view when you want the browser and the native UI
on the same torrents.