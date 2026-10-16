# Changelog

## Unreleased

- A text file opens as its lines, each with its indentation, like any text
  editor. Braces and Python's `:` no longer open levels, and an edit writes
  the line as you typed it, indentation included.
- Opens, edits and saves `.ff` files, a binary tree of nodes where every node
  carries a SHA-256 hash of its children and its text (a Merkle tree). A node
  whose text ends in `:` is a level to enter. Every Enter writes the file, and
  inserting and deleting nodes can be undone.
- Every level of a `.ff` file opens with a header row holding the hash of the
  node it lists, the root hash at the top.

## 0.3.1

- When creating, deleting, renaming or saving fails, it says why.
- In a folder or file you cannot write to, adding is refused before you type
  anything.
- Creating a file never empties a file that already exists.

## 0.3.0

Text Editor is a program of its own now, instead of a sandboxed WebAssembly component.
Sicompass starts it and talks to it, one per tab, and it runs with your rights, so
its entry in the Store says what it does before you install it, and installing it is
your approval.

- Deleting still goes to the trash and can be undone. Opening a very large file reads it whole, as before.
- One build for each of Linux (x86_64 and arm64, static), macOS (Apple Silicon and
  Intel) and Windows.
- Needs a Sicompass that runs plugin programs. An older Sicompass keeps the 0.2
  version it has.
