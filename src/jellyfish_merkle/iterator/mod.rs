// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module implements `JellyfishMerkleIterator`. Initialized with a version and a key, the
//! iterator generates all the key-value pairs in this version of the tree, starting from the
//! smallest key that is greater or equal to the given key, by performing a depth first traversal
//! on the tree.

#[cfg(test)]
mod iterator_test;

use super::hash::HashValue;
use super::{
    hash::SMTHash,
    nibble::Nibble,
    nibble_path::NibblePath,
    node_type::{InternalNode, Node, NodeKey},
    TreeReader,
};
use crate::{Key, SMTObject, Value};
use anyhow::{format_err, Result};
use std::marker::PhantomData;

/// `NodeVisitInfo` keeps track of the status of an internal node during the iteration process. It
/// indicates which ones of its children have been visited.
#[derive(Debug)]
struct NodeVisitInfo {
    /// The key to this node.
    node_key: NodeKey,

    /// The node itself.
    node: InternalNode,

    /// The bitmap indicating which children exist. It is generated by running
    /// `self.node.generate_bitmaps().0` and cached here.
    children_bitmap: u16,

    /// This integer always has exactly one 1-bit. The position of the 1-bit (from LSB) indicates
    /// the next child to visit in the iteration process. All the ones on the left have already
    /// been visited. All the children on the right (including this one) have not been visited yet.
    next_child_to_visit: u16,
}

impl NodeVisitInfo {
    /// Constructs a new `NodeVisitInfo` with given node key and node. `next_child_to_visit` will
    /// be set to the leftmost child.
    fn new(node_key: NodeKey, node: InternalNode) -> Self {
        let (children_bitmap, _) = node.generate_bitmaps();
        Self {
            node_key,
            node,
            children_bitmap,
            next_child_to_visit: 1 << children_bitmap.trailing_zeros(),
        }
    }

    /// Same as `new` but points `next_child_to_visit` to a specific location. If the child
    /// corresponding to `next_child_to_visit` does not exist, set it to the next one on the
    /// right.
    fn new_next_child_to_visit(
        node_key: NodeKey,
        node: InternalNode,
        next_child_to_visit: Nibble,
    ) -> Self {
        let (children_bitmap, _) = node.generate_bitmaps();
        let mut next_child_to_visit = 1 << u8::from(next_child_to_visit);
        while next_child_to_visit & children_bitmap == 0 {
            next_child_to_visit <<= 1;
        }
        Self {
            node_key,
            node,
            children_bitmap,
            next_child_to_visit,
        }
    }

    /// Whether the next child to visit is the rightmost one.
    fn is_rightmost(&self) -> bool {
        assert!(self.next_child_to_visit.leading_zeros() >= self.children_bitmap.leading_zeros());
        self.next_child_to_visit.leading_zeros() == self.children_bitmap.leading_zeros()
    }

    /// Advances `next_child_to_visit` to the next child on the right.
    fn advance(&mut self) {
        assert!(!self.is_rightmost(), "Advancing past rightmost child.");
        self.next_child_to_visit <<= 1;
        while self.next_child_to_visit & self.children_bitmap == 0 {
            self.next_child_to_visit <<= 1;
        }
    }
}

/// The `JellyfishMerkleIterator` implementation.
pub struct JellyfishMerkleIterator<'a, K, V, R: 'a + TreeReader<K, V>> {
    /// The storage engine from which we can read nodes using node keys.
    reader: &'a R,

    /// The root hash of the tree this iterator is running on.
    state_root_hash: HashValue,

    /// The stack used for depth first traversal.
    parent_stack: Vec<NodeVisitInfo>,

    /// Whether the iteration has finished. Usually this can be determined by checking whether
    /// `self.parent_stack` is empty. But in case of a tree with a single leaf, we need this
    /// additional bit.
    done: bool,

    key: PhantomData<K>,
    value: PhantomData<V>,
}

