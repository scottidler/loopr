//! Loop tree for hierarchical display.
//!
//! This module provides the `LoopTree` structure for displaying loops
//! in a hierarchical tree view in the Loops tab.

use crate::store::{LoopRecord, LoopType};
use std::collections::HashMap;

/// Hierarchical representation of loops for tree view.
#[derive(Debug, Default)]
pub struct LoopTree {
    /// All nodes indexed by ID
    nodes: HashMap<String, TreeNode>,
    /// IDs of root nodes (no parent)
    root_ids: Vec<String>,
    /// Flattened visible IDs for rendering (respects expand/collapse)
    visible_ids: Vec<String>,
    /// Currently selected node ID
    selected_id: Option<String>,
}

#[allow(dead_code)]
impl LoopTree {
    /// Create an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the tree from a list of loop records.
    pub fn build_from_records(&mut self, records: Vec<LoopRecord>) {
        self.nodes.clear();
        self.root_ids.clear();

        // First pass: create all nodes
        for record in &records {
            let item = LoopItem::from_record(record);
            let node = TreeNode {
                item,
                depth: 0,
                expanded: true,
                children: Vec::new(),
            };
            self.nodes.insert(record.id.clone(), node);
        }

        // Second pass: build parent-child relationships
        for record in &records {
            if let Some(parent_id) = &record.parent_loop {
                // Add this as a child of the parent
                if let Some(parent_node) = self.nodes.get_mut(parent_id) {
                    parent_node.children.push(record.id.clone());
                }
            } else {
                // Root node
                self.root_ids.push(record.id.clone());
            }
        }

        // Calculate depths
        for root_id in &self.root_ids.clone() {
            self.calculate_depth(root_id, 0);
        }

        // Sort root IDs by creation time (newest first for plans)
        self.root_ids.sort_by(|a, b| {
            let a_created = self.nodes.get(a).map(|n| n.item.created_at).unwrap_or(0);
            let b_created = self.nodes.get(b).map(|n| n.item.created_at).unwrap_or(0);
            b_created.cmp(&a_created)
        });

        // Rebuild visible IDs
        self.rebuild_visible();

        // Select first item if nothing selected
        if self.selected_id.is_none() && !self.visible_ids.is_empty() {
            self.selected_id = Some(self.visible_ids[0].clone());
        }
    }

    fn calculate_depth(&mut self, node_id: &str, depth: usize) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.depth = depth;
            let children = node.children.clone();
            for child_id in children {
                self.calculate_depth(&child_id, depth + 1);
            }
        }
    }

    fn rebuild_visible(&mut self) {
        self.visible_ids.clear();
        for root_id in &self.root_ids.clone() {
            self.add_visible_recursive(root_id);
        }
    }

    fn add_visible_recursive(&mut self, node_id: &str) {
        self.visible_ids.push(node_id.to_string());

        if let Some(node) = self.nodes.get(node_id)
            && node.expanded
        {
            let children = node.children.clone();
            for child_id in children {
                self.add_visible_recursive(&child_id);
            }
        }
    }

    /// Get the currently selected node ID.
    pub fn selected_id(&self) -> Option<&String> {
        self.selected_id.as_ref()
    }

    /// Get the currently selected node.
    pub fn selected_node(&self) -> Option<&TreeNode> {
        self.selected_id.as_ref().and_then(|id| self.nodes.get(id))
    }

    /// Get the list of visible node IDs.
    pub fn visible_ids(&self) -> &[String] {
        &self.visible_ids
    }

    /// Get a node by ID.
    pub fn get_node(&self, id: &str) -> Option<&TreeNode> {
        self.nodes.get(id)
    }

    /// Get total number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Check if tree is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        if let Some(current_id) = &self.selected_id {
            if let Some(pos) = self.visible_ids.iter().position(|id| id == current_id)
                && pos > 0
            {
                self.selected_id = Some(self.visible_ids[pos - 1].clone());
            }
        } else if !self.visible_ids.is_empty() {
            self.selected_id = Some(self.visible_ids[0].clone());
        }
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if let Some(current_id) = &self.selected_id {
            if let Some(pos) = self.visible_ids.iter().position(|id| id == current_id)
                && pos + 1 < self.visible_ids.len()
            {
                self.selected_id = Some(self.visible_ids[pos + 1].clone());
            }
        } else if !self.visible_ids.is_empty() {
            self.selected_id = Some(self.visible_ids[0].clone());
        }
    }

    /// Select first item.
    pub fn select_first(&mut self) {
        if !self.visible_ids.is_empty() {
            self.selected_id = Some(self.visible_ids[0].clone());
        }
    }

    /// Select last item.
    pub fn select_last(&mut self) {
        if !self.visible_ids.is_empty() {
            self.selected_id = Some(self.visible_ids.last().unwrap().clone());
        }
    }

    /// Toggle expand/collapse of selected node.
    pub fn toggle_expand(&mut self) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if !node.children.is_empty() {
            node.expanded = !node.expanded;
            self.rebuild_visible();
        }
    }

    /// Collapse selected node.
    pub fn collapse(&mut self) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if node.expanded && !node.children.is_empty() {
            node.expanded = false;
            self.rebuild_visible();
        } else if let Some(parent_id) = node.item.parent_id.clone() {
            // Move to parent if already collapsed or has no children
            self.selected_id = Some(parent_id);
        }
    }

    /// Expand selected node.
    pub fn expand(&mut self) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if !node.expanded && !node.children.is_empty() {
            node.expanded = true;
            self.rebuild_visible();
        }
    }
}

