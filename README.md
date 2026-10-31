# Switch Controller Connector

A small Windows app that connects Nintendo Switch controllers (Pro Controller and Joy-Con) to your PC over Bluetooth, including ones Windows has paired before and won't reconnect.

## The problem

Once you sync a controller with a Switch, the controller forgets its pairing with your PC. Windows still lists it as "Paired", so it never shows up as a new device and won't connect. This app removes the old pairing and pairs again in one click.

## Download

**[Download SwitchControllerConnector.exe](../../raw/main/download/SwitchControllerConnector.exe)** (Windows 10/11). It's a single file with no install and no admin rights needed.

Windows SmartScreen may warn you the first time because the app isn't signed. Click **More info → Run anyway**.

## How to use

1. Hold the small **SYNC** button on the controller until the lights run back and forth.
   - Joy-Con: on the rail, between SL and SR.
   - Pro Controller: on top, next to the USB-C port.
2. Select the controller in the list and click **Connect**.
3. Optional: type a nickname and click **Save name** so you can tell your controllers apart.

Other options: **Auto-connect** pairs any controller as soon as it enters pairing mode, **Forget pairing** removes a saved pairing, and **Rescan** restarts the search.

The controller's lights keep cycling after it connects. That's normal on a PC.

## Build from source

Requires [Rust](https://rustup.rs) and MinGW (for `windres`, which adds the icon).

```
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Built with Rust, the [`windows`](https://crates.io/crates/windows) crate and [`native-windows-gui`](https://crates.io/crates/native-windows-gui).
