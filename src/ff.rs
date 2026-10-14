//! `.ff` files: a tree of nodes stored as a Merkle tree.
//!
//! A node's hash covers its children's hashes and its own content:
//!
//! ```text
//! hash = sha256(child_0.hash ‖ … ‖ child_n.hash ‖ sha256(content))
//! ```
//!
//! so a leaf is `sha256(sha256(content))`, and the root's hash changes
//! whenever anything below it does.
//!
//! On disk a node is written depth-first, in the order of its fields:
//!
//! ```text
//! hash          [u8; 32]
//! children_len  u8
//! children      children_len nodes
//! content_len   u8
//! content       content_len bytes of UTF-8
//! ```
//!
//! A file is exactly one root node. Its content is empty and its children
//! are what the editor shows. A node holds at most 255 children and 255
//! bytes of content. A node stores no parent: the editor keeps the path from
//! the root in memory.
//!
//! In the editor every node is addressed by its **pre-order index**, the
//! tree's line number: node 0 is the root's first child, and the count runs
//! depth-first through the whole tree. It rides in each element's `<src=N>`,
//! exactly as a line number does for a text file.

use sha2::{Digest, Sha256};
use sicompass_sdk::ffon::{FfonElement, FfonObject};
use sicompass_sdk::tags;

/// The most children, and the most bytes of content, a node holds (`u8`).
pub const MAX_LEN: usize = u8::MAX as usize;

/// How deep `decode` nests before it refuses, so a crafted file cannot
/// overflow the stack.
pub const MAX_DEPTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfError {
    /// The bytes end in the middle of a node.
    Truncated,
    /// There are bytes after the root node.
    TrailingBytes,
    /// A node's content is not UTF-8.
    InvalidUtf8,
    /// Nodes nest deeper than [`MAX_DEPTH`].
    TooDeep,
    /// No node has that index or path (any more).
    NoSuchNode,
    /// A node would hold more than [`MAX_LEN`] children.
    TooManyChildren,
    /// A node's content would be longer than [`MAX_LEN`] bytes.
    ContentTooLong,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub hash: [u8; 32],
    pub children: Vec<Node>,
    pub content: String,
}

impl Node {
    /// A childless node, hashed.
    pub fn new(content: impl Into<String>) -> Self {
        let mut node = Node {
            hash: [0; 32],
            children: Vec::new(),
            content: content.into(),
        };
        node.hash = node.computed_hash();
        node
    }

    /// The hash this node should have, from its children's stored hashes.
    fn computed_hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        for child in &self.children {
            h.update(child.hash);
        }
        h.update(Sha256::digest(self.content.as_bytes()));
        h.finalize().into()
    }

    /// Recompute every hash in the subtree, bottom-up.
    pub fn rehash(&mut self) {
        for child in &mut self.children {
            child.rehash();
        }
        self.hash = self.computed_hash();
    }

    /// Whether every stored hash in the subtree is the one its children and
    /// content give.
    pub fn verify(&self) -> bool {
        self.children.iter().all(Node::verify) && self.hash == self.computed_hash()
    }

    /// Shown as an enterable object: it has children, or its content ends in
    /// `:` (a branch whose children are still to come).
    pub fn is_branch(&self) -> bool {
        !self.children.is_empty() || self.content.ends_with(':')
    }

    /// The number of nodes in the subtree, this one included.
    fn size(&self) -> usize {
        1 + self.children.iter().map(Node::size).sum::<usize>()
    }
}