/// A node in the loop tree.
#[derive(Debug, Clone)]
pub struct TreeNode {
    /// The loop item data
    pub item: LoopItem,
    /// Depth in the tree (0 = root)
    pub depth: usize,
    /// Whether children are visible
    pub expanded: bool,
    /// Child node IDs
    pub children: Vec<String>,
}

/// Loop display data extracted from LoopRecord.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoopItem {
    /// Loop ID
    pub id: String,
    /// Display name (extracted from context or generated)
    pub name: String,
    /// Loop type: plan, spec, phase, ralph
    pub loop_type: String,
    /// Current status
    pub status: String,
    /// Iteration display: "3/10"
    pub iteration: String,
    /// Parent loop ID
    pub parent_id: Option<String>,
    /// Artifact file path (if any)
    pub artifact_file: Option<String>,
    /// Artifact status
    pub artifact_status: Option<String>,
    /// Creation timestamp
    pub created_at: i64,
}

impl LoopItem {
    /// Create a LoopItem from a LoopRecord.
    pub fn from_record(record: &LoopRecord) -> Self {
        let name = Self::extract_name(record);
        let iteration = format!("{}/{}", record.iteration, record.max_iterations);

        Self {
            id: record.id.clone(),
            name,
            loop_type: record.loop_type.as_str().to_string(),
            status: record.status.as_str().to_string(),
            iteration,
            parent_id: record.parent_loop.clone(),
            artifact_file: None, // TODO: extract from context
            artifact_status: None,
            created_at: record.created_at,
        }
    }

    fn extract_name(record: &LoopRecord) -> String {
        // Try to extract a meaningful name from context
        if let Some(task) = record.context.get("task").and_then(|v| v.as_str()) {
            let truncated = if task.len() > 40 { format!("{}...", &task[..37]) } else { task.to_string() };
            return truncated;
        }

        if let Some(name) = record.context.get("phase_name").and_then(|v| v.as_str()) {
            return name.to_string();
        }

        // Default to type + ID suffix
        format!(
            "{} {}",
            Self::type_prefix(record.loop_type),
            &record.id[record.id.len().saturating_sub(6)..]
        )
    }

