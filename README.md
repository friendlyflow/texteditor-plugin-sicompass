# texteditor-plugin-sicompass

*Edit text files as lists, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

The text editor opens a text file as a list of its lines, each shown as it is
in the file, indentation included. i edits a line, ctrl+a
and ctrl+i insert one, ctrl+d deletes one, and every change is written to disk
and can be undone with ctrl+z.

It starts in the folder set in Settings, under text editor, which is your home
folder until you change it.

## .ff files

A `.ff` file is not text but a tree of nodes, stored as binary. It opens as
that tree. A node with children, or one whose text ends in `:`, is a level you
enter with Right. Every Enter writes the file again, and every change can be
undone.

Each node holds a SHA-256 hash of its children's hashes followed by the hash
of its own text, so the file is a Merkle tree and its root hash changes
whenever anything in it does. Every level opens with a header row, and
Right on it shows the hash of the node whose children the level holds. At the
top that is the root hash of the whole file. On disk a node is its hash (32 bytes), the
number of its children (one byte), the children themselves, the length of its
text (one byte) and the text in UTF-8. A file is one root node with empty
text, so a node holds at most 255 children and 255 bytes of text. A file whose
hashes do not match still opens, with a warning, and the next save writes
them right.

The text editor asks for your whole disk, so it can open any file you point it
at. The Store shows that before you install it, and installing it is your
approval.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
texteditor. The Store checks the release's signature before installing it, and
keeps it up to date.

To install a build of your own instead, copy `plugin.json`, the built
`plugin` program (`plugin.exe` on Windows) and `locales/` into a folder named
`texteditor` in the Sicompass plugins folder (`~/.config/sicompass/plugins/`
on Linux, `~/Library/Application Support/sicompass/plugins/` on macOS) and
restart Sicompass.

## Building from source

```bash
nix develop          # the toolchain
cargo test
cargo build --release
cp target/release/texteditor-plugin plugin
```

`./scripts/release-plugin.sh --dry-run` builds this computer's release, packs
it, and signs and verifies it with a throwaway key, the way a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK, the plugin kit and the cloud backup library

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
