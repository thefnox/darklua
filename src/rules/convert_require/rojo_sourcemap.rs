use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{utils, DarkluaError};

use super::InstancePath;

type NodeId = usize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RojoSourcemapNode {
    name: String,
    class_name: String,
    #[serde(default)]
    file_paths: Vec<PathBuf>,
    #[serde(default)]
    children: Vec<RojoSourcemapNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedNode {
    name: String,
    parent_id: NodeId,
}

/// A Rojo sourcemap flattened into a list of nodes indexed by id, where the root
/// node has id 0 and is its own parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RojoSourcemap {
    nodes: Vec<IndexedNode>,
    node_ids_by_path: HashMap<PathBuf, NodeId>,
    is_datamodel: bool,
}

impl RojoSourcemap {
    pub(crate) fn parse(
        content: &str,
        relative_to: impl AsRef<Path>,
    ) -> Result<Self, DarkluaError> {
        let root_node = serde_json::from_str::<RojoSourcemapNode>(content)?;
        let relative_to = relative_to.as_ref();
        let is_datamodel = root_node.class_name == "DataModel";

        let mut nodes = Vec::new();
        let mut node_ids_by_path = HashMap::new();
        let mut queue = vec![(root_node, 0)];

        while let Some((node, parent_id)) = queue.pop() {
            let id = nodes.len();
            for file_path in node.file_paths {
                // when a file path appears on multiple nodes, the first node visited wins
                node_ids_by_path
                    .entry(utils::normalize_path(relative_to.join(file_path)))
                    .or_insert(id);
            }
            nodes.push(IndexedNode {
                name: node.name,
                parent_id,
            });
            queue.extend(node.children.into_iter().map(|child| (child, id)));
        }

        Ok(Self {
            nodes,
            node_ids_by_path,
            is_datamodel,
        })
    }

    pub(crate) fn exists(&self, path: &Path) -> bool {
        self.node_ids_by_path.contains_key(path)
    }

    pub(crate) fn get_instance_path(
        &self,
        from_file: impl AsRef<Path>,
        target_file: impl AsRef<Path>,
    ) -> Option<InstancePath> {
        let from_file = from_file.as_ref();
        let target_file = target_file.as_ref();

        let from_node = *self.node_ids_by_path.get(from_file)?;
        let target_node = *self.node_ids_by_path.get(target_file)?;

        let from_ancestors = self.hierarchy(from_node);
        let target_ancestors = self.hierarchy(target_node);

        let (parents, descendants) = from_ancestors
            .iter()
            .enumerate()
            .find_map(|(index, ancestor_id)| {
                target_ancestors
                    .iter()
                    .position(|id| id == ancestor_id)
                    .map(|target_index| (index, target_index))
            })
            .map(|(from_ancestor_split, target_ancestor_split)| {
                (
                    from_ancestors.split_at(from_ancestor_split).0,
                    target_ancestors.split_at(target_ancestor_split).0,
                )
            })?;

        let relative_path_length = parents.len().saturating_add(descendants.len());

        if !self.is_datamodel || relative_path_length <= target_ancestors.len() {
            log::trace!("  ⨽ use Roblox path from script instance");

            let mut instance_path = InstancePath::from_script();

            for _ in 0..parents.len() {
                instance_path.parent();
            }

            Some(self.index_descendants(instance_path, descendants.iter().rev()))
        } else {
            log::trace!("  ⨽ use Roblox path from DataModel instance");

            Some(self.index_descendants(
                InstancePath::from_root(),
                target_ancestors.iter().rev().skip(1),
            ))
        }
    }

    fn index_descendants<'a>(
        &self,
        mut instance_path: InstancePath,
        descendants: impl Iterator<Item = &'a NodeId>,
    ) -> InstancePath {
        for descendant_id in descendants {
            instance_path.child(&self.nodes[*descendant_id].name);
        }
        instance_path
    }

    /// returns the ids of each ancestor of the given node and itself
    fn hierarchy(&self, node_id: NodeId) -> Vec<NodeId> {
        let mut ids = vec![node_id];
        let mut current_id = node_id;

        while current_id != 0 {
            current_id = self.nodes[current_id].parent_id;
            ids.push(current_id);
        }

        ids
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn new_sourcemap(content: &str) -> RojoSourcemap {
        RojoSourcemap::parse(content, "").expect("unable to parse sourcemap")
    }

    mod instance_paths {
        use super::*;

        fn script_path(components: &[&'static str]) -> InstancePath {
            components
                .iter()
                .fold(InstancePath::from_script(), |mut path, component| {
                    match *component {
                        "parent" => {
                            path.parent();
                        }
                        child_name => {
                            path.child(child_name);
                        }
                    }
                    path
                })
        }

        #[test]
        fn from_init_to_sibling_module() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "value",
                        "className": "ModuleScript",
                        "filePaths": ["src/value.lua"]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/init.lua", "src/value.lua")
                    .unwrap(),
                script_path(&["value"])
            );
        }

        #[test]
        fn from_sibling_to_sibling_module() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "main",
                        "className": "ModuleScript",
                        "filePaths": ["src/main.lua"]
                    },
                    {
                        "name": "value",
                        "className": "ModuleScript",
                        "filePaths": ["src/value.lua"]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/main.lua", "src/value.lua")
                    .unwrap(),
                script_path(&["parent", "value"])
            );
        }

        #[test]
        fn from_sibling_to_nested_sibling_module() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "main",
                        "className": "ModuleScript",
                        "filePaths": ["src/main.lua"]
                    },
                    {
                        "name": "Lib",
                        "className": "Folder",
                        "children": [
                            {
                                "name": "format",
                                "className": "ModuleScript",
                                "filePaths": ["src/Lib/format.lua"]
                            }
                        ]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/main.lua", "src/Lib/format.lua")
                    .unwrap(),
                script_path(&["parent", "Lib", "format"])
            );
        }

        #[test]
        fn from_child_require_parent() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "main",
                        "className": "ModuleScript",
                        "filePaths": ["src/main.lua"]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/main.lua", "src/init.lua")
                    .unwrap(),
                script_path(&["parent"])
            );
        }

        #[test]
        fn from_child_require_parent_nested() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "Sub",
                        "className": "ModuleScript",
                        "filePaths": ["src/Sub/init.lua"],
                        "children": [
                            {
                                "name": "test",
                                "className": "ModuleScript",
                                "filePaths": ["src/Sub/test.lua"]
                            }
                        ]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/Sub/test.lua", "src/Sub/init.lua")
                    .unwrap(),
                script_path(&["parent"])
            );
        }
    }
}