    fn type_prefix(loop_type: LoopType) -> &'static str {
        match loop_type {
            LoopType::Plan => "Plan",
            LoopType::Spec => "Spec",
            LoopType::Phase => "Phase",
            LoopType::Ralph => "Ralph",
        }
    }

    /// Get the status icon for display.
    pub fn status_icon(&self) -> &'static str {
        match self.status.as_str() {
            "running" => "●",
            "pending" => "○",
            "complete" => "✓",
            "failed" => "✗",
            "paused" => "◐",
            "invalidated" => "◌",
            _ => "?",
        }
    }

    /// Check if this is a draft (status = pending and iteration = 0).
    #[allow(dead_code)]
    pub fn is_draft(&self) -> bool {
        self.status == "pending" && self.iteration.starts_with("0/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LoopStatus;

    fn make_test_record(id: &str, loop_type: LoopType, parent: Option<&str>) -> LoopRecord {
        let mut record = LoopRecord::new(loop_type, serde_json::json!({"task": "Test task"}));
        record.id = id.to_string();
        record.parent_loop = parent.map(|s| s.to_string());
        record
    }

    #[test]
    fn test_loop_tree_empty() {
        let tree = LoopTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn test_loop_tree_single_node() {
        let mut tree = LoopTree::new();
        let records = vec![make_test_record("1", LoopType::Plan, None)];

        tree.build_from_records(records);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.root_ids.len(), 1);
        assert!(tree.selected_id().is_some());
    }

    #[test]
    fn test_loop_tree_hierarchy() {
        let mut tree = LoopTree::new();
        let records = vec![
            make_test_record("1", LoopType::Plan, None),
            make_test_record("2", LoopType::Spec, Some("1")),
            make_test_record("3", LoopType::Phase, Some("2")),
        ];

        tree.build_from_records(records);

        assert_eq!(tree.len(), 3);
        assert_eq!(tree.root_ids.len(), 1);

        // Check depths
        assert_eq!(tree.get_node("1").unwrap().depth, 0);
        assert_eq!(tree.get_node("2").unwrap().depth, 1);
        assert_eq!(tree.get_node("3").unwrap().depth, 2);
    }

    #[test]
    fn test_navigation() {
        let mut tree = LoopTree::new();
        let records = vec![
            make_test_record("1", LoopType::Plan, None),
            make_test_record("2", LoopType::Spec, Some("1")),
        ];

        tree.build_from_records(records);

        assert_eq!(tree.selected_id(), Some(&"1".to_string()));

        tree.select_next();
        assert_eq!(tree.selected_id(), Some(&"2".to_string()));

        tree.select_previous();
        assert_eq!(tree.selected_id(), Some(&"1".to_string()));
    }

    #[test]
    fn test_expand_collapse() {
        let mut tree = LoopTree::new();
        let records = vec![
            make_test_record("1", LoopType::Plan, None),
            make_test_record("2", LoopType::Spec, Some("1")),
        ];

        tree.build_from_records(records);

        // Initially expanded
        assert_eq!(tree.visible_ids().len(), 2);

        // Collapse
        tree.collapse();
        assert_eq!(tree.visible_ids().len(), 1);

        // Expand
        tree.expand();
        assert_eq!(tree.visible_ids().len(), 2);
    }

    #[test]
    fn test_loop_item_status_icon() {
        let mut record = make_test_record("1", LoopType::Plan, None);

        record.status = LoopStatus::Running;
        let item = LoopItem::from_record(&record);
        assert_eq!(item.status_icon(), "●");

        record.status = LoopStatus::Complete;
        let item = LoopItem::from_record(&record);
        assert_eq!(item.status_icon(), "✓");

        record.status = LoopStatus::Failed;
        let item = LoopItem::from_record(&record);
        assert_eq!(item.status_icon(), "✗");
    }

    #[test]
    fn test_loop_item_name_extraction() {
        let record = make_test_record("1", LoopType::Plan, None);
        let item = LoopItem::from_record(&record);
        assert_eq!(item.name, "Test task");
    }

    #[test]
    fn test_select_first_last() {
        let mut tree = LoopTree::new();
        let records = vec![
            make_test_record("1", LoopType::Plan, None),
            make_test_record("2", LoopType::Spec, Some("1")),
            make_test_record("3", LoopType::Phase, Some("2")),
        ];

        tree.build_from_records(records);

        tree.select_last();
        assert_eq!(tree.selected_id(), Some(&"3".to_string()));

        tree.select_first();
        assert_eq!(tree.selected_id(), Some(&"1".to_string()));
    }
}
