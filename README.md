# texteditor_plugin_sicompass

*Edit text files as lists, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

The text editor opens a text file as a list of its lines. Indentation and
braces in Python, C-family, Rust, Go, JavaScript and TypeScript files become
levels, so you walk a function the way you walk a folder. i edits a line, ctrl+a
and ctrl+i insert one, ctrl+d deletes one, and every change is written to disk
and can be undone with ctrl+z.

It starts in the folder set in Settings, under text editor, which is your home
folder until you change it.

The text editor asks for your whole disk, so it can open any file you point it
at. The Store shows that before you install it, and installing it is your
approval.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
texteditor. The Store checks the release's signature before installing it, and
keeps it up to date.

## Building from source

```bash
nix develop          # the toolchain, with the wasm32-wasip2 target
cargo test           # natively
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/texteditor_plugin.wasm plugin.wasm
```

`./scripts/release-plugin.sh --dry-run` does the build, checks the component
against `plugin.json`, and signs and verifies it with a throwaway key, the way
a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK, the WASM plugin kit and the cloud backup library

## Community

Join the conversation on
[Discord](https://discord.com/channels/1464152138753249313/1464152139231137894).

## License

#### Open source license

If you are creating an open source application under a license compatible with
the GNU GPL license v3, you may use this project under the terms of the GPLv3.
See [LICENSE](LICENSE).

## Contributing

Contributions are welcome. Whether it is code, documentation, or feedback, your
input helps make computing more accessible for everyone.