fn check_content(text: &str) -> Result<(), FfError> {
    if text.len() > MAX_LEN {
        Err(FfError::ContentTooLong)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------

pub fn encode(root: &Node) -> Result<Vec<u8>, FfError> {
    let mut out = Vec::new();
    write_node(root, &mut out)?;
    Ok(out)
}

fn write_node(node: &Node, out: &mut Vec<u8>) -> Result<(), FfError> {
    if node.children.len() > MAX_LEN {
        return Err(FfError::TooManyChildren);
    }
    check_content(&node.content)?;
    out.extend_from_slice(&node.hash);
    out.push(node.children.len() as u8);
    for child in &node.children {
        write_node(child, out)?;
    }
    out.push(node.content.len() as u8);
    out.extend_from_slice(node.content.as_bytes());
    Ok(())
}

/// The root node in `bytes`. An empty file is an empty root, so a `.ff` just
/// created in a folder opens.
pub fn decode(bytes: &[u8]) -> Result<Node, FfError> {
    if bytes.is_empty() {
        return Ok(Node::new(""));
    }
    let mut pos = 0;
    let root = read_node(bytes, &mut pos, 0)?;
    if pos != bytes.len() {
        return Err(FfError::TrailingBytes);
    }
    Ok(root)
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], FfError> {
    let end = pos.checked_add(n).ok_or(FfError::Truncated)?;
    let slice = bytes.get(*pos..end).ok_or(FfError::Truncated)?;
    *pos = end;
    Ok(slice)
}

fn read_node(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<Node, FfError> {
    if depth >= MAX_DEPTH {
        return Err(FfError::TooDeep);
    }
    let hash: [u8; 32] = take(bytes, pos, 32)?.try_into().unwrap();
    let count = take(bytes, pos, 1)?[0];
    let children = (0..count)
        .map(|_| read_node(bytes, pos, depth + 1))
        .collect::<Result<Vec<_>, _>>()?;
    let len = take(bytes, pos, 1)?[0] as usize;
    let content = std::str::from_utf8(take(bytes, pos, len)?)
        .map_err(|_| FfError::InvalidUtf8)?
        .to_owned();
    Ok(Node {
        hash,
        children,
        content,
    })
}

// ---------------------------------------------------------------------------
// The open document
// ---------------------------------------------------------------------------

/// An open `.ff` file. A *path* is a list of child positions from the root
/// (`[]` is the root itself). Every change rehashes the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfDoc {
    pub root: Node,
}

impl FfDoc {
    pub fn new(root: Node) -> Self {
        FfDoc { root }
    }

    pub fn node_at(&self, path: &[usize]) -> Option<&Node> {
        path.iter()
            .try_fold(&self.root, |node, &i| node.children.get(i))
    }

    fn node_at_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        path.iter()
            .try_fold(&mut self.root, |node, &i| node.children.get_mut(i))
    }

    /// The pre-order index of the node at `path` (not the root).
    pub fn preorder_of(&self, path: &[usize]) -> Option<usize> {
        let (&last, ancestors) = path.split_last()?;
        let mut n = 0;
        let mut node = &self.root;
        for &i in ancestors {
            n += node
                .children
                .get(..i)?
                .iter()
                .map(Node::size)
                .sum::<usize>()
                + 1;
            node = node.children.get(i)?;
        }
        n += node
            .children
            .get(..last)?
            .iter()
            .map(Node::size)
            .sum::<usize>();
        node.children.get(last).map(|_| n)
    }

    /// The path of the node with pre-order index `n`.
    pub fn path_of(&self, mut n: usize) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        let mut node = &self.root;
        'level: loop {
            for (i, child) in node.children.iter().enumerate() {
                let size = child.size();
                if n < size {
                    path.push(i);
                    if n == 0 {
                        return Some(path);
                    }
                    n -= 1;
                    node = child;
                    continue 'level;
                }
                n -= size;
            }
            return None;
        }
    }

    /// The elements one level shows: the children of the node at `path`, each
    /// `<input><src=N>content</input>`, with their subtrees.
    pub fn to_ffon(&self, path: &[usize]) -> Vec<FfonElement> {
        let Some(parent) = self.node_at(path) else {
            return Vec::new();
        };
        let mut n = if path.is_empty() {
            0
        } else {
            match self.preorder_of(path) {
                Some(p) => p + 1,
                None => return Vec::new(),
            }
        };
        let mut out = Vec::with_capacity(parent.children.len());
        for child in &parent.children {
            out.push(element(child, n));
            n += child.size();
        }
        out
    }

    /// The hash of the node at `path` (`[]` is the root, the file's Merkle
    /// root), as lowercase hex.
    pub fn hash_hex(&self, path: &[usize]) -> Option<String> {
        let node = self.node_at(path)?;
        Some(node.hash.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Set node `n`'s content. `Ok(false)` when it already had it.
    pub fn replace(&mut self, n: usize, text: &str) -> Result<bool, FfError> {
        check_content(text)?;
        let path = self.path_of(n).ok_or(FfError::NoSuchNode)?;
        let node = self.node_at_mut(&path).ok_or(FfError::NoSuchNode)?;
        if node.content == text {
            return Ok(false);
        }
        node.content = text.to_owned();
        self.root.rehash();
        Ok(true)
    }

    /// Put `node` at `pos` among the children of the node at `parent`.
    pub fn insert(&mut self, parent: &[usize], pos: usize, node: Node) -> Result<(), FfError> {
        check_content(&node.content)?;
        let at = self.node_at_mut(parent).ok_or(FfError::NoSuchNode)?;
        if at.children.len() >= MAX_LEN {
            return Err(FfError::TooManyChildren);
        }
        let pos = pos.min(at.children.len());
        at.children.insert(pos, node);
        self.root.rehash();
        Ok(())
    }

    /// Take the child at `pos` of the node at `parent`, with its subtree.
    pub fn remove(&mut self, parent: &[usize], pos: usize) -> Option<Node> {
        let at = self.node_at_mut(parent)?;
        if pos >= at.children.len() {
            return None;
        }
        let node = at.children.remove(pos);
        self.root.rehash();
        Some(node)
    }

    /// Take node `n` with its subtree: its parent's path, its position, and
    /// the node.
    pub fn delete(&mut self, n: usize) -> Option<(Vec<usize>, usize, Node)> {
        let mut parent = self.path_of(n)?;
        let pos = parent.pop()?;
        let node = self.remove(&parent, pos)?;
        Some((parent, pos, node))
    }

    /// Where the app's `<srcins=M>` lands in the level at `level`, as a
    /// position among that level's children. The app asks for M = N before
    /// node N (Ctrl+I) and M = N + 1 after it (Ctrl+A):
    ///
    /// - node M is a child of this level: before it;
    /// - otherwise, after the child whose index is M − 1. That is Ctrl+A on
    ///   a node with children (M is its first child) or on the last child.
    pub fn srcins_position(&self, level: &[usize], m: usize) -> Option<usize> {
        let parent = self.node_at(level)?;
        let mut n = if level.is_empty() {
            0
        } else {
            self.preorder_of(level)? + 1
        };
        let mut after = None;
        for (i, child) in parent.children.iter().enumerate() {
            if n == m {
                return Some(i);
            }
            if n + 1 == m {
                after = Some(i + 1);
            }
            n += child.size();
        }
        after
    }
}

/// `node`, numbered `n`, as the element the app shows.
fn element(node: &Node, n: usize) -> FfonElement {
    let key = tags::format_input(&format!("{}{}", tags::format_src(n), node.content));
    if !node.is_branch() {
        return FfonElement::Str(key);
    }
    let mut obj = FfonObject::new(key);
    let mut m = n + 1;
    for child in &node.children {
        obj.push(element(child, m));
        m += child.size();
    }
    FfonElement::Obj(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn branch(content: &str, children: Vec<Node>) -> Node {
        let mut node = Node::new(content);
        node.children = children;
        node.rehash();
        node
    }

    /// The root over a tree with one branch:
    ///
    /// ```text
    /// 0 a
    /// 1 b:
    /// 2   c
    /// 3   d
    /// 4 e
    /// ```
    fn sample() -> FfDoc {
        FfDoc::new(branch(
            "",
            vec![
                Node::new("a"),
                branch("b:", vec![Node::new("c"), Node::new("d")]),
                Node::new("e"),
            ],
        ))
    }

    fn contents(doc: &FfDoc, path: &[usize]) -> Vec<String> {
        doc.node_at(path)
            .unwrap()
            .children
            .iter()
            .map(|c| c.content.clone())
            .collect()
    }

    #[test]
    fn a_leaf_hashes_the_hash_of_its_content() {
        assert_eq!(Node::new("a").hash, sha(&sha(b"a")));
    }

    #[test]
    fn a_branch_hashes_its_childrens_hashes_then_its_content_hash() {
        let (c, d) = (Node::new("c"), Node::new("d"));
        let mut expected = Vec::new();
        expected.extend_from_slice(&c.hash);
        expected.extend_from_slice(&d.hash);
        expected.extend_from_slice(&sha(b"b:"));
        let b = branch("b:", vec![c, d]);
        assert_eq!(b.hash, sha(&expected));
    }

    #[test]
    fn a_change_below_changes_the_root_hash() {
        let mut doc = sample();
        let before = doc.root.hash;
        assert_eq!(doc.replace(3, "D"), Ok(true));
        assert_ne!(doc.root.hash, before);
        assert!(doc.root.verify());
    }

    #[test]
    fn bytes_follow_the_fields_in_order() {
        let root = branch("", vec![Node::new("hi")]);
        let child = &root.children[0];
        let mut expected = Vec::new();
        expected.extend_from_slice(&root.hash);
        expected.push(1);
        expected.extend_from_slice(&child.hash);
        expected.push(0);
        expected.push(2);
        expected.extend_from_slice(b"hi");
        expected.push(0);
        assert_eq!(encode(&root).unwrap(), expected);
    }

    #[test]
    fn encode_then_decode_gives_the_same_tree() {
        let doc = sample();
        let bytes = encode(&doc.root).unwrap();
        assert_eq!(decode(&bytes).unwrap(), doc.root);
    }

    #[test]
    fn an_empty_file_is_an_empty_root() {
        let root = decode(&[]).unwrap();
        assert!(root.children.is_empty());
        assert_eq!(root.content, "");
        assert!(root.verify());
    }

    #[test]
    fn decode_refuses_damaged_bytes() {
        let bytes = encode(&sample().root).unwrap();
        assert_eq!(decode(&bytes[..bytes.len() - 1]), Err(FfError::Truncated));
        assert_eq!(decode(&bytes[..10]), Err(FfError::Truncated));
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode(&longer), Err(FfError::TrailingBytes));

        let mut bad = encode(&Node::new("x")).unwrap();
        *bad.last_mut().unwrap() = 0xff;
        assert_eq!(decode(&bad), Err(FfError::InvalidUtf8));
    }

    #[test]
    fn decode_refuses_nesting_past_the_depth_cap() {
        // MAX_DEPTH + 1 nodes, each the only child of the one before.
        let mut bytes = Vec::new();
        for _ in 0..=MAX_DEPTH {
            bytes.extend_from_slice(&[0; 32]);
            bytes.push(1);
        }
        assert_eq!(decode(&bytes), Err(FfError::TooDeep));
    }

    #[test]
    fn a_flipped_hash_byte_fails_verification() {
        let mut bytes = encode(&sample().root).unwrap();
        bytes[40] ^= 1; // inside the first child's hash
        let root = decode(&bytes).unwrap();
        assert!(!root.verify());
    }

    #[test]
    fn content_and_children_are_limited_to_255() {
        let mut doc = sample();
        assert_eq!(doc.replace(0, &"x".repeat(255)), Ok(true));
        assert_eq!(
            doc.replace(0, &"x".repeat(256)),
            Err(FfError::ContentTooLong)
        );
        assert_eq!(
            doc.insert(&[], 0, Node::new("é".repeat(128))),
            Err(FfError::ContentTooLong),
            "the limit counts bytes"
        );

        let mut full = FfDoc::new(Node::new(""));
        for i in 0..255 {
            full.insert(&[], i, Node::new("n")).unwrap();
        }
        assert_eq!(
            full.insert(&[], 0, Node::new("one too many")),
            Err(FfError::TooManyChildren)
        );
        assert!(encode(&full.root).is_ok());

        let mut over = Node::new("");
        over.children = vec![Node::new("n"); 256];
        assert_eq!(encode(&over), Err(FfError::TooManyChildren));
        assert_eq!(
            encode(&Node::new("x".repeat(256))),
            Err(FfError::ContentTooLong)
        );
    }

    #[test]
    fn preorder_indices_and_paths_agree() {
        let doc = sample();
        let paths = [&[0][..], &[1], &[1, 0], &[1, 1], &[2]];
        for (n, path) in paths.iter().enumerate() {
            assert_eq!(doc.preorder_of(path), Some(n), "{path:?}");
            assert_eq!(doc.path_of(n).as_deref(), Some(*path), "{n}");
        }
        assert_eq!(doc.path_of(5), None);
        assert_eq!(doc.preorder_of(&[3]), None);
        assert_eq!(doc.preorder_of(&[]), None);
    }

    #[test]
    fn to_ffon_numbers_every_element_in_preorder() {
        let doc = sample();
        let top = doc.to_ffon(&[]);
        assert_eq!(top[0], FfonElement::new_str("<input><src=0>a</input>"));
        let b = top[1].as_obj().expect("b: is a branch");
        assert_eq!(b.key, "<input><src=1>b:</input>");
        assert_eq!(
            b.children[1],
            FfonElement::new_str("<input><src=3>d</input>")
        );
        assert_eq!(top[2], FfonElement::new_str("<input><src=4>e</input>"));

        let inside = doc.to_ffon(&[1]);
        assert_eq!(inside[0], FfonElement::new_str("<input><src=2>c</input>"));
    }

    #[test]
    fn a_trailing_colon_makes_an_empty_branch() {
        let doc = FfDoc::new(branch("", vec![Node::new("todo:"), Node::new("done")]));
        let top = doc.to_ffon(&[]);
        assert!(top[0].as_obj().is_some_and(|o| o.children.is_empty()));
        assert!(top[1].as_str().is_some());
    }

    #[test]
    fn srcins_lands_before_or_after_at_the_current_level() {
        let doc = sample();
        // Ctrl+I on a (0), b (1), e (4): before each.
        assert_eq!(doc.srcins_position(&[], 0), Some(0));
        assert_eq!(doc.srcins_position(&[], 1), Some(1));
        assert_eq!(doc.srcins_position(&[], 4), Some(2));
        // Ctrl+A on a leaf: after it.
        assert_eq!(doc.srcins_position(&[], 1), Some(1));
        // Ctrl+A on b, which has children: 2 is c, inside b, so after b.
        assert_eq!(doc.srcins_position(&[], 2), Some(2));
        // Ctrl+A on the last child.
        assert_eq!(doc.srcins_position(&[], 5), Some(3));
        // Inside b: before c, after c, after d (the last).
        assert_eq!(doc.srcins_position(&[1], 2), Some(0));
        assert_eq!(doc.srcins_position(&[1], 3), Some(1));
        assert_eq!(doc.srcins_position(&[1], 4), Some(2));
        assert_eq!(doc.srcins_position(&[1], 9), None);
    }

    #[test]
    fn delete_takes_the_subtree_and_insert_puts_it_back() {
        let mut doc = sample();
        let original = doc.clone();
        let (parent, pos, node) = doc.delete(1).unwrap();
        assert_eq!((parent.as_slice(), pos), (&[][..], 1));
        assert_eq!(node.children.len(), 2);
        assert_eq!(contents(&doc, &[]), ["a", "e"]);
        assert_eq!(doc.path_of(1).as_deref(), Some(&[1][..]), "e is node 1 now");

        doc.insert(&parent, pos, node).unwrap();
        assert_eq!(doc, original);
    }
}