impl<'a, K, V, R> JellyfishMerkleIterator<'a, K, V, R>
where
    R: 'a + TreeReader<K, V>,
    K: Key,
    V: Value,
{
    /// Constructs a new iterator. This puts the internal state in the correct position, so the
    /// following `next` call will yield the smallest key that is greater or equal to
    /// `starting_key`.
    pub fn new(
        reader: &'a R,
        state_root_hash: HashValue,
        starting_key: SMTObject<K>,
    ) -> Result<Self> {
        let mut parent_stack = vec![];
        let mut done = false;

        let mut current_node_key = state_root_hash;
        let starting_key_hash = starting_key.merkle_hash();
        let nibble_path = NibblePath::new(starting_key_hash.to_vec());
        let mut nibble_iter = nibble_path.nibbles();

        while let Node::Internal(internal_node) = reader.get_node(&current_node_key)? {
            let child_index = nibble_iter.next().expect("Should have enough nibbles.");
            match internal_node.child(child_index) {
                Some(child) => {
                    // If this child exists, we just push the node onto stack and repeat.
                    parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                        current_node_key,
                        internal_node.clone(),
                        child_index,
                    ));
                    current_node_key = child.hash;
                    // current_node_key.gen_child_node_key(child.version, child_index);
                }
                None => {
                    let (bitmap, _) = internal_node.generate_bitmaps();
                    if u32::from(u8::from(child_index)) < 15 - bitmap.leading_zeros() {
                        // If this child does not exist and there's another child on the right, we
                        // set the child on the right to be the next one to visit.
                        parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                            current_node_key,
                            internal_node,
                            child_index,
                        ));
                    } else {
                        // Otherwise we have done visiting this node. Go backward and clean up the
                        // stack.
                        Self::cleanup_stack(&mut parent_stack);
                    }
                    return Ok(Self {
                        reader,
                        state_root_hash,
                        parent_stack,
                        done,
                        key: PhantomData,
                        value: PhantomData,
                    });
                }
            }
        }

        match reader.get_node(&current_node_key)? {
            Node::Internal(_) => unreachable!("Should have reached the bottom of the tree."),
            Node::Leaf(leaf_node) => {
                if leaf_node.key().merkle_hash() < starting_key_hash {
                    Self::cleanup_stack(&mut parent_stack);
                    if parent_stack.is_empty() {
                        done = true;
                    }
                }
            }
            Node::Null => done = true,
        }

        Ok(Self {
            reader,
            state_root_hash,
            parent_stack,
            done,
            key: PhantomData,
            value: PhantomData,
        })
    }

    fn cleanup_stack(parent_stack: &mut Vec<NodeVisitInfo>) {
        while let Some(info) = parent_stack.last_mut() {
            if info.is_rightmost() {
                parent_stack.pop();
            } else {
                info.advance();
                break;
            }
        }
    }

    #[cfg(test)]
    pub fn print(&self) -> Result<()> {
        let nodes = &self.parent_stack;
        for node in nodes {
            println!("internal node key: {:?}", node.node_key.to_hex());
            if let Ok(Node::Internal(internal)) = self.reader.get_node(&node.node_key) {
                println!("child: {:?}", internal.all_child());
            }
        }
        Ok(())
    }
}

