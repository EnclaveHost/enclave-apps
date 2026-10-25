
## Desktop automation integration (source, not released image)

The exact POST routes `/exec` and `/computer` now forward the controller's
already-authenticated automation requests. The separate automation key is
owned only by the Wasm controller and is never forwarded. Both gateway startup
checks refuse an inherited `KVMBOX_AUTOMATION_KEY`.

The inner gateway requires `/run/native-automation/desktop.sock` with directory
1000:1000 mode 0750, socket 1000:1000 mode 0660 and connected peer UID 1000. It
validates a strict JSON request up to 64 KiB, uses a 125-second absolute exchange
deadline, and admits only bounded JSON replies with explicit length. Desktop
headers, cookies and redirects are not relayed. The outer response-header
limit is 130 seconds to accommodate the 120-second maximum command deadline and
bounded cleanup. No TCP desktop automation listener or direct GPU route is
introduced. Packaging and confined-VM acceptance remain required.
