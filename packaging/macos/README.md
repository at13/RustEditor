# macOS app bundle

`cargo bundle` (https://github.com/burtonageo/cargo-bundle) builds
`RustEditor.app` from the `[package.metadata.bundle]` section of the root
`Cargo.toml`:

```sh
cargo install cargo-bundle
cargo bundle --release
# -> target/release/bundle/osx/RustEditor.app
```

The bundle declares plain-text document types, so Finder's "Open With" and
drag-and-drop onto the Dock icon work. Those arrive as Apple Events (not
command-line arguments); the app receives them through GPUI's
`on_open_urls` and opens them like forwarded paths.

Not done yet (needs a macOS machine and an Apple Developer ID): an app icon
(`icon` in the metadata), code signing, notarization, and a `.dmg`. The
bundle, "Open With" and the keymap's macOS section have not been tested on a
Mac.