impl<'a, K, V, R> Iterator for JellyfishMerkleIterator<'a, K, V, R>
where
    R: 'a + TreeReader<K, V>,
    K: Key,
    V: Value,
{
    type Item = Result<(SMTObject<K>, SMTObject<V>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        if self.parent_stack.is_empty() {
            let root_node_key = self.state_root_hash;
            match self.reader.get_node(&root_node_key) {
                Ok(Node::Leaf(leaf_node)) => {
                    // This means the entire tree has a single leaf node. The key of this leaf node
                    // is greater or equal to `starting_key` (otherwise we would have set `done` to
                    // true in `new`). Return the node and mark `self.done` so next time we return
                    // None.
                    self.done = true;
                    return Some(Ok((leaf_node.key().clone(), leaf_node.value().clone())));
                }
                Ok(Node::Internal(_)) => {
                    // This means `starting_key` is bigger than every key in this tree, or we have
                    // iterated past the last key.
                    return None;
                }
                Ok(Node::Null) => unreachable!("We would have set done to true in new."),
                Err(err) => return Some(Err(err)),
            }
        }

        loop {
            let last_visited_node_info = self
                .parent_stack
                .last()
                .expect("We have checked that self.parent_stack is not empty.");
            let child_index =
                Nibble::from(last_visited_node_info.next_child_to_visit.trailing_zeros() as u8);
            let node_key = last_visited_node_info
                .node
                .child(child_index)
                .expect("Child should exist.")
                .hash;

            match self.reader.get_node(&node_key) {
                Ok(Node::Internal(internal_node)) => {
                    let visit_info = NodeVisitInfo::new(node_key, internal_node);
                    self.parent_stack.push(visit_info);
                }
                Ok(Node::Leaf(leaf_node)) => {
                    let ret = (leaf_node.key().clone(), leaf_node.value().clone());
                    Self::cleanup_stack(&mut self.parent_stack);
                    return Some(Ok(ret));
                }
                Ok(Node::Null) => return Some(Err(format_err!("Should not reach a null node."))),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

/// The `JellyfishMerkleIntoIterator` implementation.
pub struct JellyfishMerkleIntoIterator<K, V, R: TreeReader<K, V>> {
    /// The storage engine from which we can read nodes using node keys.
    reader: R,

    /// The root hash of the tree this iterator is running on.
    state_root_hash: HashValue,

    /// The stack used for depth first traversal.
    parent_stack: Vec<NodeVisitInfo>,

    /// Whether the iteration has finished. Usually this can be determined by checking whether
    /// `self.parent_stack` is empty. But in case of a tree with a single leaf, we need this
    /// additional bit.
    done: bool,

    key: PhantomData<K>,
    value: PhantomData<V>,
}

impl<K, V, R> JellyfishMerkleIntoIterator<K, V, R>
where
    R: TreeReader<K, V>,
    K: Key,
    V: Value,
{
    /// Constructs a new iterator. This puts the internal state in the correct position, so the
    /// following `next` call will yield the smallest key that is greater or equal to
    /// `starting_key`.
    pub fn new(reader: R, state_root_hash: HashValue, starting_key: HashValue) -> Result<Self> {
        let mut parent_stack = vec![];
        let mut done = false;

        let mut current_node_key = state_root_hash;
        let nibble_path = NibblePath::new(starting_key.to_vec());
        let mut nibble_iter = nibble_path.nibbles();

        while let Node::Internal(internal_node) = reader.get_node(&current_node_key)? {
            let child_index = nibble_iter.next().expect("Should have enough nibbles.");
            match internal_node.child(child_index) {
                Some(child) => {
                    // If this child exists, we just push the node onto stack and repeat.
                    parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                        current_node_key,
                        internal_node.clone(),
                        child_index,
                    ));
                    current_node_key = child.hash;
                    // current_node_key.gen_child_node_key(child.version, child_index);
                }
                None => {
                    let (bitmap, _) = internal_node.generate_bitmaps();
                    if u32::from(u8::from(child_index)) < 15 - bitmap.leading_zeros() {
                        // If this child does not exist and there's another child on the right, we
                        // set the child on the right to be the next one to visit.
                        parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                            current_node_key,
                            internal_node,
                            child_index,
                        ));
                    } else {
                        // Otherwise we have done visiting this node. Go backward and clean up the
                        // stack.
                        Self::cleanup_stack(&mut parent_stack);
                    }
                    return Ok(Self {
                        reader,
                        state_root_hash,
                        parent_stack,
                        done,
                        key: PhantomData,
                        value: PhantomData,
                    });
                }
            }
        }

        match reader.get_node(&current_node_key)? {
            Node::Internal(_) => unreachable!("Should have reached the bottom of the tree."),
            Node::Leaf(leaf_node) => {
                if leaf_node.key().merkle_hash() < starting_key {
                    Self::cleanup_stack(&mut parent_stack);
                    if parent_stack.is_empty() {
                        done = true;
                    }
                }
            }
            Node::Null => done = true,
        }

        Ok(Self {
            reader,
            state_root_hash,
            parent_stack,
            done,
            key: PhantomData,
            value: PhantomData,
        })
    }

    fn cleanup_stack(parent_stack: &mut Vec<NodeVisitInfo>) {
        while let Some(info) = parent_stack.last_mut() {
            if info.is_rightmost() {
                parent_stack.pop();
            } else {
                info.advance();
                break;
            }
        }
    }

    #[cfg(test)]
    pub fn print(&self) -> Result<()> {
        let nodes = &self.parent_stack;
        for node in nodes {
            println!("internal node key: {:?}", node.node_key.to_hex());
            if let Ok(Node::Internal(internal)) = self.reader.get_node(&node.node_key) {
                println!("child: {:?}", internal.all_child());
            }
        }
        Ok(())
    }
}

impl<K, V, R> Iterator for JellyfishMerkleIntoIterator<K, V, R>
where
    R: TreeReader<K, V>,
    K: Key,
    V: Value,
{
    type Item = Result<(SMTObject<K>, SMTObject<V>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        if self.parent_stack.is_empty() {
            let root_node_key = self.state_root_hash;
            match self.reader.get_node(&root_node_key) {
                Ok(Node::Leaf(leaf_node)) => {
                    // This means the entire tree has a single leaf node. The key of this leaf node
                    // is greater or equal to `starting_key` (otherwise we would have set `done` to
                    // true in `new`). Return the node and mark `self.done` so next time we return
                    // None.
                    self.done = true;
                    return Some(Ok((leaf_node.key().clone(), leaf_node.value().clone())));
                }
                Ok(Node::Internal(_)) => {
                    // This means `starting_key` is bigger than every key in this tree, or we have
                    // iterated past the last key.
                    return None;
                }
                Ok(Node::Null) => unreachable!("We would have set done to true in new."),
                Err(err) => return Some(Err(err)),
            }
        }

        loop {
            let last_visited_node_info = self
                .parent_stack
                .last()
                .expect("We have checked that self.parent_stack is not empty.");
            let child_index =
                Nibble::from(last_visited_node_info.next_child_to_visit.trailing_zeros() as u8);
            let node_key = last_visited_node_info
                .node
                .child(child_index)
                .expect("Child should exist.")
                .hash;

            match self.reader.get_node(&node_key) {
                Ok(Node::Internal(internal_node)) => {
                    let visit_info = NodeVisitInfo::new(node_key, internal_node);
                    self.parent_stack.push(visit_info);
                }
                Ok(Node::Leaf(leaf_node)) => {
                    let ret = (leaf_node.key().clone(), leaf_node.value().clone());
                    Self::cleanup_stack(&mut self.parent_stack);
                    return Some(Ok(ret));
                }
                Ok(Node::Null) => return Some(Err(format_err!("Should not reach a null node."))),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}
